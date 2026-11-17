//! User tag(JWT,用户自助)实现:全部 12 op。业务逻辑迁自
//! `aikun/src/api/auth.rs`(me / api-keys CRUD)、`aikun/src/api/logs.rs`
//! (logs / logs/stats / wallet)与 `aikun/src/api/redemption.rs::redeem_code`,
//! 对齐 `openapi.yaml` 新契约(资源化路径、{items,total} 信封、
//! problem+json、兑换 200→201 + 429 补 `Retry-After`)。
//!
//! # 装配契约(main.rs 侧,集成工人;见 docs/migration-playbook.md)
//!
//! * **鉴权上下文经 task-local 注入**。生成 trait 方法签名(query/body/path)
//!   拿不到请求上下文(踩坑 #4);auth_middleware 验证后把二进制
//!   `crate::auth::Claims` 放进 request extensions,装配层再用一行适配
//!   中间件映射为本文件的 [`UserAuth`] 塞回 extensions,最后挂
//!   [`user_auth_scope`]:把 UserAuth 放进 task-local 再执行下游。
//!   参考装配(顺序自外向内):
//!
//!   ```ignore
//!   let user_routes = user_api_router(user_service)
//!       .layer(middleware::from_fn(aikun_admin::user::user_auth_scope))
//!       .layer(middleware::from_fn(|mut req: Request, next: Next| async move {
//!           // auth_middleware 已验证;把二进制 Claims 映射为 UserAuth
//!           if let Some(c) = req.extensions().get::<crate::auth::Claims>().cloned() {
//!               req.extensions_mut().insert(aikun_admin::user::UserAuth {
//!                   user_id: c.sub, username: c.username, role: c.role,
//!               });
//!           }
//!           next.run(req).await
//!       }))
//!       .layer(middleware::from_fn_with_state(state.clone(), auth_middleware));
//!   ```
//!
//!   合入 app 时 `.merge(user_routes.with_state(()))`(踩坑 #5)。
//! * **UserService 与 AppState 共享两个内存窗口**:`redeem_attempts`
//!   (兑换失败限流,keyed by user id)与 `api_key_rate`(删 key 时清 RPM
//!   死条目)——注入同一 `Arc` 实例(与 Auth 批 login_attempts 同模式)。
//! * **version 由装配层注入**(传二进制的 `env!("CARGO_PKG_VERSION")`;
//!   aikun-admin 自身 pkg version 不代表服务版本)。
//!
//! # 已知 spec 缺口(裁决前按折衷处理,均已记 notes 上报)
//!
//! * **User tag 全部响应枚举无 5xx 变体**(`errors.rs`):旧实现
//!   "DB 锁中毒/查询失败 → 500 internal_error / query_failed"
//!   (基线 aikun/src/api/auth.rs、logs.rs、redemption.rs)无法表达。
//!   过渡映射与 admin_users / admin_billing 批同策(api-mapping 备注 13):
//!   **非 401 变体优先**(写路径 400、删除 404,均 fail-closed +
//!   `tracing::error!` 留痕),body `code` 保留真实短码 `internal_error`
//!   ——绝不静默回空列表/假资源。**401 会误杀会话**(前端 core.js 对
//!   任何 401 清 token 踢回登录页),所以仅当枚举只有 Unauthorized
//!   变体(list_api_keys / get_wallet_stats / list_public_prices 三个)
//!   才退 401,且 body `code` 用 `internal_error` 而非 `invalid_token`
//!   (不冒充凭证问题);这三个 op 的会话误杀要等 spec 补 5xx 变体重
//!   生成后才能根治。get_current_user 例外:旧行为本就是"任何查询
//!   错误 → 404"(基线 aikun/src/api/auth.rs:54-58),按 404 保留。
//! * **success 非 0/1 的 400 短码**:spec 示例 `invalid_success`,但生成
//!   路由把枚举解码失败拦成 400 `malformed_parameter`,impl 永远见不到
//!   非法值(e2e_user.rs 按实测锚定)。
//! * **RequestLog.user_id/api_key_id/provider_id 在 spec 是 int64**,与
//!   "ID 拍板为不透明 string"(api-mapping 备注 2)冲突:现网这三列是
//!   UUID 字符串。impl 按可解析则出数值、否则显式 null 处理,待 spec 修
//!   正为 string。
//! * **/api 管理面 401/403 响应体**:由二进制 auth_middleware 产生,已
//!   实现 problem+json(`auth/mod.rs::auth_error`,RFC 9457;main.rs
//!   Admin tag 装配注释亦确认归一),与生成路由错误体一致,本文件无需
//!   再包装(避免双层信封)。/v1 代理面 401 维持旧扁平体(契约冻结)。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use aikun_openapi::openapi::server::api::{GetLogStatsSuccess, ListLogsSuccess, UserApi};
use aikun_openapi::openapi::server::errors::{
    CreateApiKeyResponse, DeleteApiKeyResponse, GetCurrentUserResponse, GetLogStatsResponse,
    GetVersionResponse, GetWalletStatsResponse, ListApiKeysResponse, ListLogsResponse,
    ListPublicPricesResponse, RedeemCodeResponse, RedeemCodeResponseTooManyRequestsHeaders,
    UpdateApiKeyResponse,
};
use aikun_openapi::openapi::types::{
    ApiKey, ApiKeyCreated, ApiKeyPage, AvailablePrice, AvailablePricePage, CreateApiKeyRequest,
    LogPage, LogStats, Problem, RedeemRequest, RedemptionResult, RequestLog, UpdateApiKeyRequest,
    UserRole, UserWithBalance, VersionInfo, Wallet, WalletDailyItem, WalletMonth, WalletToday,
    WalletTopModelsItem, WalletWeek,
};

use crate::problem;

// ---------- 鉴权上下文(装配层注入) ----------

