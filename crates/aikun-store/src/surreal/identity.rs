//! # SurrealDB 身份域实现(user / api_key + 认证路径)
//!
//! **指派:并行工人 · Identity**(分工边界与纪律见
//! `docs/surreal-migration.md` §八)。
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
//!   (`user:<旧uuid>`,引用零翻译,见设计文档 §三)。本文件对外一律收发
//!   **裸 key**(ULID / 旧 uuid,与 SQLite 时代的管理面展示一致),
//!   `表:key` 全形也兼容(取 key 部分定位)。
//! * `balance` 一律 **i64 微元**(1 元 = 1e6 微元)——红线,不得引入
//!   浮点。
//! * `token_version` 语义不变:重置密码 +1,旧 JWT 全失效
//!   (改密联动见 `update_user` / `reset_admin_password`,对齐
//!   `aikun/src/main.rs` DbAdminUsersStore::update_user 与
//!   `pool.rs` reset_admin_password)。
//! * 新记录的 record id:`ulid::Ulid::new()` 作 RecordIdKey(可排序)。
//! * 认证哈希沿用 `crate::password::{hash_password, hash_api_key}`
//!   (Argon2id PHC / sha256 hex),落库形态与 SQLite 侧一致;哈希的
//!   **生成**在调用方,本实现只存取。
//! * 错误统一 `crate::StoreError`;唯一索引冲突(用户名/key 重复)按
//!   桩注释返回 `Err`(携带 Surreal 索引错误原文,调用方按需识别)。
//! * 语义基准 = SQLite 实现的现有行为(`aikun/src/main.rs` 的
//!   `DbAuthStore`/`DbUserStore`/`DbAdminUsersStore` 适配器 +
//!   `pool.rs`);对外 API 契约(openapi.yaml)不变。
//!
//! ## SurrealQL 口径(4.0.1 嵌入式引擎,本文件测试实测)
//!
//! * 动态 record 定位:`type::record('user', $key)`(4.0 起
//!   `type::thing` 更名 `type::record`,实测 parse error 证实);
//! * 聚合计数:`SELECT count() … GROUP ALL`——带 WHERE 时省略 GROUP
//!   ALL 会按行展开多行(实测 take 单值报 "contains multiple");
//! * UPDATE/DELETE 语句级回读:`UPDATE … WHERE` 返回受影响行数组、
//!   `DELETE … RETURN BEFORE` 返回删除前行数组,空数组 = 未命中;
//! * record 链接字段读侧是 `"user:<key>"` 字符串;未设置的
//!   `option<datetime>` 字段**整键缺席**(不是 null)——读侧统一按
//!   缺席/null 双口径降级 None;
//! * 行读出统一走 `serde_json::Value`(SDK 现成 impl,免 derive,
//!   见设计文档 §一序列化面),datetime 以 RFC3339 UTC 字符串出参,
//!   本文件 `parse_dt` 解析。

use crate::StoreError;
use crate::surreal::SurrealHandle;
use surrealdb::types::RecordId;

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

    // ---- 集成批补充(默认切换装配面) ----

    /// 按 id 查登录行(JWT 复核:role 覆盖、is_active、token_version 比对
    /// 的数据源;auth 中间件的 authenticate_jwt 口径)。
    async fn find_login_user_by_id(&self, id: &str) -> Result<Option<LoginUser>, StoreError>;

    /// /v1 Bearer/x-api-key 认证热路径的**单往返**完整行:key 自身状态 +
    /// 属主用户状态 + 模型白名单(SQLite 侧 JOIN users 的一体查询口径)。
    /// 失活/过期/白名单损坏的判定在调用方。
    async fn find_api_key_auth(&self, key_hash: &str)
    -> Result<Option<ApiKeyAuthFull>, StoreError>;
}

