//! Admin tag · users 方法组(用户 CRUD / 批量创建)。
//!
//! **指派:并行工人·Users**(docs/migration-playbook.md §〇)。方法体迁自
//! `aikun/src/api/admin/users.rs`(只读参考),对齐 `openapi.yaml` v0.5.0
//! 的 users 契约。`balance-transactions` 两个 op(调账 + 流水查询)按
//! lib.rs 的 facade 分组在 `admin_billing.rs`(Billing 批)实现——本批
//! 负责其 e2e 调账流水断言(`aikun/tests/e2e_admin_users.rs`)。
//!
//! # 语义变化(相对旧 `/api/admin/users`,api-mapping Admin 层表)
//!
//! | op | 变化 |
//! |---|---|
//! | adminListUsers | 全量 → `{items,total}` + limit/offset(默认 50,clamp 1..=200) |
//! | adminCreateUser | 201 保留;409 username_exists(problem+json) |
//! | adminCreateUsersBatch | 整体 200;逐项失败字段 `error` → `code` |
//! | adminGetUser | 404 保留 |
//! | adminUpdateUser | `{message:'updated'}` → **返回更新后的 User 资源**;last_admin 400 → **409**;改密 token_version+1 保留 |
//! | adminDeleteUser | 200 `{message:'disabled'}` → **204**;软禁用(is_active=0)保留 |
//!
//! # 装配契约(集成工人 / main.rs 侧,参照 DbAuthStore,main.rs:435)
//!
//! * 路由:`admin_api_router`(外层包 auth_middleware + require_admin,由
//!   集成工人统一装配);本文件只提供固有方法,签名 = trait 方法去 self,
//!   `AdminService.users` 字段路由到此处。
//! * 端口:`AdminUsersStore` 由 main.rs 适配器实现(下面每个方法的文档即
//!   SQL 语义),`AdminUsersService::new(std::sync::Arc::new(DbAdminUsersStore { .. }))`
//!   构造后装入 `AdminService.users`。aikun-store 就位后收编(plan 步骤 8)。
//! * Argon2 哈希经端口在 `spawn_blocking` 执行:CPU 密集,不进 DB 锁、
//!   不占 async executor(与旧实现 users.rs:174、users.rs:336 一致)。
//!
//! # 已知 spec 缺口(记 notes 报人工裁决,勿自创状态码)
//!
//! Users 组的响应枚举**全部缺 5xx 变体**(AdminCreateUserResponse 等,
//! errors.rs:1080 起;spec 只给 providers / model-health / redemption-codes
//! 三处 500,而 Problem 词表却保留着 create_failed / update_failed 等
//! 500 码——疑似遗漏)。旧实现"存储不可用/写入失败 → 500"无法表达。
//! 当前折衷(与 Auth 批 LoginServerResponse 同思路,playbook 踩坑 #2):
//! **fail-closed 403 + body 携带真实 500 词表 code + `tracing::error!`
//! 留痕**。选 403 不选 401:前端 core.js 对 401 会清 token 踢回登录页
//! (core.js:76),存储抖动不应踢会话。若拍板补 5xx,重生成后把
//! `store_failure` 的调用点换成 InternalError 变体即可(已集中收敛)。
//! 例外:`admin_get_user` 的查询错误旧实现本就与"不存在"同走 404
//! (users.rs:429 的 `.ok()` 语义,由适配器映射为 `Ok(None)`),不走 403。

use std::sync::Arc;

use aikun_openapi::openapi::server::errors::*;
use aikun_openapi::openapi::types::*;

use crate::problem;

// ---------- 过渡期存储端口(装配层实现;SQL 语义写在方法文档里) ----------

/// users 表出参行(password_hash / token_version 永不出参)。
/// 时间是 SQLite 存储原文(`YYYY-MM-DD HH:MM:SS`),出参经 `sqlite_utc`
/// 转 RFC3339;余额按库内口径整数微元(1 元 = 1e6 微元)。
pub struct AdminUserRecord {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub is_active: bool,
    pub created_at: String,
    pub updated_at: String,
    pub balance_micro: i64,
}

/// 单条创建入参(已校验、已哈希;ID 由存储层生成——不透明 string,
/// api-mapping 备注 2)。
pub struct NewUser {
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub password_hash: String,
}