/// 当前请求的鉴权上下文(二进制 Claims 的映射;装配层中间件塞进
/// extensions,`user_auth_scope` 转入 task-local)。
#[derive(Debug, Clone)]
pub struct UserAuth {
    /// JWT sub(用户 id,不透明 string)
    pub user_id: String,
    pub username: String,
    /// "admin" | "user"(词表外值按普通用户处理,同旧实现)
    pub role: String,
}

impl UserAuth {
    fn is_admin(&self) -> bool {
        self.role == "admin"
    }
}

tokio::task_local! {
    /// 当前请求的鉴权上下文(由 [`user_auth_scope`] 注入)。
    static CURRENT_USER_AUTH: UserAuth;
}

/// 取当前请求的鉴权上下文;None = 中间件未注入(装配缺失/直调),
/// 全部方法按 fail-closed 401 处理。
fn current_auth() -> Option<UserAuth> {
    CURRENT_USER_AUTH.try_with(|a| a.clone()).ok()
}

/// 统一的 401 problem 体(spec Unauthorized 示例口径)。
fn invalid_token() -> Problem {
    problem(401, "invalid_token", "Invalid or expired token")
}

/// 存储不可用时的 problem 体(过渡期映射,见文件头"已知 spec 缺口"):
/// 响应枚举缺 5xx 变体,HTTP 状态按各 op 枚举可得变体妥协,`code` 保留
/// 真实短码(与 admin_users / admin_billing 批同策,api-mapping 备注 13)。
/// `status` 只能取该 op 枚举里存在的变体状态码。
fn store_down(status: u16) -> Problem {
    problem(status, "internal_error", "存储不可用,请稍后再试")
}

// ---------- 过渡期存储端口 ----------
//
// aikun-store 就位前(plan 步骤 8)aikun-admin 不直接依赖 rusqlite/uuid/
// sha2:全部 SQL 与 key 材料生成由二进制装配层的适配器(main.rs 的
// DbUserStore)实现本端口;业务编排(权限分叉、分页 clamp、校验、限流、
// 信封映射)在本 crate。SQL 语义逐条迁自旧 handler,见各方法文档。

/// 出参视角的用户行(getCurrentUser)。时间字段是库内原文
/// (`YYYY-MM-DD HH:MM:SS` 或 RFC3339),出参统一经 `sqlite_utc` 转换。
pub struct UserBalanceRecord {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub is_active: bool,
    pub created_at: String,
    pub updated_at: String,
    /// 余额(整数微元;1 元 = 1_000_000)
    pub balance_micro: i64,
}

/// 库内视角的 API key 行:`key_suffix` 是明文后 4 位(掩码展示用),
/// 完整明文与哈希永不出端口。
pub struct ApiKeyRecord {
    pub id: String,
    pub user_id: String,
    pub key_suffix: String,
    pub name: String,
    pub is_active: bool,
    /// 库内原文(RFC3339 / SQLite datetime 混存),出参经宽松解析转换
    pub last_used_at: Option<String>,
    pub expires_at: Option<String>,
    /// JSON array 文本
    pub models_json: String,
    pub rate_limit_rpm: i64,
    pub quota_daily_tokens: i64,
    pub max_concurrent: i64,
    pub created_at: String,
}

/// 待插入的新 API key(业务侧已定默认值;id/哈希/后缀由装配层生成)。
pub struct NewApiKey {
    pub id: String,
    pub user_id: String,
    pub key_hash: String,
    pub key_suffix: String,
    pub name: String,
    /// None = 永不过期;原文入库(与旧实现一致,不做归一)
    pub expires_at: Option<String>,
    pub models_json: String,
    pub rate_limit_rpm: i64,
    pub quota_daily_tokens: i64,
    pub max_concurrent: i64,
}

/// PATCH 的部分更新集(全部 None = 无可更新字段)。`expires_at`:
/// `Some(None)` = 清除(永不过期),`Some(Some(s))` = 设置原文。
pub struct ApiKeyUpdates {
    pub name: Option<String>,
    pub is_active: Option<bool>,
    pub expires_at: Option<Option<String>>,
    pub models_json: Option<String>,
    pub rate_limit_rpm: Option<i64>,
    pub quota_daily_tokens: Option<i64>,
    pub max_concurrent: Option<i64>,
}

/// 日志过滤条件(列表与统计共用,保证口径一致;迁自 build_log_filters)。
/// 非管理员时 `user_id` 由 impl 强制填 caller 自身。
#[derive(Default)]
pub struct LogFilters {
    pub user_id: Option<String>,
    pub model: Option<String>,
    /// 0 / 1(生成路由已把 success 参数解码为枚举,非法值到不了 impl)
    pub success: Option<i32>,
    /// 词法比较原文,不解析(api-mapping 备注 12)
    pub since: Option<String>,
}

/// 日志行(LEFT JOIN 用户名/渠道名已带出;id 三列为库内字符串)。
pub struct LogRecord {
    pub id: String,
    pub user_id: Option<String>,
    pub api_key_id: Option<String>,
    pub provider_id: Option<String>,
    pub username: Option<String>,
    pub provider_name: Option<String>,
    pub model: String,
    pub request_type: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub cached_tokens: i64,
    pub latency_ms: i32,
    pub status_code: i32,
    pub success: bool,
    pub error_message: Option<String>,
    /// 整数微元
    pub cost_micro: i64,
    pub created_at: String,
}

/// 统计原始值(SQL 聚合原文;修约与换算在 impl)。
pub struct LogStatsRaw {
    pub total_requests: i64,
    pub total_tokens: i64,
    pub avg_latency_ms: f64,
    /// 百分数(0..100,如 98.75)
    pub success_rate_pct: f64,
    pub total_cost_micro: i64,
}