/// /v1 认证热路径所需的完整行(见 [`IdentityStore::find_api_key_auth`])。
#[derive(Debug, Clone)]
pub struct ApiKeyAuthFull {
    pub key_id: String,
    pub user_id: String,
    pub key_active: bool,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 模型白名单(空集 = 不限制;损坏行由调用方 fail-closed)。
    pub models: Vec<String>,
    pub username: String,
    pub role: String,
    pub user_active: bool,
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

/// 身份域的 SurrealDB 实现。
pub struct SurrealIdentity {
    handle: SurrealHandle,
}

impl SurrealIdentity {
    pub fn new(handle: SurrealHandle) -> Self {
        Self { handle }
    }
}

// ---------- 内部辅助(SDK 类型不出本文件;见 mod.rs 纪律) ----------

fn failed(e: impl std::fmt::Display) -> StoreError {
    StoreError::Failed(e.to_string())
}

/// 行内 record 链接字符串里的 key 反转义:形如 uuid 的 **字符串 key**
/// 在 `to_sql()` 里会被反引号包裹(`` user:`550e…` ``,与真 Uuid key 的
/// `u'…'` 字面量区分);key 本体不含反引号,剥掉即裸 key(实测)。
fn unquote_key(k: &str) -> &str {
    k.strip_prefix('`')
        .and_then(|s| s.strip_suffix('`'))
        .unwrap_or(k)
}

/// 外部不透明 ID(裸 key;兼容 `user:<key>` 全形)→ 裸 key。
/// 表名不匹配的全形输入按裸 key 处理(查询自然不命中,同 SQLite 的
/// `WHERE id = ?` 语义)。
fn bare_key(table: &str, id: &str) -> String {
    match id.split_once(':') {
        Some((t, k)) if t == table => unquote_key(k).to_string(),
        _ => id.to_string(),
    }
}

/// 外部不透明 ID → record id(内部查询绑定用)。
fn rid(table: &str, id: &str) -> RecordId {
    RecordId::new(table, bare_key(table, id))
}

/// 行内 record 链接字符串(`"user:<key>"`)→ 裸 key;非字符串/无冒号
/// 时返回原文(schemafull 行内该字段必然是 record 链接,防御性兜底)。
fn link_key(v: &serde_json::Value, field: &str) -> String {
    match v.get(field).and_then(serde_json::Value::as_str) {
        Some(s) => match s.split_once(':') {
            Some((_, k)) => unquote_key(k).to_string(),
            None => s.to_string(),
        },
        None => String::new(),
    }
}

fn j_str(v: &serde_json::Value, field: &str) -> String {
    v.get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn j_i64(v: &serde_json::Value, field: &str) -> i64 {
    v.get(field)
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0)
}

fn j_bool(v: &serde_json::Value, field: &str) -> bool {
    v.get(field)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn j_dt(v: &serde_json::Value, field: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    // datetime 出参是 RFC3339 UTC 字符串(实测 to_rfc3339_opts(AutoSi, true));
    // option<datetime> 未设置时整键缺席或 null,统一降级 None。
    v.get(field)
        .and_then(serde_json::Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc))
}

fn j_strs(v: &serde_json::Value, field: &str) -> Vec<String> {
    v.get(field)
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn dt_required(
    v: &serde_json::Value,
    field: &str,
    table: &str,
) -> Result<chrono::DateTime<chrono::Utc>, StoreError> {
    j_dt(v, field).ok_or_else(|| StoreError::Failed(format!("{table} 行缺少 {field}(datetime)")))
}

fn login_user_of(v: &serde_json::Value) -> Result<LoginUser, StoreError> {
    Ok(LoginUser {
        id: link_key(v, "id"),
        username: j_str(v, "username"),
        password_hash: j_str(v, "password_hash"),
        display_name: j_str(v, "display_name"),
        role: j_str(v, "role"),
        is_active: j_bool(v, "is_active"),
        created_at: dt_required(v, "created_at", "user")?,
        updated_at: dt_required(v, "updated_at", "user")?,
        token_version: j_i64(v, "token_version"),
    })
}

fn user_row_of(v: &serde_json::Value) -> Result<UserRow, StoreError> {
    Ok(UserRow {
        id: link_key(v, "id"),
        username: j_str(v, "username"),
        display_name: j_str(v, "display_name"),
        role: j_str(v, "role"),
        is_active: j_bool(v, "is_active"),
        // 余额:整数微元(红线),出参不经任何浮点换算。
        balance_micro: j_i64(v, "balance"),
        created_at: dt_required(v, "created_at", "user")?,
        updated_at: dt_required(v, "updated_at", "user")?,
    })
}

fn api_key_row_of(v: &serde_json::Value) -> Result<ApiKeyRow, StoreError> {
    Ok(ApiKeyRow {
        id: link_key(v, "id"),
        user_id: link_key(v, "user"),
        key_suffix: j_str(v, "key_suffix"),
        name: j_str(v, "name"),
        is_active: j_bool(v, "is_active"),
        models: j_strs(v, "models"),
        last_used_at: j_dt(v, "last_used_at"),
        expires_at: j_dt(v, "expires_at"),
        rate_limit_rpm: j_i64(v, "rate_limit_rpm"),
        quota_daily_tokens: j_i64(v, "quota_daily_tokens"),
        max_concurrent: j_i64(v, "max_concurrent"),
        created_at: dt_required(v, "created_at", "api_key")?,
    })
}

fn api_key_auth_row_of(v: &serde_json::Value) -> Result<ApiKeyAuthRow, StoreError> {
    Ok(ApiKeyAuthRow {
        id: link_key(v, "id"),
        user_id: link_key(v, "user"),
        is_active: j_bool(v, "is_active"),
        expires_at: j_dt(v, "expires_at"),
        // user.token_version 的 record 链接投影(实测有效),JWT 校验口径。
        token_version: j_i64(v, "token_version"),
    })
}

/// user 行读取列(两处用户读取共用;balance 为微元 int)。
const USER_COLS: &str =
    "id, username, display_name, role, is_active, balance, created_at, updated_at";

/// api_key 行读取列(管理面/自助出参口径;不含 key 哈希本体)。
const API_KEY_COLS: &str = "id, user, key_suffix, name, is_active, models, last_used_at, \
                            expires_at, rate_limit_rpm, quota_daily_tokens, max_concurrent, \
                            created_at";

#[async_trait::async_trait]
impl IdentityStore for SurrealIdentity {
    async fn find_login_user(&self, username: &str) -> Result<Option<LoginUser>, StoreError> {
        // unique_user_username 唯一索引点查;空结果 → None(SQLite 侧
        // `.ok()` 统一 401 防枚举的口径由调用方保留)。
        let mut res = self
            .handle
            .db()
            .query(
                "SELECT id, username, password_hash, display_name, role, is_active, \
                 token_version, created_at, updated_at \
                 FROM user WHERE username = $u",
            )
            .bind(("u", username.to_string()))
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        let row: Option<serde_json::Value> = res.take(0).map_err(failed)?;
        row.as_ref().map(login_user_of).transpose()
    }

    async fn find_user(&self, id: &str) -> Result<Option<UserRow>, StoreError> {
        let mut res = self
            .handle
            .db()
            .query(format!("SELECT {USER_COLS} FROM user WHERE id = $rid"))
            .bind(("rid", rid("user", id)))
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        let row: Option<serde_json::Value> = res.take(0).map_err(failed)?;
        row.as_ref().map(user_row_of).transpose()
    }

    async fn create_user(&self, row: NewUser) -> Result<UserRow, StoreError> {
        // 显式 id(迁移/种子)沿用为 record key;否则 ULID(可排序)。
        // 冗余字段(username 之外的画像列)走 schema DEFAULT;用户名撞
        // unique 索引 → check() 报错(SQLite 侧 unique violation 同源)。
        let key = row
            .id
            .clone()
            .unwrap_or_else(|| ulid::Ulid::new().to_string());
        let mut res = self
            .handle
            .db()
            .query(
                "CREATE type::record('user', $key) SET \
                 username = $username, password_hash = $password_hash, \
                 display_name = $display_name, role = $role",
            )
            .bind(("key", key))
            .bind(("username", row.username))
            .bind(("password_hash", row.password_hash))
            .bind(("display_name", row.display_name))
            .bind(("role", row.role))
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        let created: Option<serde_json::Value> = res.take(0).map_err(failed)?;
        created
            .as_ref()
            .map(user_row_of)
            .transpose()?
            .ok_or_else(|| StoreError::Failed("create_user 未回读到新行".to_string()))
    }

    async fn update_user(
        &self,
        id: &str,
        display_name: Option<String>,
        role: Option<String>,
        is_active: Option<bool>,
        password_hash: Option<String>,
    ) -> Result<Option<UserRow>, StoreError> {
        // SET 片段按 Some 拼接(值一律绑定,不内联);改密联动
        // token_version+1(旧 JWT 全失效,SQLite DbAdminUsersStore 同款);
        // updated_at 恒刷新(全 None 时仍执行,同 SQLite 侧行为)。
        let mut sets: Vec<&str> = Vec::new();
        if display_name.is_some() {
            sets.push("display_name = $display_name");
        }
        if role.is_some() {
            sets.push("role = $role");
        }
        if is_active.is_some() {
            sets.push("is_active = $is_active");
        }
        if password_hash.is_some() {
            sets.push("password_hash = $password_hash");
            sets.push("token_version = token_version + 1");
        }
        sets.push("updated_at = time::now()");
        let mut res = self
            .handle
            .db()
            .query(format!(
                "UPDATE user SET {} WHERE id = $rid",
                sets.join(", ")
            ))
            .bind(("display_name", display_name))
            .bind(("role", role))
            .bind(("is_active", is_active))
            .bind(("password_hash", password_hash))
            .bind(("rid", rid("user", id)))
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        rows.first().map(user_row_of).transpose()
    }

    async fn list_users(
        &self,
        q: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<UserRow>), StoreError> {
        // q:用户名/显示名子串过滤(SQLite 侧无此入参的先例,桩新增能力;
        // 双侧 lowercase 实现大小写不敏感,口径与 SQLite LIKE(ASCII
        // 不敏感)对齐)。count 带 WHERE 必须 GROUP ALL(见文件头)。
        let lq = q.trim().to_lowercase();
        let filter = if lq.is_empty() {
            String::new()
        } else {
            "WHERE string::lowercase(username) CONTAINS $lq \
             OR string::lowercase(display_name) CONTAINS $lq"
                .to_string()
        };
        let mut res = self
            .handle
            .db()
            .query(format!(
                "SELECT count() FROM user {filter} GROUP ALL; \
                 SELECT {USER_COLS} FROM user {filter} \
                 ORDER BY created_at DESC LIMIT $limit START $offset"
            ))
            .bind(("lq", lq))
            .bind(("limit", limit))
            .bind(("offset", offset))
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        let total: Option<i64> = res.take((0, "count")).map_err(failed)?;
        let rows: Vec<serde_json::Value> = res.take(1).map_err(failed)?;
        let users = rows
            .iter()
            .map(user_row_of)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((total.unwrap_or(0), users))
    }

    async fn count_active_admins(&self) -> Result<i64, StoreError> {
        let mut res = self
            .handle
            .db()
            .query("SELECT count() FROM user WHERE role = 'admin' AND is_active = true GROUP ALL")
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        let count: Option<i64> = res.take((0, "count")).map_err(failed)?;
        Ok(count.unwrap_or(0))
    }

    async fn reset_admin_password(&self, password_hash: &str) -> Result<bool, StoreError> {
        // 目标选择与 SQLite pool.rs::reset_admin_password 逐句对齐:
        // 先 username='admin',没有再取最早创建的 role='admin' 用户。
        let mut res = self
            .handle
            .db()
            .query(
                "SELECT VALUE id FROM user WHERE username = 'admin' LIMIT 1; \
                 SELECT VALUE id FROM user WHERE role = 'admin' \
                 ORDER BY created_at ASC LIMIT 1",
            )
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        let by_name: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        let by_role: Vec<serde_json::Value> = res.take(1).map_err(failed)?;
        let target = by_name
            .first()
            .or(by_role.first())
            .and_then(serde_json::Value::as_str)
            .map(|s| match s.split_once(':') {
                Some((_, k)) => unquote_key(k).to_string(),
                None => s.to_string(),
            });
        let Some(key) = target else {
            return Ok(false); // 库中没有 admin,视为无需处理(SQLite 同款)
        };
        let mut res = self
            .handle
            .db()
            .query(
                "UPDATE user SET password_hash = $password_hash, \
                 token_version = token_version + 1, updated_at = time::now() \
                 WHERE id = $rid",
            )
            .bind(("password_hash", password_hash.to_string()))
            .bind(("rid", RecordId::new("user", key)))
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        Ok(!rows.is_empty())
    }

    async fn find_api_key_by_hash(
        &self,
        key_hash: &str,
    ) -> Result<Option<ApiKeyAuthRow>, StoreError> {
        // /v1 热路径:unique_api_key_key 唯一索引点查;user.token_version
        // 经 record 链接投影一次取回(JWT token_version 校验口径)。
        // 行不存在/失活/过期的判定在调用方(桩注释)。
        let mut res = self
            .handle
            .db()
            .query(
                "SELECT id, user, is_active, expires_at, \
                 user.token_version AS token_version \
                 FROM api_key WHERE key = $key_hash LIMIT 1",
            )
            .bind(("key_hash", key_hash.to_string()))
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        let row: Option<serde_json::Value> = res.take(0).map_err(failed)?;
        row.as_ref().map(api_key_auth_row_of).transpose()
    }

    async fn touch_api_key_used(&self, id: &str) -> Result<(), StoreError> {
        // 60s 节流(SQLite 侧 SQL 同款):不足 60s/刚写过不更新,
        // 避免每次请求都写库;未命中静默(尽力而为)。
        let _ = self
            .handle
            .db()
            .query(
                "UPDATE api_key SET last_used_at = time::now() WHERE id = $rid \
                 AND (last_used_at IS NONE OR last_used_at <= time::now() - 60s)",
            )
            .bind(("rid", rid("api_key", id)))
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        Ok(())
    }

    async fn insert_api_key(&self, row: NewApiKey) -> Result<ApiKeyRow, StoreError> {
        // user 列是 record<user> 链接;限额列不在 NewApiKey(桩契约),
        // 走 schema DEFAULT(0 / NONE),后续 update_api_key 可改。
        let key = row
            .id
            .clone()
            .unwrap_or_else(|| ulid::Ulid::new().to_string());
        let mut res = self
            .handle
            .db()
            .query(
                "CREATE type::record('api_key', $key) SET \
                 user = type::record('user', $user_key), key = $key_hash, \
                 key_suffix = $key_suffix, name = $name, models = $models",
            )
            .bind(("key", key))
            .bind(("user_key", bare_key("user", &row.user_id)))
            .bind(("key_hash", row.key_hash))
            .bind(("key_suffix", row.key_suffix))
            .bind(("name", row.name))
            .bind(("models", row.models))
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        let created: Option<serde_json::Value> = res.take(0).map_err(failed)?;
        created
            .as_ref()
            .map(api_key_row_of)
            .transpose()?
            .ok_or_else(|| StoreError::Failed("insert_api_key 未回读到新行".to_string()))
    }

    async fn update_api_key(
        &self,
        id: &str,
        owner: Option<&str>,
        patch: ApiKeyPatch,
    ) -> Result<Option<ApiKeyRow>, StoreError> {
        // 属主过滤:owner=Some 限本人的 key,未命中 → None(非 admin 跨
        // 用户 404 防存在性,SQLite DbUserStore::update_api_key 同款)。
        // expires_at:Some(None) = 清除(永不过期),Some(Some(t)) = 设置。
        let mut sets: Vec<&str> = Vec::new();
        if patch.name.is_some() {
            sets.push("name = $name");
        }
        if patch.is_active.is_some() {
            sets.push("is_active = $is_active");
        }
        if patch.expires_at.is_some() {
            sets.push("expires_at = $expires_at");
        }
        if patch.models.is_some() {
            sets.push("models = $models");
        }
        if patch.rate_limit_rpm.is_some() {
            sets.push("rate_limit_rpm = $rate_limit_rpm");
        }
        if patch.quota_daily_tokens.is_some() {
            sets.push("quota_daily_tokens = $quota_daily_tokens");
        }
        if patch.max_concurrent.is_some() {
            sets.push("max_concurrent = $max_concurrent");
        }
        let owner_clause = if owner.is_some() {
            " AND user = $owner"
        } else {
            ""
        };

        if sets.is_empty() {
            // 全空 patch:SQLite 侧会拼出非法 SQL 报错;桩契约是
            // "None = 不动",此处按契约回读现值(行为更正,注释存照)。
            let mut res = self
                .handle
                .db()
                .query(format!(
                    "SELECT {API_KEY_COLS} FROM api_key WHERE id = $rid{owner_clause}"
                ))
                .bind(("rid", rid("api_key", id)))
                .bind(("owner", owner.map(|o| rid("user", o))))
                .await
                .map_err(failed)?
                .check()
                .map_err(failed)?;
            let row: Option<serde_json::Value> = res.take(0).map_err(failed)?;
            return row.as_ref().map(api_key_row_of).transpose();
        }

        let mut res = self
            .handle
            .db()
            .query(format!(
                "UPDATE api_key SET {} WHERE id = $rid{owner_clause}",
                sets.join(", ")
            ))
            .bind(("name", patch.name))
            .bind(("is_active", patch.is_active))
            .bind(("expires_at", patch.expires_at))
            .bind(("models", patch.models))
            .bind(("rate_limit_rpm", patch.rate_limit_rpm))
            .bind(("quota_daily_tokens", patch.quota_daily_tokens))
            .bind(("max_concurrent", patch.max_concurrent))
            .bind(("rid", rid("api_key", id)))
            .bind(("owner", owner.map(|o| rid("user", o))))
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        rows.first().map(api_key_row_of).transpose()
    }

    async fn delete_api_key(&self, id: &str, owner: Option<&str>) -> Result<bool, StoreError> {
        // RETURN BEFORE 回读删除前行(4.0 默认不回传,显式声明稳定),
        // 行数即是否删除;属主过滤同 update。
        let owner_clause = if owner.is_some() {
            " AND user = $owner"
        } else {
            ""
        };
        let mut res = self
            .handle
            .db()
            .query(format!(
                "DELETE api_key WHERE id = $rid{owner_clause} RETURN BEFORE"
            ))
            .bind(("rid", rid("api_key", id)))
            .bind(("owner", owner.map(|o| rid("user", o))))
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        Ok(!rows.is_empty())
    }

    async fn list_api_keys(
        &self,
        user_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<ApiKeyRow>), StoreError> {
        // idx_api_key_user 索引覆盖 user 等值;ORDER BY created_at DESC
        // 与 SQLite 侧一致(同刻并列时顺序不保证,与 SQLite 同款口径)。
        let mut res = self
            .handle
            .db()
            .query(format!(
                "SELECT count() FROM api_key WHERE user = $user GROUP ALL; \
                 SELECT {API_KEY_COLS} FROM api_key WHERE user = $user \
                 ORDER BY created_at DESC LIMIT $limit START $offset"
            ))
            .bind(("user", rid("user", user_id)))
            .bind(("limit", limit))
            .bind(("offset", offset))
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        let total: Option<i64> = res.take((0, "count")).map_err(failed)?;
        let rows: Vec<serde_json::Value> = res.take(1).map_err(failed)?;
        let keys = rows
            .iter()
            .map(api_key_row_of)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((total.unwrap_or(0), keys))
    }

    // ---- 集成批补充(默认切换装配面) ----

    async fn find_login_user_by_id(&self, id: &str) -> Result<Option<LoginUser>, StoreError> {
        let mut res = self
            .handle
            .db()
            .query(
                "SELECT id, username, password_hash, display_name, role, is_active, \
                 token_version, created_at, updated_at \
                 FROM user WHERE id = $rid",
            )
            .bind(("rid", rid("user", id)))
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        let row: Option<serde_json::Value> = res.take(0).map_err(failed)?;
        row.as_ref().map(login_user_of).transpose()
    }

    async fn find_api_key_auth(
        &self,
        key_hash: &str,
    ) -> Result<Option<ApiKeyAuthFull>, StoreError> {
        // record 链接投影一次取齐 key + 属主用户(SQLite JOIN 的等价);
        // user.* 探测列是必填列,活行非 null。
        let mut res = self
            .handle
            .db()
            .query(
                "SELECT id, user, is_active, expires_at, models, \
                 user.username AS username, user.role AS role, \
                 user.is_active AS user_active \
                 FROM api_key WHERE key = $key_hash LIMIT 1",
            )
            .bind(("key_hash", key_hash.to_string()))
            .await
            .map_err(failed)?
            .check()
            .map_err(failed)?;
        let row: Option<serde_json::Value> = res.take(0).map_err(failed)?;
        Ok(row.and_then(|r| {
            // 属主用户已删(dangling 链接)→ 认证面等同无此 key。
            let username = j_str(&r, "username");
            if username.is_empty() {
                return None;
            }
            Some(ApiKeyAuthFull {
                key_id: link_key(&r, "id"),
                user_id: link_key(&r, "user"),
                key_active: j_bool(&r, "is_active"),
                expires_at: j_dt(&r, "expires_at"),
                models: j_strs(&r, "models"),
                username,
                role: j_str(&r, "role"),
                user_active: j_bool(&r, "user_active"),
            })
        }))
    }
}

#[cfg(test)]
mod tests {
    // 库内行为单测(设计文档 §七第 2 层):mem:// 起进程内实例,断言
    // 行映射/微元口径/token_version 联动/属主过滤等库内语义,不经服务。
    //
    // 栈配置:嵌入式引擎在调用任务栈上内联执行查询,默认 2MiB 测试
    // 线程会溢出(mod.rs mem_connect_schema_and_gate 实测),全部测试
    // 经 run_identity_test 以 16MiB 栈线程跑。
    use super::*;

    fn run_identity_test<F, Fut>(f: F)
    where
        F: FnOnce(SurrealIdentity) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()>,
    {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("test runtime");
                rt.block_on(async {
                    let ep = crate::surreal::Endpoint::parse("mem://").unwrap();
                    let handle = crate::surreal::SurrealHandle::connect(&ep)
                        .await
                        .expect("connect mem://");
                    handle.ensure_schema().await.expect("ensure schema");
                    f(SurrealIdentity::new(handle)).await;
                });
            })
            .expect("spawn big-stack thread")
            .join()
            .expect("join test thread");
    }