/// 批量插入入参(与单个创建同构;`generated_password` 仅用于回显一次,
/// 不入库)。
pub struct PreparedUser {
    pub username: String,
    pub display_name: String,
    pub role: String,
    pub password_hash: String,
    pub generated_password: Option<String>,
}

/// 存储层错误。旧 500 词表的映射:Unavailable → internal_error(连接锁
/// 中毒等);Failed → 由调用点补 create_failed / update_failed /
/// delete_failed(见 `store_failure`)。
pub enum UsersStoreError {
    /// 连接锁中毒等存储不可用(旧 internal_error,users.rs:102/189)。
    Unavailable,
    /// SQL 执行失败(旧 query_failed / create_failed / ...,
    /// users.rs:112/217/537)。
    Failed,
}

/// 单条插入结果。
pub enum InsertOutcome {
    /// 创建成功,携带存储层生成的 ID。
    Created(String),
    /// username 唯一约束冲突(旧 is_unique_violation,users.rs:22)。
    UsernameExists,
}

/// 批量插入的逐条结果(单项失败不影响其余,旧相位 2 语义)。
pub enum BatchInsertItem {
    /// 创建成功,携带生成的 ID。
    Created(String),
    /// username 唯一约束冲突。
    UsernameExists,
    /// 该条 INSERT 失败(旧 per-item create_failed,users.rs:383)。
    Failed,
}

/// 条件更新结果。
pub enum UpdateOutcome {
    /// 更新成功,回读的整行(新版 PATCH 返回资源,语义变化 #6)。
    Updated(Box<AdminUserRecord>),
    /// 影响行数为 0(用户不存在)。
    NotFound,
    /// last_admin 保护命中(降级或禁用唯一活跃 admin)。
    LastAdmin,
}

/// 软禁用结果。
pub enum DisableOutcome {
    Disabled,
    NotFound,
    /// last_admin 保护命中。
    LastAdmin,
}

/// 条件更新载荷(Some 字段才进 SET 子句)。
pub struct UserPatch {
    pub display_name: Option<String>,
    /// Some 时同时 `token_version = token_version + 1`(踢该用户全部
    /// 已签发 JWT,users.rs:509)。
    pub password_hash: Option<String>,
    pub is_active: Option<bool>,
    pub role: Option<String>,
    /// 业务侧判定:本次改动会降级或禁用目标 → 存储层须在**同一把写锁内**
    /// 先做 last_admin 检查再更新(旧实现顺序,防 check-then-update 竞态,
    /// users.rs:486-496)。
    pub demotes_or_disables: bool,
}