/// 钱包快照(近 30 天,`since_date` 起;daily 已合并
/// request_logs 与 usage_daily 两表,不重复计数)。
pub struct WalletSnapshot {
    pub balance_micro: i64,
    /// (date "YYYY-MM-DD", cost_micro, tokens, requests)
    pub daily: Vec<(String, i64, i64, i64)>,
    /// (model, cost_micro, requests) 按费用降序 LIMIT 8
    pub top_models: Vec<(String, i64, i64)>,
}

/// 每模型单价三元组(prompt, completion, cached;每 1M tokens,元)。
pub type PriceTriple = (f64, f64, Option<f64>);

/// 过渡期 User tag 存储端口(装配层实现,复用二进制 db 模块;适配器写
/// main.rs,由集成工人装配)。`Err(())` = 存储不可用(如连接锁中毒)。
/// 阶段 S async 化(docs/surreal-migration.md §六):端口随装配适配器
/// 翻转 `#[async_trait]`;纯计算方法(generate_api_key_material /
/// hash_code)保持 sync。
#[allow(clippy::result_unit_err)]
#[async_trait::async_trait]
pub trait UserStore: Send + Sync + 'static {
    /// 按 id 查用户行(含余额)。Ok(None) = 不存在。
    async fn find_user_with_balance(&self, user_id: &str) -> Result<Option<UserBalanceRecord>, ()>;

    /// 生成 key 材料(id、明文 sk-…、sha256 哈希、明文后 4 位)。
    /// uuid/sha2 依赖在装配层(aikun-admin 不引入)。
    fn generate_api_key_material(&self) -> (String, String, String, String);

    /// 插入新 key 并回读(含 DB 默认 created_at)。Err(()) = 插入失败。
    async fn insert_api_key(&self, row: &NewApiKey) -> Result<ApiKeyRecord, ()>;

    /// 列指定用户的 key(admin 跨用户时 target 为他人)。
    /// 返回 (total, 页);ORDER BY created_at DESC。
    async fn list_api_keys(
        &self,
        target_user_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<ApiKeyRecord>), ()>;

    /// 部分更新 + 回读。`owner` = Some(uid) 时 SQL 附加属主过滤
    /// (非 admin 改他人 key 影响行数为 0 → 404,不暴露存在性)。
    /// Ok(None) = 未命中;Err(()) = 存储不可用/更新失败。
    async fn update_api_key(
        &self,
        key_id: &str,
        owner: Option<&str>,
        updates: &ApiKeyUpdates,
    ) -> Result<Option<ApiKeyRecord>, ()>;

    /// 删除(带属主过滤);返回受影响行数。
    async fn delete_api_key(&self, key_id: &str, owner: Option<&str>) -> Result<i64, ()>;

    /// 日志列表(LEFT JOIN users/providers;ORDER BY created_at DESC
    /// LIMIT ? OFFSET ?)。返回 (total, 页)。
    async fn query_logs(
        &self,
        filters: &LogFilters,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<LogRecord>), ()>;

    /// 日志统计(与列表共用 filters)。
    async fn query_log_stats(&self, filters: &LogFilters) -> Result<LogStatsRaw, ()>;

    /// 钱包快照:`since_date`(含)起的每日序列 + 模型分布 + 当前余额。
    async fn wallet_snapshot(&self, user_id: &str, since_date: &str) -> Result<WalletSnapshot, ()>;

    /// 在售价目:健康渠道提供的全部模型(已排序)逐个经 find_price 展开。
    /// None = 未定价(三价出 null)。
    async fn available_prices(&self) -> Result<Vec<(String, Option<PriceTriple>)>, ()>;

    /// sha256(code)(十六进制);依赖在装配层。
    fn hash_code(&self, code: &str) -> String;

    /// 核销 + 加余额 + 记流水(kind='recharge', note='兑换码充值'),
    /// 同一事务;UPDATE...WHERE status='unused' AND 未过期 的影响行数
    /// 判竞争:并发兑换同一码只有一方生效。
    /// Ok(None) = 无效/已用/已禁用/已过期(不细分,防枚举)。
    /// Ok(Some((amount_micro, balance_after_micro))) = 成功。
    async fn redeem(&self, user_id: &str, code_hash: &str) -> Result<Option<(i64, i64)>, ()>;
}

// ---------- 服务 ----------

/// 兑换失败限流:同一用户 10 分钟 5 次(内存滑动窗口,重启清零,
/// 尽力而为;迁自 src/api/redemption.rs 常量)。
const REDEEM_WINDOW: Duration = Duration::from_secs(600);
const REDEEM_MAX_FAILS: usize = 5;

/// 分页参数口径(api-mapping 全局语义 #3):默认 50,clamp 1..=200;
/// offset 默认 0、负值归 0(生成侧已有 minimum:0 → 422,这是兜底)。
fn clamp_page(limit: Option<i64>, offset: Option<i64>) -> (i64, i64) {
    (
        limit.unwrap_or(50).clamp(1, 200),
        offset.unwrap_or(0).max(0),
    )
}

/// 锁中毒时取回内部数据继续干(踩坑 #9:不 fail-open 也不 fail-closed)。
fn lock_map(
    map: &Arc<Mutex<HashMap<String, Vec<Instant>>>>,
) -> MutexGuard<'_, HashMap<String, Vec<Instant>>> {
    map.lock().unwrap_or_else(|e| e.into_inner())
}

/// 微元 → 元(整除 1e6 在 f64 下精确;口径同 billing::micro_to_yuan)。
fn micro_to_yuan(micro: i64) -> f64 {
    micro as f64 / 1_000_000.0
}

/// 宽松过期时间解析(迁自 src/auth/mod.rs::parse_expiry,行为一致):
/// RFC3339 / `%Y-%m-%dT%H:%M` / `%Y-%m-%d %H:%M[:SS]` / 日期(当日零点 UTC)。
fn parse_expiry(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&chrono::Utc));
    }
    for fmt in ["%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(naive.and_utc());
        }
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return date.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc());
    }
    None
}