    fn new_user(username: &str, role: &str) -> NewUser {
        NewUser {
            id: None,
            username: username.to_string(),
            password_hash: "$argon2id$v=19$m=19456,t=2,p=1$test$test".to_string(),
            display_name: format!("display-{username}"),
            role: role.to_string(),
        }
    }

    #[test]
    fn user_crud_login_and_unique() {
        run_identity_test(|store| async move {
            // 创建:默认值(balance=0 微元/token_version=0/is_active=true)
            let u = store.create_user(new_user("alice", "user")).await.unwrap();
            assert!(!u.id.is_empty(), "自动 ULID key");
            assert_eq!(u.balance_micro, 0, "余额微元整数口径");
            assert!(u.is_active);
            assert_eq!(u.role, "user");
            assert_eq!(u.display_name, "display-alice");

            // 裸 key 与全形 record id 都能定位
            let by_id = store.find_user(&u.id).await.unwrap().unwrap();
            assert_eq!(by_id.username, "alice");
            let full_form = format!("user:{}", u.id);
            let by_full = store.find_user(&full_form).await.unwrap().unwrap();
            assert_eq!(by_full.id, u.id);

            // 登录行:password_hash/token_version 齐(管理面签发 JWT 口径)
            let login = store.find_login_user("alice").await.unwrap().unwrap();
            assert_eq!(login.id, u.id);
            assert!(login.password_hash.starts_with("$argon2id$"));
            assert_eq!(login.token_version, 0);
            assert!(store.find_login_user("nobody").await.unwrap().is_none());

            // 用户名唯一索引冲突 → Err(携带索引错误原文)
            let err = store
                .create_user(new_user("alice", "user"))
                .await
                .expect_err("duplicate username must error");
            assert!(
                err.to_string().contains("unique_user_username"),
                "unique index error surfaced: {err}"
            );

            // 显式 id(迁移/种子路径)沿用为 record key;形如 uuid 的
            // 字符串 key 在行内会被 to_sql 反引号包裹,读侧剥引号出裸 key
            let explicit = store
                .create_user(NewUser {
                    id: Some("550e8400-e29b-41d4-a716-446655440000".to_string()),
                    ..new_user("legacy", "user")
                })
                .await
                .unwrap();
            assert_eq!(explicit.id, "550e8400-e29b-41d4-a716-446655440000");
            assert!(
                store
                    .find_user("550e8400-e29b-41d4-a716-446655440000")
                    .await
                    .unwrap()
                    .is_some(),
                "显式 uuid key 与查询口径一致(record key 是字符串,非 Uuid 类型)"
            );
        });
    }