/// users 方法组端口(过渡期由 main.rs 适配器实现,aikun-store 就位后收编)。
/// 方法是同步的:SQLite 进程内 + 互斥锁,单条开销极小(与 AuthStore 同口径)。
/// 阶段 S async 化(docs/surreal-migration.md §六):端口随装配适配器
/// 翻转 `#[async_trait]`;纯计算方法(random_password)保持 sync。
#[async_trait::async_trait]
pub trait AdminUsersStore: Send + Sync + 'static {
    /// 列表页(读锁):
    /// `SELECT id, username, display_name, role, is_active, created_at,
    ///  updated_at, balance FROM users ORDER BY created_at DESC, rowid DESC
    ///  LIMIT ? OFFSET ?` + `SELECT COUNT(*) FROM users`(total 为全量数,
    /// 与页无关;rowid 是分页稳定性加的并列决胜——旧行为仅
    /// created_at DESC,同秒顺序本就未定义,users.rs:108-109)。
    /// 行映射失败按旧语义静默跳过(filter_map .ok(),users.rs:117)。
    async fn list_users(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AdminUserRecord>, i64), UsersStoreError>;

    /// 查单个(读锁):同列 `WHERE id = ?`。**查询错误与不存在统一
    /// Ok(None)**(旧 `.ok()` 语义 → 404,users.rs:415-429);仅锁不可用
    /// 返回 Err。
    async fn find_user(&self, id: &str) -> Result<Option<AdminUserRecord>, UsersStoreError>;

    /// 插入单条(写锁):`INSERT INTO users (id, username, password_hash,
    /// display_name, role) VALUES (uuid4, ?, ?, ?, ?)`(users.rs:196-199)。
    /// 唯一约束冲突 → Ok(UsernameExists);其他 INSERT 错误 → Err(Failed)。
    async fn insert_user(&self, user: &NewUser) -> Result<InsertOutcome, UsersStoreError>;

    /// 批量插入(写锁):**一把锁内逐条** INSERT,不包整体事务(单项失败
    /// 不影响其余,users.rs:349-388);返回与入参**等长等序**的逐条结果,
    /// 单条非唯一约束失败 → BatchInsertItem::Failed(而非整体 Err)。
    /// 仅锁不可用 → Err(整体失败)。
    async fn insert_users_batch(
        &self,
        users: &[PreparedUser],
    ) -> Result<Vec<BatchInsertItem>, UsersStoreError>;

    /// 条件更新(一把写锁内三步,users.rs:475-547):
    /// 1. `demotes_or_disables` 时做 last_admin 判定:目标存在且
    ///    (role='admin' AND is_active=1) 且
    ///    `SELECT COUNT(*) FROM users WHERE role='admin' AND is_active=1`
    ///    <= 1 → 返回 LastAdmin(计数查询失败按 0,旧 unwrap_or(0) 的
    ///    fail-open 口径保留,users.rs:85-89);
    /// 2. 按 Some 字段拼 SET(display_name / password_hash 附
    ///    `token_version = token_version + 1` / is_active / role),恒附
    ///    `updated_at = datetime('now')`,执行 UPDATE;影响行数 0 → NotFound;
    /// 3. 回读整行(`find_user` 同列)返回 Updated(新版返回资源)。
    async fn update_user(
        &self,
        id: &str,
        patch: &UserPatch,
    ) -> Result<UpdateOutcome, UsersStoreError>;

    /// 软禁用(一把写锁内先 last_admin 判定,同上):`UPDATE users SET
    /// is_active = 0, updated_at = datetime('now') WHERE id = ?`
    /// (users.rs:565-578);影响行数 0 → NotFound。
    async fn disable_user(&self, id: &str) -> Result<DisableOutcome, UsersStoreError>;

    /// Argon2 哈希(CPU 密集;impl 侧已派发 blocking 线程池)。
    /// Err = 哈希失败(旧 hash_failed)。unit Err 是刻意的:失败细节只进
    /// 日志不进响应(与 AuthStore::find_login_user 同口径)。
    #[allow(clippy::result_unit_err)]
    async fn hash_password(&self, password: &str) -> Result<String, ()>;

    /// 随机密码(批量创建缺省密码时生成,仅回显一次)。
    fn random_password(&self) -> String;
}

// ---------- 服务 ----------

/// 列表分页:默认 50,clamp 1..=200(api-mapping 全局语义 #3;生成器只保证
/// default,不夹范围,clamp 在本层)。
const DEFAULT_LIMIT: i64 = 50;
const MIN_LIMIT: i64 = 1;
const MAX_LIMIT: i64 = 200;

/// users 方法组服务。端口以 trait 对象注入:`AdminService`(lib.rs)的
/// 字段类型是本类型,不能带泛型参数——这是与 AuthService<S>(独享 trait,
/// 可泛型)的差别,见 playbook §〇"Admin tag 注意"。
#[derive(Clone)]
pub struct AdminUsersService {
    store: Arc<dyn AdminUsersStore>,
}

impl AdminUsersService {
    pub fn new(store: Arc<dyn AdminUsersStore>) -> Self {
        Self { store }
    }

    /// 用户列表。
    ///
    /// `GET /api/users`(旧 `GET /api/admin/users`)。
    pub async fn admin_list_users(
        &self,
        limit: ::std::option::Option<i64>,
        offset: ::std::option::Option<i64>,
    ) -> AdminListUsersResponse {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(MIN_LIMIT, MAX_LIMIT);
        // 负 offset 防御:生成路由已 422 拦截(minimum: 0),这里兜底直调。
        let offset = offset.unwrap_or(0).max(0);
        match self.store.list_users(limit, offset).await {
            Ok((records, total)) => AdminListUsersResponse::Ok(UserWithBalancePage {
                items: records.iter().map(to_user_with_balance).collect(),
                total,
            }),
            // 5xx 缺变体:fail-closed 403 + 真实 code,见文件头。
            Err(e) => AdminListUsersResponse::Forbidden(store_failure(
                "query_failed",
                e,
                "admin_list_users",
            )),
        }
    }

