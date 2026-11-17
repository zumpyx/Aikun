//! # SurrealDB 身份域实现桩(阶段 S)
//!
//! **指派:并行工人 · Identity**(分工边界与纪律见
//! `docs/surreal-migration.md` §七)。
//!
//! ## 你负责的域
//!
//! `user` 与 `api_key` 两张表的全部读写,以及围绕它们的认证路径:
//!
//! * 登录查用户(`find_login_user`,管理面 `AuthStore` 端口的数据源);
//! * /v1 热路径的 API key 认证(`find_api_key_by_hash`:sha256 hex 点查,
//!   `unique_api_key_key` 唯一索引,行不存在/`is_active=false`/过期
//!   判定在调用方);
//! * 用户 CRUD(管理面 users 组 + 用户自助 me/wallet 的数据源);
//! * 兑换码兑换与调账要动的 `user.balance` **写入不在本文件**(原子性
//!   归 billing_store.rs 的 `redeem_code` / `adjust_balance`,你只提供
//!   读取与不含余额的更新)。
//!
//! ## 实现约定(必读)
//!
//! * `SurrealHandle` 经 `new` 注入,`db()` 取已选定 ns/db 的句柄;
//!   **不碰** mod.rs / SCHEMA_SURQL / config.rs——要新字段先在设计文档
//!   记待办,由集成批统一改。
//! * 表结构、字段映射(旧 SQLite 列 → 新字段)见 `docs/surreal-migration.md`
//!   §四/§五;`created_at`/`updated_at` 是原生 datetime,出参转
//!   `chrono::DateTime<Utc>`。
//! * ID 对外是不透明 string:record id 的字符串形如 `user:01J8…`
//!   (`表:ulid`);**迁移进来的旧行沿用旧 uuid 作 key**
//!   (`user:<旧uuid>`,引用零翻译,见设计文档 §三)。
//! * `balance` 一律 **i64 微元**(1 元 = 1e6 微元)——红线,不得引入
//!   浮点。
//! * `token_version` 语义不变:重置密码 +1,旧 JWT 全失效。
//! * 新 key 的 record id:`ulid::Ulid::new()` 作 RecordIdKey(可排序)。
//! * 认证哈希沿用 `crate::password::{hash_password, hash_api_key}`
//!   (Argon2id PHC / sha256 hex),落库形态与 SQLite 侧一致。
//! * 错误统一 `crate::StoreError`;唯一索引冲突(用户名/key 重复)
//!   按调用方需要返回 `Ok(None)`/`Err`——对齐 SQLite 侧端口行为。
//! * 语义基准 = SQLite 实现的现有行为(`aikun/src/main.rs` 的
//!   `DbAuthStore`/`DbUserStore`/`DbAdminUsersStore` 适配器 +
//!   `schema.rs`);对外 API 契约(openapi.yaml)不变。

use crate::StoreError;
use crate::surreal::SurrealHandle;

/// 登录所需用户行(password_hash/token_version 之外即 User 出参字段;
/// 与 aikun-admin `LoginUserRecord` 对齐,时间已是 UTC datetime)。
#[derive(Debug, Clone)]
pub struct LoginUser {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    pub display_name: String,
    pub role: String,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub token_version: i64,
}

/// 用户行(含余额,微元);me/wallet 与管理面 users 组的数据源。
#[derive(Debug, Clone)]
pub struct UserRow {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub is_active: bool,
    pub balance_micro: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// 新建用户(管理面创建/首启种子 admin);id 为空 = 自动生成 ULID key。
#[derive(Debug, Clone)]
pub struct NewUser {
    pub id: Option<String>,
    pub username: String,
    pub password_hash: String,
    pub display_name: String,
    pub role: String,
}

/// /v1 认证热路径回读的 key 行(哈希点查命中后所需的最小字段集)。
#[derive(Debug, Clone)]
pub struct ApiKeyAuthRow {
    pub id: String,
    pub user_id: String,
    pub is_active: bool,
    /// 过期时刻;None = 永不过期。
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub token_version: i64,
}

/// 身份域能力:async trait(async 化决策见设计文档 §六)。
#[async_trait::async_trait]
pub trait IdentityStore: Send + Sync {
    /// 按用户名查登录行;不存在返回 None(调用方统一 401 防枚举)。
    async fn find_login_user(&self, username: &str) -> Result<Option<LoginUser>, StoreError>;