    #[test]
    fn update_user_semantics_and_token_version() {
        run_identity_test(|store| async move {
            let u = store.create_user(new_user("bob", "user")).await.unwrap();

            // 普通字段更新:updated_at 刷新,token_version 不动
            let updated = store
                .update_user(&u.id, Some("Bob B".to_string()), None, None, None)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(updated.display_name, "Bob B");
            assert!(
                store
                    .find_login_user("bob")
                    .await
                    .unwrap()
                    .unwrap()
                    .token_version
                    == 0,
                "非改密更新不递增 token_version"
            );
            assert!(updated.updated_at >= u.updated_at);

            // 改密联动 token_version+1(旧 JWT 全失效,SQLite 同款)
            let updated = store
                .update_user(
                    &u.id,
                    None,
                    Some("admin".to_string()),
                    Some(false),
                    Some("$argon2id$v=19$new$new".to_string()),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                store
                    .find_login_user("bob")
                    .await
                    .unwrap()
                    .unwrap()
                    .token_version,
                1,
                "改密后 token_version+1"
            );
            assert_eq!(updated.role, "admin");
            assert!(!updated.is_active);

            // 未命中 → None
            assert!(
                store
                    .update_user("does-not-exist", Some("x".to_string()), None, None, None)
                    .await
                    .unwrap()
                    .is_none()
            );
        });
    }