    /// 创建用户。
    ///
    /// `POST /api/users`(旧 `POST /api/admin/users`)。校验与 409 保留;
    /// role 非法值由生成器 422 拦截(schema enum),impl 不可达(旧 400
    /// invalid_role 作废,记 notes)。
    pub async fn admin_create_user(&self, body: CreateUserRequest) -> AdminCreateUserResponse {
        let role = body
            .role
            .map(|r| r.as_str().to_string())
            .unwrap_or_else(|| "user".to_string());
        let username = match validate_username(&body.username) {
            Ok(u) => u,
            Err(p) => return AdminCreateUserResponse::BadRequest(p),
        };
        if let Err(p) = validate_password(&body.password) {
            return AdminCreateUserResponse::BadRequest(p);
        }
        let display_name = match &body.display_name {
            Some(d) => {
                if let Err(p) = validate_display_name(d) {
                    return AdminCreateUserResponse::BadRequest(p);
                }
                d.clone()
            }
            None => username.clone(),
        };

        // Argon2 的 blocking 派发在端口 impl 侧(async 化翻转后服务层
        // 直接 await,语义同旧"哈希在 blocking 线程池")。
        let password = body.password.clone();
        let password_hash = match self.store.hash_password(&password).await {
            Ok(h) => h,
            Err(()) => {
                tracing::error!("admin_create_user: password hashing failed");
                return AdminCreateUserResponse::Forbidden(problem(
                    403,
                    "hash_failed",
                    "密码哈希失败,请稍后重试",
                ));
            }
        };

        match self
            .store
            .insert_user(&NewUser {
                username: username.clone(),
                display_name: display_name.clone(),
                role: role.clone(),
                password_hash,
            })
            .await
        {
            // 出参与旧实现同构:请求值 + 当前时间(不回读,users.rs:203-211)。
            Ok(InsertOutcome::Created(id)) => {
                let now = chrono::Utc::now();
                AdminCreateUserResponse::Created(User {
                    id,
                    username,
                    display_name,
                    role: user_role(&role),
                    is_active: true,
                    created_at: now,
                    updated_at: now,
                })
            }
            Ok(InsertOutcome::UsernameExists) => {
                AdminCreateUserResponse::Conflict(problem(409, "username_exists", "该用户名已存在"))
            }
            Err(e) => AdminCreateUserResponse::Forbidden(store_failure(
                "create_failed",
                e,
                "admin_create_user",
            )),
        }
    }

    /// 批量创建用户(逐项独立成功/失败)。
    ///
    /// `POST /api/users/batch`(旧 `POST /api/admin/users/batch`)。整体 200,
    /// 成败在 per-item 结果;失败项 `error` → `code`。空批次 400 empty_batch
    /// 可达(schema 无 minItems);>100 经生成路由被 maxItems 拦成 422,
    /// impl 的 400 batch_too_large 只守护直调(spec 400 示例与生成器实测
    /// 的差异记 notes,同 playbook 踩坑 #3 一类)。
    pub async fn admin_create_users_batch(
        &self,
        body: BatchCreateUsersRequest,
    ) -> AdminCreateUsersBatchResponse {
        const MAX_BATCH: usize = 100;
        if body.users.is_empty() {
            return AdminCreateUsersBatchResponse::BadRequest(problem(
                400,
                "empty_batch",
                "users must not be empty",
            ));
        }
        if body.users.len() > MAX_BATCH {
            return AdminCreateUsersBatchResponse::BadRequest(problem(
                400,
                "batch_too_large",
                "at most 100 users per batch",
            ));
        }

        // 相位 1:校验 + Argon2(旧 users.rs:336;blocking 派发在端口
        // impl 侧,这里顺序 await)。
        let (prepared, mut results, prepared_result_idx) =
            prepare_batch_users(self.store.as_ref(), body.users).await;

        // 相位 2:一把写锁内逐条插入,单项失败不影响其余(旧 users.rs:349)。
        let outcomes = match self.store.insert_users_batch(&prepared).await {
            Ok(v) => v,
            Err(e) => {
                return AdminCreateUsersBatchResponse::Forbidden(store_failure(
                    "create_failed",
                    e,
                    "admin_create_users_batch",
                ));
            }
        };
        debug_assert_eq!(
            outcomes.len(),
            prepared.len(),
            "insert_users_batch 契约:返回与入参等长"
        );
        for ((p, ridx), outcome) in prepared.iter().zip(&prepared_result_idx).zip(outcomes) {
            results[*ridx] = match outcome {
                BatchInsertItem::Created(id) => BatchUserItemResult {
                    username: p.username.clone(),
                    ok: true,
                    id: Some(id),
                    role: Some(p.role.clone()),
                    // 随机生成的密码仅在此回显一次(语义变化:字段仍叫 password)
                    password: p.generated_password.clone(),
                    code: None,
                    message: None,
                },
                BatchInsertItem::UsernameExists => {
                    batch_item_failure(p.username.clone(), "username_exists")
                }
                BatchInsertItem::Failed => batch_item_failure(p.username.clone(), "create_failed"),
            };
        }

        let created = results.iter().filter(|r| r.ok).count() as i64;
        AdminCreateUsersBatchResponse::Ok(BatchUsersResult {
            created,
            failed: results.len() as i64 - created,
            results,
        })
    }