    /// 按 id 查用户行(含余额);不存在返回 None。
    async fn find_user(&self, id: &str) -> Result<Option<UserRow>, StoreError>;

    /// 创建用户并回读;用户名冲突按调用方需要报错(unique 索引)。
    async fn create_user(&self, row: NewUser) -> Result<UserRow, StoreError>;

    /// 部分更新(display_name/role/is_active/password_hash;含属主/角色
    /// 守卫语义由调用方注入条件)。返回更新后的行;未命中 None。
    async fn update_user(
        &self,
        id: &str,
        display_name: Option<String>,
        role: Option<String>,
        is_active: Option<bool>,
        password_hash: Option<String>,
    ) -> Result<Option<UserRow>, StoreError>;

    /// 用户列表(q 为用户名/显示名模糊过滤,空串 = 全量),
    /// ORDER BY created_at DESC;返回 (total, 页)。
    async fn list_users(
        &self,
        q: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<UserRow>), StoreError>;

    /// 活跃管理员数(last_admin 409 守卫的数据源)。
    async fn count_active_admins(&self) -> Result<i64, StoreError>;

    /// 运维逃生口(AIKUN_RESET_ADMIN_PASSWORD):优先 username='admin',
    /// 否则最早创建的 role='admin' 用户;重置哈希 + token_version+1。
    /// 返回 false = 库中没有 admin。
    async fn reset_admin_password(&self, password_hash: &str) -> Result<bool, StoreError>;

    /// /v1 认证热路径:sha256(明文 key) 的 hex 点查。
    async fn find_api_key_by_hash(
        &self,
        key_hash: &str,
    ) -> Result<Option<ApiKeyAuthRow>, StoreError>;

    /// 认证命中后刷新 last_used_at(尽力而为,失败仅日志)。
    async fn touch_api_key_used(&self, id: &str) -> Result<(), StoreError>;

    /// 创建 API key(id 为空 = 自动生成 ULID;key 存哈希)并回读。
    async fn insert_api_key(&self, row: NewApiKey) -> Result<ApiKeyRow, StoreError>;

    /// 属主(非 admin 跨用户 404 防存在性)与限额/模型清单的部分更新;
    /// 返回更新后的行;未命中 None。
    async fn update_api_key(
        &self,
        id: &str,
        owner: Option<&str>,
        patch: ApiKeyPatch,
    ) -> Result<Option<ApiKeyRow>, StoreError>;

    /// 删除(带属主过滤);返回是否删除。
    async fn delete_api_key(&self, id: &str, owner: Option<&str>) -> Result<bool, StoreError>;

    /// 列指定用户的 key,ORDER BY created_at DESC;返回 (total, 页)。
    async fn list_api_keys(
        &self,
        user_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<ApiKeyRow>), StoreError>;
}

/// 新建 API key(材料生成在调用方;key = sha256 hex)。
#[derive(Debug, Clone)]
pub struct NewApiKey {
    pub id: Option<String>,
    pub user_id: String,
    pub key_hash: String,
    pub key_suffix: String,
    pub name: String,
    pub models: Vec<String>,
}

/// key 行(管理面/用户自助出参口径;不含 key 哈希本体)。
#[derive(Debug, Clone)]
pub struct ApiKeyRow {
    pub id: String,
    pub user_id: String,
    pub key_suffix: String,
    pub name: String,
    pub is_active: bool,
    pub models: Vec<String>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub rate_limit_rpm: i64,
    pub quota_daily_tokens: i64,
    pub max_concurrent: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// key 的部分更新(None = 不动;语义对齐 SQLite 侧 ApiKeyUpdates)。
#[derive(Debug, Clone, Default)]
pub struct ApiKeyPatch {
    pub name: Option<String>,
    pub is_active: Option<bool>,
    pub expires_at: Option<Option<chrono::DateTime<chrono::Utc>>>,
    pub models: Option<Vec<String>>,
    pub rate_limit_rpm: Option<i64>,
    pub quota_daily_tokens: Option<i64>,
    pub max_concurrent: Option<i64>,
}

/// 身份域的 SurrealDB 实现(桩:工人按上述约定填 todo!() 体)。
pub struct SurrealIdentity {
    handle: SurrealHandle,
}

impl SurrealIdentity {
    pub fn new(handle: SurrealHandle) -> Self {
        Self { handle }
    }
}

#[async_trait::async_trait]
impl IdentityStore for SurrealIdentity {
    async fn find_login_user(&self, username: &str) -> Result<Option<LoginUser>, StoreError> {
        let _ = (&self.handle, username);
        todo!("工人·Identity:SELECT ... FROM user WHERE username = $u(unique 索引)")
    }

