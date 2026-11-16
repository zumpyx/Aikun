//! Auth tag 实现(迁移试点批次,2026-11-16):login / serveFrontend /
//! serveStaticAsset。业务逻辑迁自 `aikun/src/api/auth.rs::login` 与
//! `aikun/src/main.rs::{serve_frontend, serve_static}`,对齐 `openapi.yaml`
//! 新契约(路径 `/api/auth/login`、problem+json、429 补 `Retry-After`)。
//!
//! # 装配契约(main.rs 侧,见 docs/migration-playbook.md)
//!
//! * **IP 维度限流在中间件**:生成的 trait 方法签名只有 body,拿不到对端
//!   地址;`login_ip_guard` 由 main.rs 用 `from_fn_with_state` 挂在 auth
//!   路由上,与 impl 内的用户名维度共享同一个 `login_attempts` 窗口表。
//! * **`/` 与单段 `/static/{path}`** 走生成路由;响应头(CSP / no-cache /
//!   charset)由 `frontend_header_fixup` 归一——生成响应枚举只能定
//!   content-type,表达不了自定义头。
//! * **嵌套 `/static/js/*.js` 生成路由匹配不到**:matchit 的 `{path}` 不含
//!   斜杠(实测,见 playbook 踩坑)。main.rs 把 `static_fallback` 挂成应用
//!   级 fallback 兜底;两条路最终都查同一张 `STATIC_ASSETS` 表。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use aikun_openapi::openapi::server::api::AuthApi;
use aikun_openapi::openapi::server::errors::{
    LoginServerResponse, LoginServerResponseTooManyRequestsHeaders, ServeFrontendResponse,
    ServeStaticAssetResponse,
};
use aikun_openapi::openapi::types::{LoginRequest, LoginResponse, User, UserRole};

use crate::problem;

// ---------- 内嵌前端(自 main.rs 迁入,资产清单与行为不变) ----------

/// Frontend assets, embedded into the binary at compile time — the
/// executable is fully self-contained and reads no static files at runtime.
/// The HTML shell references these under /static/*.
const INDEX_HTML: &str = include_str!("../../../aikun/src/frontend/index.html");

/// 前端全自托管、无外链脚本,CSP 收敛为仅允许自身来源。
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'";

/// 旧路径写死的 no-cache 头(迁移期逐字保留,含 pragma)。
const NO_CACHE: &str = "no-cache, no-store, must-revalidate";

/// (path, content) for every embedded frontend asset.
const STATIC_ASSETS: &[(&str, &str)] = &[
    (
        "style.css",
        include_str!("../../../aikun/src/frontend/style.css"),
    ),
    (
        "js/core.js",
        include_str!("../../../aikun/src/frontend/js/core.js"),
    ),
    (
        "js/dashboard.js",
        include_str!("../../../aikun/src/frontend/js/dashboard.js"),
    ),
    (
        "js/wallet.js",
        include_str!("../../../aikun/src/frontend/js/wallet.js"),
    ),
    (
        "js/providers.js",
        include_str!("../../../aikun/src/frontend/js/providers.js"),
    ),
    (
        "js/users.js",
        include_str!("../../../aikun/src/frontend/js/users.js"),
    ),
    (
        "js/billing.js",
        include_str!("../../../aikun/src/frontend/js/billing.js"),
    ),
    (
        "js/redemption.js",
        include_str!("../../../aikun/src/frontend/js/redemption.js"),
    ),
    (
        "js/keys.js",
        include_str!("../../../aikun/src/frontend/js/keys.js"),
    ),
    (
        "js/models.js",
        include_str!("../../../aikun/src/frontend/js/models.js"),
    ),
    (
        "js/test.js",
        include_str!("../../../aikun/src/frontend/js/test.js"),
    ),
    (
        "js/logs.js",
        include_str!("../../../aikun/src/frontend/js/logs.js"),
    ),
    (
        "js/docs.js",
        include_str!("../../../aikun/src/frontend/js/docs.js"),
    ),
    (
        "js/init.js",
        include_str!("../../../aikun/src/frontend/js/init.js"),
    ),
];