    #[test]
    fn list_users_filter_pagination_order() {
        run_identity_test(|store| async move {
            for name in ["carol", "alan", "alice", "dave"] {
                store.create_user(new_user(name, "user")).await.unwrap();
            }

            // q 过滤:用户名/显示名子串,大小写不敏感
            let (total, page) = store.list_users("AL", 10, 0).await.unwrap();
            assert_eq!(total, 2, "alan + alice(display-*al*)");
            let names: Vec<_> = page.iter().map(|u| u.username.clone()).collect();
            assert!(names.contains(&"alan".to_string()));
            assert!(names.contains(&"alice".to_string()));

            // display_name 命中:display-dave
            let (total, _) = store.list_users("display-dave", 10, 0).await.unwrap();
            assert_eq!(total, 1);

            // 空串 = 全量 + 分页
            let (total, page) = store.list_users("", 2, 0).await.unwrap();
            assert_eq!(total, 4);
            assert_eq!(page.len(), 2);
            let (_, page2) = store.list_users("", 2, 2).await.unwrap();
            assert_eq!(page2.len(), 2);
            // ORDER BY created_at DESC:后创建的在前(创建顺序固定)
            assert_eq!(page[0].username, "dave");
            assert_eq!(page[1].username, "alice");
        });
    }

    #[test]
    fn admin_count_and_reset_paths() {
        run_identity_test(|store| async move {
            // 无 admin → false(不报错)
            assert_eq!(store.count_active_admins().await.unwrap(), 0);
            assert!(!store.reset_admin_password("h1").await.unwrap());

            // 早创建的 role=admin(无 username=admin)
            let early = store.create_user(new_user("root1", "admin")).await.unwrap();
            let _late = store.create_user(new_user("root2", "admin")).await.unwrap();
            assert_eq!(store.count_active_admins().await.unwrap(), 2);

            // fallback:最早创建的 admin 拿到重置 + token_version+1
            assert!(store.reset_admin_password("h2").await.unwrap());
            let after = store.find_login_user("root1").await.unwrap().unwrap();
            assert_eq!(after.password_hash, "h2");
            assert_eq!(after.token_version, 1);
            let untouched = store.find_login_user("root2").await.unwrap().unwrap();
            assert_eq!(untouched.token_version, 0);

            // username='admin' 优先于更早的 role=admin
            let named = store.create_user(new_user("admin", "admin")).await.unwrap();
            assert!(store.reset_admin_password("h3").await.unwrap());
            let named_after = store.find_login_user("admin").await.unwrap().unwrap();
            assert_eq!(named_after.password_hash, "h3");
            assert_eq!(named_after.token_version, 1);
            assert_ne!(named.id, early.id);

            // 停用不计数(root1/root2 仍活跃,named 已停)
            store
                .update_user(&named.id, None, None, Some(false), None)
                .await
                .unwrap();
            assert_eq!(store.count_active_admins().await.unwrap(), 2);
        });
    }