/// 掩码:`sk-****` + 明文后 4 位(列表/更新视角永不出全量明文)。
fn mask_api_key(key_suffix: &str) -> String {
    format!("sk-****{key_suffix}")
}

/// 兑换码归一化:去全部空白 + 转大写(输入允许小写/多空格)。
fn normalize_redeem_code(raw: &str) -> String {
    raw.split_whitespace().collect::<String>().to_uppercase()
}

/// 库内字符串 ID → spec 的 int64 出参:可解析则数值,否则显式 null
/// (UUID 不丢字段名,待 spec 修正为 string;见文件头缺口说明)。
fn opaque_id(s: &Option<String>) -> Option<Option<i64>> {
    Some(s.as_deref().and_then(|v| v.parse::<i64>().ok()))
}

/// 可空时间列(SQLite datetime / RFC3339 混存)→ 显式 null 或解析值;
/// 解析失败的存量脏数据按 null 出(创建/更新路径已保证可解析)。
fn opt_sqlite_time(s: &Option<String>) -> Option<Option<chrono::DateTime<chrono::Utc>>> {
    Some(s.as_deref().and_then(parse_expiry))
}

/// 出参视角的 key 映射(key 恒掩码;时间为 RFC3339)。
fn to_api_key(rec: &ApiKeyRecord) -> ApiKey {
    ApiKey {
        created_at: crate::sqlite_utc(&rec.created_at),
        expires_at: opt_sqlite_time(&rec.expires_at),
        id: rec.id.clone(),
        is_active: rec.is_active,
        key: mask_api_key(&rec.key_suffix),
        last_used_at: opt_sqlite_time(&rec.last_used_at),
        max_concurrent: Some(rec.max_concurrent),
        models: serde_json::from_str(&rec.models_json).unwrap_or_default(),
        name: rec.name.clone(),
        quota_daily_tokens: Some(rec.quota_daily_tokens),
        rate_limit_rpm: Some(rec.rate_limit_rpm),
        user_id: rec.user_id.clone(),
    }
}

/// 日志行 → spec RequestLog(cost 微元→元;token 列库内 NOT NULL,
/// 出参恒有值;id 三列见 `opaque_id` 的 spec 缺口说明)。
fn to_api_log(rec: LogRecord) -> RequestLog {
    RequestLog {
        api_key_id: opaque_id(&rec.api_key_id),
        cached_tokens: Some(rec.cached_tokens),
        completion_tokens: Some(rec.completion_tokens),
        cost: micro_to_yuan(rec.cost_micro),
        created_at: crate::sqlite_utc(&rec.created_at),
        error_message: Some(rec.error_message),
        id: rec.id,
        latency_ms: Some(rec.latency_ms),
        model: rec.model,
        prompt_tokens: Some(rec.prompt_tokens),
        provider_id: opaque_id(&rec.provider_id),
        provider_name: Some(rec.provider_name),
        request_type: rec.request_type,
        status_code: Some(rec.status_code),
        success: rec.success,
        total_tokens: Some(rec.total_tokens),
        user_id: opaque_id(&rec.user_id),
        username: Some(rec.username),
    }
}

/// User tag 服务。`redeem_attempts` / `api_key_rate` 与二进制
/// AppState 共享同一实例(同 Auth 批 login_attempts 的注入模式)。
pub struct UserService<S: UserStore> {
    store: Arc<S>,
    redeem_attempts: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
    api_key_rate: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
    /// 服务版本(装配层注入二进制 env!("CARGO_PKG_VERSION"))
    version: String,
}

impl<S: UserStore> Clone for UserService<S> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            redeem_attempts: Arc::clone(&self.redeem_attempts),
            api_key_rate: Arc::clone(&self.api_key_rate),
            version: self.version.clone(),
        }
    }
}

impl<S: UserStore> UserService<S> {
    pub fn new(
        store: Arc<S>,
        redeem_attempts: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
        api_key_rate: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            store,
            redeem_attempts,
            api_key_rate,
            version: version.into(),
        }
    }

    // ---- 兑换限流(内存滑动窗口;与登录限流同构) ----

    fn redeem_limited(&self, user_id: &str) -> bool {
        let now = Instant::now();
        let mut attempts = lock_map(&self.redeem_attempts);
        attempts.retain(|_, v| {
            v.retain(|t| now.duration_since(*t) < REDEEM_WINDOW);
            !v.is_empty()
        });
        attempts
            .get(user_id)
            .is_some_and(|v| v.len() >= REDEEM_MAX_FAILS)
    }

    fn record_redeem_failure(&self, user_id: &str) {
        lock_map(&self.redeem_attempts)
            .entry(user_id.to_string())
            .or_default()
            .push(Instant::now());
    }

    fn clear_redeem_failures(&self, user_id: &str) {
        lock_map(&self.redeem_attempts).remove(user_id);
    }

    /// 429 的 `Retry-After` 建议秒数:窗口内最早一次失败到期为止,向上
    /// 取整、至少 1s(目标态新增,api-mapping 全局语义 #12)。
    fn redeem_retry_after_secs(&self, user_id: &str) -> u64 {
        let now = Instant::now();
        lock_map(&self.redeem_attempts)
            .get(user_id)
            .and_then(|v| v.first())
            .map(|oldest| now.duration_since(*oldest))
            .map(|elapsed| {
                let rem = REDEEM_WINDOW.saturating_sub(elapsed);
                rem.as_secs() + u64::from(rem.subsec_nanos() > 0)
            })
            .unwrap_or(1)
            .max(1)
    }

    fn redeem_rate_limited(&self, user_id: &str) -> RedeemCodeResponse {
        RedeemCodeResponse::TooManyRequests(
            problem(429, "rate_limited", "失败次数过多,请稍后再试"),
            RedeemCodeResponseTooManyRequestsHeaders {
                retry_after: Some(self.redeem_retry_after_secs(user_id).to_string()),
            },
        )
    }

    /// 列表/统计共用的过滤构建(迁自 build_log_filters:非 admin 强制只看
    /// 自己;admin 的 user_id 非空才生效;model/since 非空才生效)。
    /// success 已是生成枚举,不存在非法值(见文件头 spec 缺口说明)。
    fn log_filters(
        &self,
        auth: &UserAuth,
        model: Option<String>,
        success: Option<i32>,
        user_id: Option<String>,
        since: Option<String>,
    ) -> LogFilters {
        LogFilters {
            user_id: if auth.is_admin() {
                user_id.filter(|u| !u.is_empty())
            } else {
                Some(auth.user_id.clone())
            },
            model: model.filter(|m| !m.is_empty()),
            success,
            since: since.filter(|s| !s.is_empty()),
        }
    }
}

