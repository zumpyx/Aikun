//! SurrealDB 存储装配(阶段 S 默认切换):连接、schema 初始化、三域能力
//! 句柄与首启播种。SQLite 后端已收口进迁移工具 crate(aikun-migrate),
//! 本二进制不再含 SQLite 直连依赖(docs/surreal-migration.md §二/§六)。

use std::sync::Arc;

use aikun_store::crypto::KeyCipher;
use aikun_store::surreal::SurrealHandle;
use aikun_store::surreal::billing_store::{BillingStore, SurrealBilling};
use aikun_store::surreal::channels_store::{ChannelStore, SurrealChannels};
use aikun_store::surreal::identity::{IdentityStore, SurrealIdentity};

use crate::config::AppConfig;

/// 进程唯一的存储门面:三域能力(身份 / 渠道 / 计费)各持一份
/// `Arc<dyn …>`(SDK 句柄 Clone 廉价,内部共享连接),外加渠道 key
/// 解密器(selector 选中渠道后解一次,快照持有密文)。
pub struct AppStore {
    pub identity: Arc<dyn IdentityStore>,
    pub channels: Arc<dyn ChannelStore>,
    pub billing: Arc<dyn BillingStore>,
    pub cipher: KeyCipher,
    /// 本实例序号(缓存一致性比对的 owner 维;单实例部署恒定,
    /// 测试多实例时不共享命中)。
    pub id: u64,
}

static STORE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 按配置建立 SurrealDB 连接并完成启动期工作:
///
/// 1. `Endpoint::parse`(config 已在加载时校验过,这里再解析一次拿结构);
/// 2. `SurrealHandle::connect`:引擎握手 →(远程)root 登录 → 选定
///    ns/db,**任一步失败即拒绝启动**(不静默回退,配置面口径);
/// 3. `ensure_schema`:版本闸门(降级拒绝)→ 幂等 DDL → 首启写版本;
/// 4. 价格表为空时播种内置默认价;
/// 5. 无 admin 用户时播种默认 admin(随机密码打印一次,SQLite 侧
///    `seed_default_admin` 同口径);
/// 6. 当日日用量计数器重建(明细是唯一真相,重启归零对齐——SQLite
///    池启动路径同款)。
pub async fn init(config: &AppConfig) -> Result<AppStore, String> {
    let endpoint = aikun_store::surreal::Endpoint::parse(&config.surreal_url)
        .map_err(|e| format!("surreal-url 非法: {e}"))?;
    let handle = SurrealHandle::connect(&endpoint)
        .await
        .map_err(|e| format!("SurrealDB 连接失败({}): {e}", endpoint.address))?;
    handle
        .ensure_schema()
        .await
        .map_err(|e| format!("SurrealDB schema 初始化失败: {e}"))?;
    tracing::info!("SurrealDB 已连接: {}", endpoint.address);

    let identity = Arc::new(SurrealIdentity::new(handle.clone()));
    let cipher = crate::crypto::cipher_from_config(config);
    let channels = Arc::new(SurrealChannels::new(handle.clone(), cipher.clone()));
    let billing = Arc::new(SurrealBilling::new(handle));

    // 价格表为空时播种内置默认价(空表→非空之间的中断留给下次启动)。
    match billing.seed_default_prices_if_empty().await {
        Ok(n) if n > 0 => tracing::info!("已播种内置默认价格 {n} 条"),
        Ok(_) => {}
        Err(e) => tracing::warn!("默认价格播种失败(下次启动重试): {e}"),
    }

    // 首启种子 admin:无任何 admin 用户时创建(username=admin,随机密码
    // 打印一次;SQLite seed_default_admin 同款)。
    seed_default_admin(&*identity).await;

    // 日用量计数器从明细重建(幂等;重启归零对齐,refactor-plan 9.3)。
    match billing
        .rebuild_usage_counters(chrono::Utc::now().date_naive())
        .await
    {
        Ok(n) if n > 0 => tracing::debug!("已从明细重建 {n} 把 key 的当日用量计数器"),
        Ok(_) => {}
        Err(e) => tracing::warn!("当日用量计数器重建失败: {e}"),
    }

    Ok(AppStore {
        identity,
        channels,
        billing,
        cipher,
        id: STORE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    })
}

/// 库中没有 role='admin' 的用户时播种默认 admin(密码只打印一次)。
async fn seed_default_admin(identity: &dyn IdentityStore) {
    match identity.count_active_admins().await {
        Ok(0) => {}
        Ok(_) => return,
        Err(e) => {
            tracing::warn!("admin 存量检查失败,跳过默认 admin 播种: {e}");
            return;
        }
    }
    let password = crate::auth::generate_random_password();
    let hash = match crate::auth::hash_password(&password) {
        Ok(h) => h,
        Err(e) => {
            // 绝不播种带不可用占位哈希的 admin(SQLite 同口径)。
            tracing::error!("默认 admin 密码哈希失败,跳过播种: {e}");
            return;
        }
    };
    match identity
        .create_user(aikun_store::surreal::identity::NewUser {
            id: None,
            username: "admin".to_string(),
            password_hash: hash,
            display_name: "Admin".to_string(),
            role: "admin".to_string(),
        })
        .await
    {
        Ok(_) => {
            tracing::warn!("============================================================");
            tracing::warn!("Default admin created — username: admin  password: {password}");
            tracing::warn!("============================================================");
        }
        Err(e) => tracing::error!("默认 admin 播种失败: {e}"),
    }
}