/// 按资产路径扩展名给 mime(表内只有 css/js 两类,保持旧实现口径)。
fn asset_mime(path: &str) -> &'static str {
    if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else {
        "text/javascript; charset=utf-8"
    }
}

// ---------- 过渡期存储端口 ----------
//
// aikun-store 就位前(plan 步骤 8)aikun-admin 不直接依赖 rusqlite /
// jsonwebtoken / argon2:DB 查询、Argon2 验密、JWT 签发由二进制装配层
// (main.rs 的适配器)实现本端口。登录的业务编排(防枚举时序均衡、失败
// 记账、状态码映射、限流窗口)全部在本 crate,端口只是数据/加密通道。

/// 登录所需的用户行(password_hash / token_version 之外即 User 出参字段)。
pub struct LoginUserRecord {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    pub display_name: String,
    pub role: String,
    pub is_active: bool,
    /// SQLite datetime 原文(`YYYY-MM-DD HH:MM:SS`),出参时经 `sqlite_utc` 转换。
    pub created_at: String,
    pub updated_at: String,
    pub token_version: i64,
}

/// 过渡期登录端口(装配层实现,复用二进制 crate 的 db/auth 模块)。
/// unit Err 是刻意的:错误细节不进登录路径(防泄漏),可观测性走
/// `tracing::error!`。
#[allow(clippy::result_unit_err)]
pub trait AuthStore: Send + Sync + 'static {
    /// Err(()) = 存储不可用(如连接锁中毒)——旧实现此处回 500,但生成
    /// 响应枚举无 5xx 变体,登录路径按 fail-closed 401 处理并打 error 日志
    /// (spec 缺口已记 api-mapping/playbook,待人工裁决)。
    /// 查询错误与"用户不存在"一律 Ok(None)(旧实现 `.ok()` 语义,统一 401)。
    fn find_login_user(&self, username: &str) -> Result<Option<LoginUserRecord>, ()>;
    /// Argon2 校验(CPU 密集;由 impl 侧派发到 blocking 线程池)。
    fn verify_password(&self, password: &str, hash: &str) -> bool;
    /// 账号枚举防护:未知用户名也跑一次的 dummy PHC hash。
    fn dummy_password_hash(&self) -> &'static str;
    /// 签发 JWT(sub/username/role/token_version);Err → fail-closed 401。
    fn create_token(&self, user: &LoginUserRecord) -> Result<String, ()>;
}

// ---------- 服务 ----------

/// Sliding-window login rate limiter (lives in AppState).
/// Per-username limit stops targeted brute-force; per-IP limit stops
/// spraying across usernames.
const LOGIN_WINDOW: Duration = Duration::from_secs(600);
const MAX_FAILS_PER_USER: usize = 5;
const MAX_FAILS_PER_IP: usize = 30;

/// Auth tag 服务。`attempts` 与二进制 `AppState.login_attempts` 是同一个
/// 实例:ip 维度(`login_ip_guard` 中间件)与 user 维度(impl)共享窗口表,
/// 成功登录只清 user 键、保留 ip 键的语义才成立。
pub struct AuthService<S: AuthStore> {
    store: Arc<S>,
    attempts: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
    trust_x_forwarded_for: bool,
}

impl<S: AuthStore> Clone for AuthService<S> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            attempts: Arc::clone(&self.attempts),
            trust_x_forwarded_for: self.trust_x_forwarded_for,
        }
    }
}

/// 锁中毒时取回内部数据继续工作,不静默 fail-open/fail-closed。
fn lock_login_attempts(
    attempts: &Arc<Mutex<HashMap<String, Vec<Instant>>>>,
) -> MutexGuard<'_, HashMap<String, Vec<Instant>>> {
    attempts.lock().unwrap_or_else(|e| e.into_inner())
}