// ---------- trait 实现(生成路由入口) ----------

#[async_trait::async_trait]
impl<S: UserStore> UserApi for UserService<S> {
    /// 当前登录用户信息 + 余额
    ///
    /// `GET /api/users/me`(旧 GET /api/me;404 码 user_not_found → not_found)
    async fn get_current_user(&self) -> GetCurrentUserResponse {
        let Some(auth) = current_auth() else {
            return GetCurrentUserResponse::Unauthorized(invalid_token());
        };
        // 旧行为:任何查询错误(含锁中毒)→ 404;保持(见文件头)。
        let found = self
            .store
            .find_user_with_balance(&auth.user_id)
            .await
            .ok()
            .flatten();
        match found {
            Some(rec) => GetCurrentUserResponse::Ok(UserWithBalance {
                balance: micro_to_yuan(rec.balance_micro),
                created_at: crate::sqlite_utc(&rec.created_at),
                display_name: rec.display_name,
                id: rec.id,
                is_active: rec.is_active,
                role: match rec.role.as_str() {
                    "admin" => UserRole::Admin,
                    _ => UserRole::User,
                },
                updated_at: crate::sqlite_utc(&rec.updated_at),
                username: rec.username,
            }),
            None => GetCurrentUserResponse::NotFound(problem(404, "not_found", "资源不存在")),
        }
    }

    /// 服务版本号
    ///
    /// `GET /api/system/version`(旧 GET /api/version;仍刻意放鉴权后)
    async fn get_version(&self) -> GetVersionResponse {
        if current_auth().is_none() {
            return GetVersionResponse::Unauthorized(invalid_token());
        }
        GetVersionResponse::Ok(VersionInfo {
            version: self.version.clone(),
        })
    }