    #[test]
    fn api_key_hash_lookup_touch_and_expiry() {
        run_identity_test(|store| async move {
            let u = store.create_user(new_user("kim", "user")).await.unwrap();
            let hash = crate::password::hash_api_key("sk-plain-secret");
            let k = store
                .insert_api_key(NewApiKey {
                    id: None,
                    user_id: u.id.clone(),
                    key_hash: hash.clone(),
                    key_suffix: "cret".to_string(),
                    name: "k1".to_string(),
                    models: vec!["gpt-4o".to_string(), "claude-*".to_string()],
                })
                .await
                .unwrap();
            assert_eq!(k.user_id, u.id, "属主链接读侧解析为裸 key");
            assert_eq!(k.key_suffix, "cret");
            assert_eq!(k.models, vec!["gpt-4o".to_string(), "claude-*".to_string()]);
            assert!(k.expires_at.is_none());
            assert!(k.last_used_at.is_none());

            // 热路径点查:哈希口径不变(sha256 hex),token_version 经链接投影
            let hit = store.find_api_key_by_hash(&hash).await.unwrap().unwrap();
            assert_eq!(hit.id, k.id);
            assert_eq!(hit.user_id, u.id);
            assert!(hit.is_active);
            assert_eq!(hit.token_version, 0);
            assert!(store.find_api_key_by_hash(&hash).await.unwrap().is_some());
            assert!(
                store
                    .find_api_key_by_hash(&crate::password::hash_api_key("sk-other"))
                    .await
                    .unwrap()
                    .is_none()
            );

            // touch:60s 节流内首次落 last_used_at
            store.touch_api_key_used(&k.id).await.unwrap();
            let (total, page) = store.list_api_keys(&u.id, 10, 0).await.unwrap();
            assert_eq!(total, 1);
            assert!(page[0].last_used_at.is_some());

            // expires_at 设置/清除(Some(None) = 永不过期)
            let exp = chrono::Utc::now() + chrono::Duration::hours(24);
            let updated = store
                .update_api_key(
                    &k.id,
                    None,
                    ApiKeyPatch {
                        expires_at: Some(Some(exp)),
                        rate_limit_rpm: Some(600),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(updated.expires_at, Some(exp));
            assert_eq!(updated.rate_limit_rpm, 600);

            let cleared = store
                .update_api_key(
                    &k.id,
                    None,
                    ApiKeyPatch {
                        expires_at: Some(None),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .unwrap();
            assert!(cleared.expires_at.is_none(), "Some(None) 清除过期时刻");

            // 认证行回读的过期时刻与库内一致
            let hit = store.find_api_key_by_hash(&hash).await.unwrap().unwrap();
            assert!(hit.expires_at.is_none());
        });
    }

    #[test]
    fn api_key_owner_filter_update_delete_list() {
        run_identity_test(|store| async move {
            let alice = store.create_user(new_user("alice", "user")).await.unwrap();
            let mallory = store
                .create_user(new_user("mallory", "user"))
                .await
                .unwrap();
            let mk = |user: &str, name: &str| NewApiKey {
                id: None,
                user_id: user.to_string(),
                key_hash: crate::password::hash_api_key(&format!("sk-{name}")),
                key_suffix: name[..2].to_string(),
                name: name.to_string(),
                models: vec![],
            };
            let a1 = store
                .insert_api_key(mk(&alice.id, "alicek1"))
                .await
                .unwrap();
            let a2 = store
                .insert_api_key(mk(&alice.id, "alicek2"))
                .await
                .unwrap();
            let _m1 = store
                .insert_api_key(mk(&mallory.id, "malkey1"))
                .await
                .unwrap();

            // 属主错误 → 未命中 None(404 防存在性)
            assert!(
                store
                    .update_api_key(
                        &a1.id,
                        Some(&mallory.id),
                        ApiKeyPatch {
                            name: Some("hijack".to_string()),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
            // 无属主(admin 口径)可改
            let ok = store
                .update_api_key(
                    &a1.id,
                    None,
                    ApiKeyPatch {
                        name: Some("renamed".to_string()),
                        is_active: Some(false),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(ok.name, "renamed");
            assert!(!ok.is_active);

            // 删除:属主错误 false,正确属主 true,admin 口径 true
            assert!(
                !store
                    .delete_api_key(&a2.id, Some(&mallory.id))
                    .await
                    .unwrap()
            );
            assert!(store.delete_api_key(&a2.id, Some(&alice.id)).await.unwrap());
            assert!(store.delete_api_key(&a1.id, None).await.unwrap());
            assert!(
                !store.delete_api_key(&a1.id, None).await.unwrap(),
                "已删再删 false"
            );

            // 列表只看属主,total 与行数一致
            let (total, page) = store.list_api_keys(&alice.id, 10, 0).await.unwrap();
            assert_eq!(total, 0);
            assert!(page.is_empty());
            let (total, page) = store.list_api_keys(&mallory.id, 10, 0).await.unwrap();
            assert_eq!(total, 1);
            assert_eq!(page[0].name, "malkey1");
        });
    }

    #[test]
    fn api_key_hash_unique_conflict() {
        run_identity_test(|store| async move {
            let u = store.create_user(new_user("uniq", "user")).await.unwrap();
            let hash = crate::password::hash_api_key("sk-same");
            let row = NewApiKey {
                id: None,
                user_id: u.id,
                key_hash: hash.clone(),
                key_suffix: "same".to_string(),
                name: "k".to_string(),
                models: vec![],
            };
            store.insert_api_key(row.clone()).await.unwrap();
            let err = store
                .insert_api_key(row)
                .await
                .expect_err("duplicate key hash must error");
            assert!(
                err.to_string().contains("unique_api_key_key"),
                "unique index error surfaced: {err}"
            );
        });
    }
}