    async fn find_user(&self, id: &str) -> Result<Option<UserRow>, StoreError> {
        let _ = (&self.handle, id);
        todo!("工人·Identity:SELECT record user:<id>(含 balance 微元)")
    }

    async fn create_user(&self, row: NewUser) -> Result<UserRow, StoreError> {
        let _ = (&self.handle, row);
        todo!("工人·Identity:CREATE user(ULID key 或显式 id)")
    }

    async fn update_user(
        &self,
        id: &str,
        display_name: Option<String>,
        role: Option<String>,
        is_active: Option<bool>,
        password_hash: Option<String>,
    ) -> Result<Option<UserRow>, StoreError> {
        let _ = (
            &self.handle,
            id,
            display_name,
            role,
            is_active,
            password_hash,
        );
        todo!("工人·Identity:UPDATE ... SET ... + updated_at = time::now(),回读")
    }

    async fn list_users(
        &self,
        q: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<UserRow>), StoreError> {
        let _ = (&self.handle, q, limit, offset);
        todo!("工人·Identity:SELECT ... WHERE string::contains / LIMIT / START + COUNT")
    }

    async fn count_active_admins(&self) -> Result<i64, StoreError> {
        let _ = &self.handle;
        todo!("工人·Identity:count(role='admin' AND is_active)")
    }

    async fn reset_admin_password(&self, password_hash: &str) -> Result<bool, StoreError> {
        let _ = (&self.handle, password_hash);
        todo!("工人·Identity:UPDATE 优先 username='admin' 的 admin,token_version +1")
    }

    async fn find_api_key_by_hash(
        &self,
        key_hash: &str,
    ) -> Result<Option<ApiKeyAuthRow>, StoreError> {
        let _ = (&self.handle, key_hash);
        todo!("工人·Identity:/v1 热路径点查(unique_api_key_key),JOIN user 取 token_version")
    }

    async fn touch_api_key_used(&self, id: &str) -> Result<(), StoreError> {
        let _ = (&self.handle, id);
        todo!("工人·Identity:UPDATE api_key SET last_used_at = time::now()")
    }

    async fn insert_api_key(&self, row: NewApiKey) -> Result<ApiKeyRow, StoreError> {
        let _ = (&self.handle, row);
        todo!("工人·Identity:CREATE api_key(用户 record 链接),回读")
    }

    async fn update_api_key(
        &self,
        id: &str,
        owner: Option<&str>,
        patch: ApiKeyPatch,
    ) -> Result<Option<ApiKeyRow>, StoreError> {
        let _ = (&self.handle, id, owner, patch);
        todo!("工人·Identity:UPDATE(属主过滤),回读")
    }

    async fn delete_api_key(&self, id: &str, owner: Option<&str>) -> Result<bool, StoreError> {
        let _ = (&self.handle, id, owner);
        todo!("工人·Identity:DELETE(属主过滤)")
    }

    async fn list_api_keys(
        &self,
        user_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<ApiKeyRow>), StoreError> {
        let _ = (&self.handle, user_id, limit, offset);
        todo!("工人·Identity:SELECT WHERE user = user:<id> ORDER created_at DESC")
    }
}