    /// 列出本人(或 admin 指定用户)的 API key
    ///
    /// `GET /api/api-keys`(裸数组 → {items,total} + 分页;key 恒掩码)
    async fn list_api_keys(
        &self,
        limit: Option<i64>,
        offset: Option<i64>,
        user_id: Option<String>,
    ) -> ListApiKeysResponse {
        let Some(auth) = current_auth() else {
            return ListApiKeysResponse::Unauthorized(invalid_token());
        };
        // 权限分叉:admin 带 ?user_id= 跨用户查看;普通用户强制只看自己
        // (忽略该参数,不报错)。
        let target = if auth.is_admin() {
            user_id
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| auth.user_id.clone())
        } else {
            auth.user_id.clone()
        };
        let (limit, offset) = clamp_page(limit, offset);
        match self.store.list_api_keys(&target, limit, offset).await.ok() {
            Some((total, records)) => ListApiKeysResponse::Ok(ApiKeyPage {
                items: records.iter().map(to_api_key).collect(),
                total,
            }),
            None => {
                tracing::error!("list_api_keys: store unavailable (db lock poisoned?)");
                // 枚举只有 Unauthorized 变体(文件头):退 401,但 code 用
                // internal_error,不冒充凭证问题。
                ListApiKeysResponse::Unauthorized(store_down(401))
            }
        }
    }

    /// 创建 API key
    ///
    /// `POST /api/api-keys`(明文 key 仅本响应返回一次)
    async fn create_api_key(&self, body: CreateApiKeyRequest) -> CreateApiKeyResponse {
        let Some(auth) = current_auth() else {
            return CreateApiKeyResponse::Unauthorized(invalid_token());
        };
        // expires_at 空串/缺省 = 永不过期;非空必须可解析且在未来。
        let expires_at = body.expires_at.filter(|s| !s.trim().is_empty());
        if let Some(exp) = &expires_at
            && !matches!(parse_expiry(exp), Some(t) if t > chrono::Utc::now())
        {
            return CreateApiKeyResponse::BadRequest(problem(
                400,
                "invalid_expires_at",
                "expires_at is not a valid future timestamp",
            ));
        }

        let (id, key, key_hash, key_suffix) = self.store.generate_api_key_material();
        let row = NewApiKey {
            id,
            user_id: auth.user_id.clone(),
            key_hash,
            key_suffix,
            name: body.name.unwrap_or_default(),
            expires_at,
            models_json: serde_json::to_string(&body.models.unwrap_or_default())
                .unwrap_or_else(|_| "[]".to_string()),
            // schema minimum:0 已拦负值;.max(0) 兜底不劣化旧行为。
            rate_limit_rpm: body.rate_limit_rpm.unwrap_or(0).max(0),
            quota_daily_tokens: body.quota_daily_tokens.unwrap_or(0).max(0),
            max_concurrent: body.max_concurrent.unwrap_or(0).max(0),
        };
        match self.store.insert_api_key(&row).await.ok() {
            Some(rec) => CreateApiKeyResponse::Created(ApiKeyCreated {
                created_at: crate::sqlite_utc(&rec.created_at),
                expires_at: opt_sqlite_time(&rec.expires_at),
                id: rec.id,
                is_active: rec.is_active,
                // 完整明文仅此一次返回
                key,
                last_used_at: opt_sqlite_time(&rec.last_used_at),
                max_concurrent: Some(rec.max_concurrent),
                models: serde_json::from_str(&rec.models_json).unwrap_or_default(),
                name: rec.name,
                quota_daily_tokens: Some(rec.quota_daily_tokens),
                rate_limit_rpm: Some(rec.rate_limit_rpm),
                user_id: rec.user_id,
            }),
            None => {
                tracing::error!("create_api_key: insert failed for user {}", auth.username);
                // 写路径:400 + 真实短码(文件头过渡映射,避免 401 误杀会话)。
                CreateApiKeyResponse::BadRequest(store_down(400))
            }
        }
    }

    /// 部分更新 API key
    ///
    /// `PATCH /api/api-keys/{id}`({message:'updated'} → 返回更新后资源)
    async fn update_api_key(&self, id: String, body: UpdateApiKeyRequest) -> UpdateApiKeyResponse {
        let Some(auth) = current_auth() else {
            return UpdateApiKeyResponse::Unauthorized(invalid_token());
        };
        // 空串 = 清除过期时间;其余必须可解析且在未来。
        let expires_at = body.expires_at.map(|exp| {
            if exp.trim().is_empty() {
                None
            } else {
                Some(exp)
            }
        });
        if let Some(Some(exp)) = &expires_at
            && !matches!(parse_expiry(exp), Some(t) if t > chrono::Utc::now())
        {
            return UpdateApiKeyResponse::BadRequest(problem(
                400,
                "invalid_expires_at",
                "expires_at is not a valid future timestamp",
            ));
        }

        let updates = ApiKeyUpdates {
            name: body.name,
            is_active: body.is_active,
            expires_at,
            models_json: body
                .models
                .map(|m| serde_json::to_string(&m).unwrap_or_else(|_| "[]".to_string())),
            rate_limit_rpm: body.rate_limit_rpm.map(|v| v.max(0)),
            quota_daily_tokens: body.quota_daily_tokens.map(|v| v.max(0)),
            max_concurrent: body.max_concurrent.map(|v| v.max(0)),
        };
        let has_update = updates.name.is_some()
            || updates.is_active.is_some()
            || updates.expires_at.is_some()
            || updates.models_json.is_some()
            || updates.rate_limit_rpm.is_some()
            || updates.quota_daily_tokens.is_some()
            || updates.max_concurrent.is_some();
        // 未知字段-only 的 body 能过 schema(minProperties 数的是全部属性),
        // impl 侧仍按"无可更新字段"400(旧语义)。
        if !has_update {
            return UpdateApiKeyResponse::BadRequest(problem(
                400,
                "no_fields_to_update",
                "请求未携带任何可更新字段",
            ));
        }

        // admin 不受属主限制;非 admin 限自己的 key,未命中一律 404。
        let owner = if auth.is_admin() {
            None
        } else {
            Some(auth.user_id.clone())
        };
        let key_id = id.clone();
        match self
            .store
            .update_api_key(&key_id, owner.as_deref(), &updates)
            .await
            .ok()
        {
            Some(Some(rec)) => UpdateApiKeyResponse::Ok(to_api_key(&rec)),
            Some(None) => UpdateApiKeyResponse::NotFound(problem(404, "not_found", "资源不存在")),
            None => {
                tracing::error!("update_api_key: store unavailable (db lock poisoned?)");
                // 写路径:400 + 真实短码(文件头过渡映射,避免 401 误杀会话)。
                UpdateApiKeyResponse::BadRequest(store_down(400))
            }
        }
    }

    /// 删除 API key
    ///
    /// `DELETE /api/api-keys/{id}`(200 {message:'deleted'} → 204)
    async fn delete_api_key(&self, id: String) -> DeleteApiKeyResponse {
        let Some(auth) = current_auth() else {
            return DeleteApiKeyResponse::Unauthorized(invalid_token());
        };
        let owner = if auth.is_admin() {
            None
        } else {
            Some(auth.user_id.clone())
        };
        let key_id = id.clone();
        let deleted = self
            .store
            .delete_api_key(&key_id, owner.as_deref())
            .await
            .ok();
        match deleted {
            Some(n) if n > 0 => {
                // 清内存 RPM 滑动窗口死条目(窗口只在 key 自身有请求时才
                // 被 retain,被删 key 的非空窗口会常驻)。
                if let Ok(mut windows) = self.api_key_rate.lock() {
                    windows.remove(&id);
                }
                DeleteApiKeyResponse::NoContent
            }
            // 未命中(不存在或非属主,不暴露存在性)
            Some(_) => DeleteApiKeyResponse::NotFound(problem(404, "not_found", "资源不存在")),
            None => {
                tracing::error!("delete_api_key: store unavailable (db lock poisoned?)");
                // 删除幂等语义(同 admin_delete_provider):404 + 真实短码,
                // "当作未确认删除"最不误导,也避免 401 误杀会话。
                DeleteApiKeyResponse::NotFound(store_down(404))
            }
        }
    }

    /// 请求日志列表
    ///
    /// `GET /api/logs`(裸数组 → {items,total})
    async fn list_logs(
        &self,
        limit: Option<i64>,
        offset: Option<i64>,
        model: Option<String>,
        success: Option<ListLogsSuccess>,
        user_id: Option<String>,
        since: Option<String>,
    ) -> ListLogsResponse {
        let Some(auth) = current_auth() else {
            return ListLogsResponse::Unauthorized(invalid_token());
        };
        let (limit, offset) = clamp_page(limit, offset);
        let filters = self.log_filters(
            &auth,
            model,
            success.map(|s| match s {
                ListLogsSuccess::Variant0 => 0,
                ListLogsSuccess::Variant1 => 1,
            }),
            user_id,
            since,
        );
        match self.store.query_logs(&filters, limit, offset).await.ok() {
            Some((total, records)) => ListLogsResponse::Ok(LogPage {
                items: records.into_iter().map(to_api_log).collect(),
                total,
            }),
            None => {
                tracing::error!("list_logs: store unavailable (db lock poisoned?)");
                // 枚举无非 401 变体外的 5xx:400 + 真实短码(文件头过渡映射)。
                ListLogsResponse::BadRequest(store_down(400))
            }
        }
    }

    /// 日志统计(与列表共用筛选条件)
    ///
    /// `GET /api/logs/stats`
    async fn get_log_stats(
        &self,
        model: Option<String>,
        success: Option<GetLogStatsSuccess>,
        user_id: Option<String>,
        since: Option<String>,
    ) -> GetLogStatsResponse {
        let Some(auth) = current_auth() else {
            return GetLogStatsResponse::Unauthorized(invalid_token());
        };
        let filters = self.log_filters(
            &auth,
            model,
            success.map(|s| match s {
                GetLogStatsSuccess::Variant0 => 0,
                GetLogStatsSuccess::Variant1 => 1,
            }),
            user_id,
            since,
        );
        match self.store.query_log_stats(&filters).await.ok() {
            Some(raw) => GetLogStatsResponse::Ok(LogStats {
                total_requests: raw.total_requests,
                total_tokens: raw.total_tokens,
                avg_latency_ms: raw.avg_latency_ms.round() as i64,
                success_rate: (raw.success_rate_pct * 100.0).round() / 100.0,
                total_cost: micro_to_yuan(raw.total_cost_micro),
            }),
            None => {
                tracing::error!("get_log_stats: store unavailable (db lock poisoned?)");
                // 同 list_logs:400 + 真实短码(文件头过渡映射)。
                GetLogStatsResponse::BadRequest(store_down(400))
            }
        }
    }

    /// 钱包:本人余额 + 近 30 天消费分析
    ///
    /// `GET /api/users/me/wallet`(旧 GET /api/wallet;固定查本人)
    async fn get_wallet_stats(&self) -> GetWalletStatsResponse {
        let Some(auth) = current_auth() else {
            return GetWalletStatsResponse::Unauthorized(invalid_token());
        };
        // 窗口固定近 30 天(UTC 日期);today/week 拆分与缺日补零在业务侧,
        // 与 usage_stats 同口径。
        let today = chrono::Utc::now().date_naive();
        let fmt = |d: chrono::NaiveDate| d.format("%Y-%m-%d").to_string();
        let today_s = fmt(today);
        let week_s = fmt(today - chrono::Duration::days(6));
        let month_s = fmt(today - chrono::Duration::days(29));

        let uid = auth.user_id.clone();
        let snapshot = self.store.wallet_snapshot(&uid, &month_s).await.ok();
        let Some(snap) = snapshot else {
            tracing::error!("get_wallet_stats: store unavailable (db lock poisoned?)");
            // 枚举只有 Unauthorized 变体(文件头):退 401 + internal_error。
            return GetWalletStatsResponse::Unauthorized(store_down(401));
        };

        let (mut month_cost, mut month_tok, mut month_req) = (0i64, 0i64, 0i64);
        let (mut week_cost, mut week_req, mut today_cost, mut today_req) = (0i64, 0i64, 0i64, 0i64);
        for (d, c, t, r) in &snap.daily {
            month_cost += c;
            month_tok += t;
            month_req += r;
            if d.as_str() >= week_s.as_str() {
                week_cost += c;
                week_req += r;
            }
            if *d == today_s {
                today_cost += c;
                today_req += r;
            }
        }
        // 补齐缺失日期为 0,保证图表是完整的 30 天序列
        let daily_map: HashMap<String, (i64, i64, i64)> = snap
            .daily
            .into_iter()
            .map(|(d, c, t, r)| (d, (c, t, r)))
            .collect();
        let daily = (0..30)
            .map(|i| {
                let d = fmt(today - chrono::Duration::days(29 - i));
                let (c, t, r) = daily_map.get(&d).copied().unwrap_or((0, 0, 0));
                WalletDailyItem {
                    cost: Some(micro_to_yuan(c)),
                    date: chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok(),
                    requests: Some(r),
                    tokens: Some(t),
                }
            })
            .collect();

        GetWalletStatsResponse::Ok(Wallet {
            balance: micro_to_yuan(snap.balance_micro),
            daily,
            month: WalletMonth {
                cost: Some(micro_to_yuan(month_cost)),
                requests: Some(month_req),
                tokens: Some(month_tok),
            },
            today: WalletToday {
                cost: Some(micro_to_yuan(today_cost)),
                requests: Some(today_req),
            },
            top_models: snap
                .top_models
                .into_iter()
                .map(|(model, cost, requests)| WalletTopModelsItem {
                    cost: Some(micro_to_yuan(cost)),
                    model: Some(model),
                    requests: Some(requests),
                })
                .collect(),
            week: WalletWeek {
                cost: Some(micro_to_yuan(week_cost)),
                requests: Some(week_req),
            },
        })
    }

    /// 兑换充值码
    ///
    /// `POST /api/users/me/redemptions`(旧 POST /api/wallet/redeem;
    /// 200 → 201;429 补 Retry-After)
    async fn redeem_code(&self, body: RedeemRequest) -> RedeemCodeResponse {
        let Some(auth) = current_auth() else {
            return RedeemCodeResponse::Unauthorized(invalid_token());
        };
        let uid = auth.user_id.clone();
        if self.redeem_limited(&uid) {
            return self.redeem_rate_limited(&uid);
        }
        // 归一化:去空白 + 转大写(输入允许小写/多空格)。
        let code = normalize_redeem_code(&body.code);
        let invalid = || {
            self.record_redeem_failure(&uid);
            RedeemCodeResponse::BadRequest(problem(
                400,
                "invalid_code",
                "兑换码无效、已使用或已过期",
            ))
        };
        // 形状粗校验(与旧实现同界:AK- 前缀、长度上限)。
        if !code.starts_with("AK-") || code.len() > 32 {
            return invalid();
        }
        let code_hash = self.store.hash_code(&code);
        let redeemed = self.store.redeem(&uid, &code_hash).await.ok();
        match redeemed {
            Some(Some((amount_micro, balance_micro))) => {
                self.clear_redeem_failures(&uid);
                RedeemCodeResponse::Created(RedemptionResult {
                    amount: micro_to_yuan(amount_micro),
                    balance: micro_to_yuan(balance_micro),
                })
            }
            // 无效/已用/已禁用/已过期统一 400 不细分(防枚举)。
            Some(None) => invalid(),
            // 存储不可用:不计失败(不是用户的错);fail-closed 400 + 真实
            // 短码(文件头过渡映射)——不走 401,避免 DB 抖动误杀会话
            //(core.js 对任何 401 清 token 踢回登录页)。
            None => {
                tracing::error!("redeem_code: store unavailable (db lock poisoned?)");
                RedeemCodeResponse::BadRequest(store_down(400))
            }
        }
    }

    /// 在售模型价目(登录用户只读)
    ///
    /// `GET /api/models/prices`(旧 GET /api/prices;裸数组 → {items,total} + 分页)
    async fn list_public_prices(
        &self,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> ListPublicPricesResponse {
        if current_auth().is_none() {
            return ListPublicPricesResponse::Unauthorized(invalid_token());
        }
        let (limit, offset) = clamp_page(limit, offset);
        let prices = self.store.available_prices().await.ok();
        match prices {
            Some(rows) => {
                let total = rows.len() as i64;
                let items = rows
                    .into_iter()
                    .skip(offset as usize)
                    .take(limit as usize)
                    .map(|(model, price)| AvailablePrice {
                        cached_price: Some(price.as_ref().and_then(|p| p.2)),
                        completion_price: Some(price.as_ref().map(|p| p.1)),
                        model,
                        prompt_price: Some(price.as_ref().map(|p| p.0)),
                    })
                    .collect();
                ListPublicPricesResponse::Ok(AvailablePricePage { items, total })
            }
            None => {
                tracing::error!("list_public_prices: store unavailable (db lock poisoned?)");
                // 枚举只有 Unauthorized 变体(文件头):退 401 + internal_error;
                // 会话误杀需 spec 补 5xx 后重生成才能根治(记 notes 待裁决)。
                ListPublicPricesResponse::Unauthorized(store_down(401))
            }
        }
    }
}