impl<S: AuthStore> AuthService<S> {
    pub fn new(
        store: Arc<S>,
        attempts: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
        trust_x_forwarded_for: bool,
    ) -> Self {
        Self {
            store,
            attempts,
            trust_x_forwarded_for,
        }
    }

    /// True if either key has exhausted its failure budget within the window.
    /// 顺手清理过期条目(机会式)。
    fn limited(&self, keys: &[String]) -> bool {
        let now = Instant::now();
        let mut attempts = lock_login_attempts(&self.attempts);
        attempts.retain(|_, v| {
            v.retain(|t| now.duration_since(*t) < LOGIN_WINDOW);
            !v.is_empty()
        });
        keys.iter().any(|k| {
            let limit = if k.starts_with("ip:") {
                MAX_FAILS_PER_IP
            } else {
                MAX_FAILS_PER_USER
            };
            attempts.get(k).is_some_and(|v| v.len() >= limit)
        })
    }

    fn record_failure(&self, keys: &[String]) {
        let mut attempts = lock_login_attempts(&self.attempts);
        let now = Instant::now();
        for k in keys {
            attempts.entry(k.clone()).or_default().push(now);
        }
    }

    fn clear_failures(&self, keys: &[String]) {
        let mut attempts = lock_login_attempts(&self.attempts);
        for k in keys {
            attempts.remove(k);
        }
    }

    /// 429 的 `Retry-After` 建议秒数:窗口内最早一次失败到期为止,向上取
    /// 整、至少 1s(spec 目标态新增,api-mapping 全局语义 #12)。
    fn retry_after_secs(&self, key: &str) -> u64 {
        let now = Instant::now();
        let attempts = lock_login_attempts(&self.attempts);
        attempts
            .get(key)
            .and_then(|v| v.first())
            .map(|oldest| now.duration_since(*oldest))
            .map(|elapsed| {
                let rem = LOGIN_WINDOW.saturating_sub(elapsed);
                rem.as_secs() + u64::from(rem.subsec_nanos() > 0)
            })
            .unwrap_or(1)
            .max(1)
    }

    fn too_many_requests(&self, key: &str) -> LoginServerResponse {
        LoginServerResponse::TooManyRequests(
            problem(
                429,
                "rate_limited",
                "Too many failed login attempts, try again later",
            ),
            LoginServerResponseTooManyRequestsHeaders {
                retry_after: Some(self.retry_after_secs(key).to_string()),
            },
        )
    }