    /// 查单个用户(含余额)。
    ///
    /// `GET /api/users/{id}`(旧 `GET /api/admin/users/{id}`)。查询失败与
    /// 不存在同走 404(旧 `.ok()` 语义);锁不可用 fail-closed(文件头)。
    pub async fn admin_get_user(&self, id: String) -> AdminGetUserResponse {
        match self.store.find_user(&id).await {
            Ok(Some(rec)) => AdminGetUserResponse::Ok(to_user_with_balance(&rec)),
            Ok(None) => AdminGetUserResponse::NotFound(problem(404, "not_found", "用户不存在")),
            Err(e) => {
                AdminGetUserResponse::Forbidden(store_failure("query_failed", e, "admin_get_user"))
            }
        }
    }

    /// 部分更新用户(改密/启停/改角色)。
    ///
    /// `PATCH /api/users/{id}`(旧 `PATCH /api/admin/users/{id}`)。成功返回
    /// 更新后的 User 资源(语义变化 #6);last_admin 400 → 409(#8);改密
    /// token_version+1 保留。空 body 经生成路由被 minProperties 拦成 422;
    /// 仅含未知字段的 body 走到 impl 的 400 no_fields_to_update。
    pub async fn admin_update_user(
        &self,
        id: String,
        body: UpdateUserRequest,
    ) -> AdminUpdateUserResponse {
        // 校验顺序与旧实现一致:password → display_name(users.rs:449-458;
        // role 非法值已由生成器 422 拦截)。
        if let Some(ref password) = body.password
            && let Err(p) = validate_password(password)
        {
            return AdminUpdateUserResponse::BadRequest(p);
        }
        if let Some(ref name) = body.display_name
            && let Err(p) = validate_display_name(name)
        {
            return AdminUpdateUserResponse::BadRequest(p);
        }

        // 改密哈希(blocking 派发在端口 impl 侧;旧 users.rs:461)。
        let password_hash = match body.password.clone() {
            Some(password) => match self.store.hash_password(&password).await {
                Ok(h) => Some(h),
                Err(()) => {
                    tracing::error!("admin_update_user: password hashing failed");
                    return AdminUpdateUserResponse::Forbidden(problem(
                        403,
                        "hash_failed",
                        "密码哈希失败,请稍后重试",
                    ));
                }
            },
            None => None,
        };

        let demotes_or_disables = body.role.as_ref().is_some_and(|r| r.as_str() != "admin")
            || body.is_active == Some(false);
        let patch = UserPatch {
            display_name: body.display_name.clone(),
            password_hash,
            is_active: body.is_active,
            role: body.role.map(|r| r.as_str().to_string()),
            // 降级(role 离开 admin)或禁用触发 last_admin 保护(users.rs:486)。
            demotes_or_disables,
        };
        if patch.display_name.is_none()
            && patch.password_hash.is_none()
            && patch.is_active.is_none()
            && patch.role.is_none()
        {
            return AdminUpdateUserResponse::BadRequest(problem(
                400,
                "no_fields_to_update",
                "no fields to update",
            ));
        }

        match self.store.update_user(&id, &patch).await {
            Ok(UpdateOutcome::Updated(rec)) => AdminUpdateUserResponse::Ok(to_api_user(&rec)),
            Ok(UpdateOutcome::NotFound) => {
                AdminUpdateUserResponse::NotFound(problem(404, "not_found", "用户不存在"))
            }
            Ok(UpdateOutcome::LastAdmin) => AdminUpdateUserResponse::Conflict(problem(
                409,
                "last_admin",
                "不能禁用或降级最后一个活跃管理员",
            )),
            Err(e) => AdminUpdateUserResponse::Forbidden(store_failure(
                "update_failed",
                e,
                "admin_update_user",
            )),
        }
    }