// ---------- 装配层中间件 ----------

/// **鉴权上下文 task-local 注入中间件**(main.rs 挂在 user 路由上,位于
/// auth_middleware 与 Claims→UserAuth 适配层之后):读取 extensions 里的
/// [`UserAuth`] 放进 task-local 再执行下游;缺失(装配错误)fail-closed
/// 401 problem+json。生成 trait 方法拿不到请求上下文,这是 claims 进入
/// impl 的唯一通道(见文件头装配契约)。
pub async fn user_auth_scope(req: Request, next: Next) -> Response {
    let Some(auth) = req.extensions().get::<UserAuth>().cloned() else {
        let mut resp = (
            StatusCode::UNAUTHORIZED,
            axum::Json(problem(401, "invalid_token", "Invalid or expired token")),
        )
            .into_response();
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        return resp;
    };
    CURRENT_USER_AUTH.scope(auth, next.run(req)).await
}

// ---------- 单元测试(纯逻辑;DB 行为由 e2e_user.rs 在集成阶段验证) ----------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_clamp_follows_global_semantics() {
        assert_eq!(clamp_page(None, None), (50, 0));
        assert_eq!(
            clamp_page(Some(0), None),
            (1, 0),
            "limit=0 clamp 到 1(不 400)"
        );
        assert_eq!(clamp_page(Some(999), Some(-5)), (200, 0));
        assert_eq!(clamp_page(Some(3), Some(7)), (3, 7));
    }

    #[test]
    fn parse_expiry_accepts_loose_formats() {
        assert!(parse_expiry("2030-01-01T00:00:00Z").is_some());
        assert!(parse_expiry("2030-01-01 12:30").is_some());
        assert!(parse_expiry("2030-01-01 12:30:45").is_some());
        assert!(parse_expiry("2030-01-01").is_some());
        assert!(parse_expiry("").is_none());
        assert!(parse_expiry("not a date").is_none());
    }

    #[test]
    fn redeem_code_normalization() {
        assert_eq!(
            normalize_redeem_code(" ak-AbCd -2345  efgh "),
            "AK-ABCD-2345EFGH"
        );
    }

    #[test]
    fn masked_key_never_carries_plaintext() {
        assert_eq!(mask_api_key("wxyz"), "sk-****wxyz");
    }

    #[test]
    fn opaque_ids_parse_or_null() {
        assert_eq!(opaque_id(&Some("123".into())), Some(Some(123)));
        // UUID 落显式 null(spec int64 缺口,见文件头)
        assert_eq!(opaque_id(&Some("u-e2e-uuid".into())), Some(None));
        assert_eq!(opaque_id(&None), Some(None));
    }

    #[test]
    fn micro_yuan_conversion_is_exact() {
        assert_eq!(micro_to_yuan(1_000_000), 1.0);
        assert_eq!(micro_to_yuan(140), 0.00014);
    }
}