    /// 登录业务主体(迁自 `aikun/src/api/auth.rs::login`,编排顺序保持):
    /// 限流(user 维度)→ 查库+验密(防枚举时序均衡)→ 401 记账 →
    /// 密码通过后查禁用(403,防探测)→ 签发 JWT(清 user 键、保留 ip 键)。
    pub async fn do_login(&self, body: LoginRequest) -> LoginServerResponse {
        let user_key = format!("user:{}", body.username.to_lowercase());
        if self.limited(std::slice::from_ref(&user_key)) {
            return self.too_many_requests(&user_key);
        }

        let found = self.store.find_login_user(&body.username);

        // Verify the password regardless of whether the user exists, so unknown
        // usernames cost the same Argon2 run (no timing oracle). Argon2 is
        // CPU-bound, so it runs on the blocking thread pool.
        let (user, password_ok) = match found {
            Ok(Some(rec)) => {
                let store = Arc::clone(&self.store);
                let password = body.password.clone();
                let hash = rec.password_hash.clone();
                let ok =
                    tokio::task::spawn_blocking(move || store.verify_password(&password, &hash))
                        .await
                        .unwrap_or(false);
                (Some(rec), ok)
            }
            Ok(None) => {
                let store = Arc::clone(&self.store);
                let password = body.password.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = store.verify_password(&password, store.dummy_password_hash());
                })
                .await;
                (None, false)
            }
            // 存储不可用:旧实现回 500,但 LoginServerResponse 无 5xx 变体
            // (spec 缺口,见文件头),按 fail-closed 401 兜底并留痕。
            Err(()) => {
                tracing::error!("login: login store unavailable (db lock poisoned?)");
                return LoginServerResponse::Unauthorized(problem(
                    401,
                    "invalid_credentials",
                    "Invalid username or password",
                ));
            }
        };

        let unauthorized = || {
            LoginServerResponse::Unauthorized(problem(
                401,
                "invalid_credentials",
                "Invalid username or password",
            ))
        };

        // 凭证错误统一 401,不区分"用户不存在"与"密码错误"(ip 维度由
        // login_ip_guard 中间件按响应状态记账)。
        let rec = match (user, password_ok) {
            (Some(rec), true) => rec,
            _ => {
                self.record_failure(&[user_key]);
                return unauthorized();
            }
        };

        // Check account status only after the password has passed, so disabled
        // state cannot be probed without valid credentials. 403 不计失败
        // (旧实现同)。
        if !rec.is_active {
            return LoginServerResponse::Forbidden(problem(
                403,
                "account_disabled",
                "This account has been disabled",
            ));
        }

        match self.store.create_token(&rec) {
            Ok(token) => {
                // 只清该用户的失败记录;ip: 键保留,防止借成功登录重置 IP 限额。
                self.clear_failures(&[user_key]);
                LoginServerResponse::Ok(LoginResponse {
                    token,
                    user: to_api_user(&rec),
                })
            }
            Err(()) => {
                tracing::error!("login: token creation failed for user {}", rec.username);
                unauthorized()
            }
        }
    }

    /// 全保真静态资产响应(状态 + CSP/no-cache 头 + mime + body / 404 纯文本)。
    /// 生成路由的枚举路径表达不了这些头,嵌套路径又到不了生成路由——
    /// 装配层 fallback 直接调本方法,是静态资产响应的真源。
    pub fn asset_response(&self, path: &str) -> Response {
        match STATIC_ASSETS.iter().find(|(name, _)| *name == path) {
            Some((_, content)) => (
                StatusCode::OK,
                [
                    ("content-type", asset_mime(path)),
                    ("cache-control", NO_CACHE),
                    ("content-security-policy", CSP),
                ],
                *content,
            )
                .into_response(),
            // 白名单外一律 404 纯文本 'not found'(非 JSON),不触文件系统。
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        }
    }

    /// 应用级 fallback:生成路由 `/static/{path}` 只匹配单段路径,
    /// `/static/js/core.js` 这类嵌套资产在此兜底;非 /static/ 前缀的
    /// 未匹配路径维持 axum 默认空 404(与迁移前一致)。
    pub async fn static_fallback(&self, req: Request) -> Response {
        let path = req.uri().path();
        match path.strip_prefix("/static/") {
            Some(rest) if !rest.is_empty() => self.asset_response(rest),
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }
}

/// 登录响应的 User 出参映射(role 词表收敛;时间统一 RFC3339)。
fn to_api_user(rec: &LoginUserRecord) -> User {
    User {
        id: rec.id.clone(),
        username: rec.username.clone(),
        display_name: rec.display_name.clone(),
        role: match rec.role.as_str() {
            "admin" => UserRole::Admin,
            _ => UserRole::User,
        },
        is_active: rec.is_active,
        created_at: crate::sqlite_utc(&rec.created_at),
        updated_at: crate::sqlite_utc(&rec.updated_at),
    }
}

// ---------- trait 实现(生成路由的入口) ----------

#[async_trait::async_trait]
impl<S: AuthStore> AuthApi for AuthService<S> {
    async fn login(&self, body: LoginRequest) -> LoginServerResponse {
        self.do_login(body).await
    }