    /// 禁用用户(软删除)。
    ///
    /// `DELETE /api/users/{id}`(旧 `DELETE /api/admin/users/{id}`)。软禁用
    /// 保留,不提供物理删除;200 `{message}` → 204(语义变化 #4);
    /// last_admin 409。
    pub async fn admin_delete_user(&self, id: String) -> AdminDeleteUserResponse {
        match self.store.disable_user(&id).await {
            Ok(DisableOutcome::Disabled) => AdminDeleteUserResponse::NoContent,
            Ok(DisableOutcome::NotFound) => {
                AdminDeleteUserResponse::NotFound(problem(404, "not_found", "用户不存在"))
            }
            Ok(DisableOutcome::LastAdmin) => AdminDeleteUserResponse::Conflict(problem(
                409,
                "last_admin",
                "不能禁用或降级最后一个活跃管理员",
            )),
            Err(e) => AdminDeleteUserResponse::Forbidden(store_failure(
                "delete_failed",
                e,
                "admin_delete_user",
            )),
        }
    }
}

// ---------- 业务助手 ----------

/// username:trim 后 1-64 字符,返回 trim 后的值(旧 validate_username,
/// users.rs:38)。schema 的 minLength 只验原文长度,trim 语义只能在这层。
// Problem(~192B)是响应变体的既定载荷类型,包装只会把解包挪到调用点。
#[allow(clippy::result_large_err)]
fn validate_username(username: &str) -> Result<String, Problem> {
    let u = username.trim();
    if u.is_empty() || u.chars().count() > 64 {
        return Err(problem(
            400,
            "invalid_username",
            "username must be 1-64 characters",
        ));
    }
    Ok(u.to_string())
}

/// 密码:8-256 字节且非全空白(旧 validate_password,users.rs:50)。
/// schema 对 password 无长度约束(只在描述里),校验保留在 impl。
#[allow(clippy::result_large_err)]
fn validate_password(password: &str) -> Result<(), Problem> {
    if password.len() < 8 || password.len() > 256 || password.trim().is_empty() {
        return Err(problem(
            400,
            "invalid_password",
            "password must be 8-256 bytes and not all whitespace",
        ));
    }
    Ok(())
}

/// display_name:≤64 字符,允许空串(旧 validate_display_name,users.rs:61)。
#[allow(clippy::result_large_err)]
fn validate_display_name(name: &str) -> Result<(), Problem> {
    if name.chars().count() > 64 {
        return Err(problem(
            400,
            "invalid_display_name",
            "display_name must be at most 64 characters",
        ));
    }
    Ok(())
}

/// 库内角色串 → 出参枚举(未知值兜底 User;DB CHECK 约束保证只有两种)。
fn user_role(role: &str) -> UserRole {
    match role {
        "admin" => UserRole::Admin,
        _ => UserRole::User,
    }
}

/// 微元 → 元。与二进制 `billing::micro_to_yuan` 同式(aikun/src/billing/
/// mod.rs:20;1e6 整除在 f64 下精确)。aikun-admin 不引入存储侧模块,
/// 本地同式实现,口径注释互相指向。
fn micro_to_yuan(micro: i64) -> f64 {
    micro as f64 / 1_000_000.0
}