    async fn serve_frontend(&self) -> ServeFrontendResponse {
        // CSP / no-cache 头由装配层的 frontend_header_fixup 归一
        //(生成枚举只能表达 content-type)。
        ServeFrontendResponse::Ok(INDEX_HTML.to_string())
    }

    async fn serve_static_asset(&self, path: String) -> ServeStaticAssetResponse {
        match STATIC_ASSETS.iter().find(|(name, _)| *name == path) {
            Some((_, content)) => ServeStaticAssetResponse::Ok((*content).to_string()),
            // 404 的 'not found' 纯文本 body 同样由 frontend_header_fixup 补
            //(枚举 NotFound 变体无 body 位)。
            None => ServeStaticAssetResponse::NotFound,
        }
    }
}

// ---------- 装配层中间件 ----------

/// 登录限流用的客户端 IP:配置开启 trust_x_forwarded_for 时取
/// X-Forwarded-For 首个 IP(部署在可信反代之后),否则用对端 socket 地址。
/// (迁自 `aikun/src/api/auth.rs::client_ip`,行为不变。)
fn client_ip(headers: &HeaderMap, addr: &SocketAddr, trust_xff: bool) -> String {
    if trust_xff
        && let Some(first) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    {
        return first.to_string();
    }
    addr.ip().to_string()
}

/// **IP 维度登录限流中间件**(main.rs 用 `from_fn_with_state` 挂在 auth
/// 路由上)。生成 trait 方法只收 body,拿不到对端地址,IP 维度只能在这里:
/// * 进门先查 `ip:<addr>` 窗口(30 次/10 分钟)→ 超限直接 429 problem+json
///   + Retry-After(与 impl 侧 user 维度同形);
/// * 出门看响应:401(凭证错误)才给 ip 键记账——403(禁用,已验密)与
///   429 不计,和旧实现一致。
pub async fn login_ip_guard<S: AuthStore>(
    State(svc): State<AuthService<S>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if req.method() != Method::POST || req.uri().path() != "/api/auth/login" {
        return next.run(req).await;
    }
    let ip_key = format!(
        "ip:{}",
        client_ip(req.headers(), &addr, svc.trust_x_forwarded_for)
    );
    if svc.limited(std::slice::from_ref(&ip_key)) {
        let mut resp = (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(problem(
                429,
                "rate_limited",
                "Too many failed login attempts, try again later",
            )),
        )
            .into_response();
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/problem+json"),
        );
        if let Ok(v) = header::HeaderValue::from_str(&svc.retry_after_secs(&ip_key).to_string()) {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        return resp;
    }
    let resp = next.run(req).await;
    if resp.status() == StatusCode::UNAUTHORIZED {
        svc.record_failure(&[ip_key]);
    }
    resp
}

/// **前端响应头归一中间件**(main.rs 挂在 auth 路由上):
/// `/` 与匹配到生成路由 `/static/{path}` 的响应,CSP / no-cache / charset
/// 头在这里补齐(生成响应枚举只能定 content-type,且静态 Ok 变体恒
/// text/css、NotFound 变体无 body——均按迁移前行为修正)。
pub async fn frontend_header_fixup(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let mut resp = next.run(req).await;
    if path == "/" {
        let headers = resp.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/html; charset=utf-8"),
        );
        headers.insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static(NO_CACHE),
        );
        headers.insert(header::PRAGMA, header::HeaderValue::from_static("no-cache"));
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            header::HeaderValue::from_static(CSP),
        );
        return resp;
    }
    if let Some(rest) = path.strip_prefix("/static/") {
        if resp.status() == StatusCode::NOT_FOUND {
            // 枚举 NotFound 变体无 body 位;spec 要求纯文本 'not found'。
            return (StatusCode::NOT_FOUND, "not found").into_response();
        }
        let headers = resp.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static(asset_mime(rest)),
        );
        headers.insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static(NO_CACHE),
        );
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            header::HeaderValue::from_static(CSP),
        );
    }
    resp
}