/// 记录 → UserWithBalance(时间统一 RFC3339,语义变化 #9)。
fn to_user_with_balance(rec: &AdminUserRecord) -> UserWithBalance {
    UserWithBalance {
        balance: micro_to_yuan(rec.balance_micro),
        created_at: crate::sqlite_utc(&rec.created_at),
        display_name: rec.display_name.clone(),
        id: rec.id.clone(),
        is_active: rec.is_active,
        role: user_role(&rec.role),
        updated_at: crate::sqlite_utc(&rec.updated_at),
        username: rec.username.clone(),
    }
}

/// 记录 → User(PATCH 成功回资源用;不含 balance)。
fn to_api_user(rec: &AdminUserRecord) -> User {
    User {
        id: rec.id.clone(),
        username: rec.username.clone(),
        display_name: rec.display_name.clone(),
        role: user_role(&rec.role),
        is_active: rec.is_active,
        created_at: crate::sqlite_utc(&rec.created_at),
        updated_at: crate::sqlite_utc(&rec.updated_at),
    }
}

/// 批量逐项失败结果(旧 {username, ok:false, error} → code,语义变化 #2/表)。
fn batch_item_failure(username: String, code: &str) -> BatchUserItemResult {
    BatchUserItemResult {
        username,
        ok: false,
        id: None,
        role: None,
        password: None,
        code: Some(code.to_string()),
        message: None,
    }
}

/// 内部失败 → fail-closed 403 + 真实 500 词表 code(枚举无 5xx 变体,
/// 见文件头"已知 spec 缺口";调用点集中在此,拍板补 5xx 后一处换)。
fn store_failure(op_code: &str, e: UsersStoreError, op: &str) -> Problem {
    let code = match e {
        UsersStoreError::Unavailable => "internal_error",
        UsersStoreError::Failed => op_code,
    };
    tracing::error!("admin_users::{op}: store failure ({code})");
    problem(403, code, "存储暂时不可用,请稍后重试或查看服务日志")
}

/// 批量相位 1:逐项校验 + Argon2 哈希(整个函数跑在 blocking 线程池,
/// 旧 prepare_batch_users,users.rs:243)。校验规则与单个创建同一套,
/// 批量不放弱账号进来;缺省密码随机生成并记入 `generated_password`
/// (仅回显一次)。返回 (待插入项, 逐项结果占位向量, 待插入项在结果
/// 向量中的下标)。
async fn prepare_batch_users(
    store: &dyn AdminUsersStore,
    users: Vec<BatchCreateUserItem>,
) -> (Vec<PreparedUser>, Vec<BatchUserItemResult>, Vec<usize>) {
    let mut prepared: Vec<PreparedUser> = Vec::with_capacity(users.len());
    let mut results: Vec<BatchUserItemResult> = Vec::with_capacity(users.len());
    let mut prepared_result_idx: Vec<usize> = Vec::with_capacity(users.len());

    for item in users {
        let username = item.username.trim().to_string();
        let role = item
            .role
            .map(|r| r.as_str().to_string())
            .unwrap_or_else(|| "user".to_string());
        if username.is_empty() || username.chars().count() > 64 {
            results.push(batch_item_failure(username, "invalid_username"));
            continue;
        }
        let display_name = item.display_name.unwrap_or_else(|| username.clone());
        if display_name.chars().count() > 64 {
            results.push(batch_item_failure(username, "invalid_display_name"));
            continue;
        }
        let (password, generated) = match item.password {
            Some(p) if !p.is_empty() => {
                if p.len() < 8 || p.len() > 256 || p.trim().is_empty() {
                    results.push(batch_item_failure(username, "invalid_password"));
                    continue;
                }
                (p, None)
            }
            _ => {
                let p = store.random_password();
                (p.clone(), Some(p))
            }
        };
        let password_hash = match store.hash_password(&password).await {
            Ok(h) => h,
            Err(_) => {
                tracing::error!("admin_create_users_batch: hash failed for {username}");
                results.push(batch_item_failure(username, "hash_failed"));
                continue;
            }
        };
        prepared_result_idx.push(results.len());
        // 占位:username 先落位,相位 2 回填 id/role/password 或失败码。
        results.push(BatchUserItemResult {
            username: username.clone(),
            ok: true,
            id: None,
            role: None,
            password: None,
            code: None,
            message: None,
        });
        prepared.push(PreparedUser {
            username,
            display_name,
            role,
            password_hash,
            generated_password: generated,
        });
    }

    (prepared, results, prepared_result_idx)
}
