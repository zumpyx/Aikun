mod api;
mod auth;
mod billing;
mod cache;
mod channel;
mod config;
mod crypto;
mod db;
mod models;
mod pipeline;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    Json, Router, middleware,
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::{Value, json};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::auth::{auth_middleware, require_admin};
use crate::config::AppConfig;
use crate::db::DbPool;

/// Shared application state — all handlers receive this via `State<AppState>`.
#[derive(Clone)]
pub struct AppState {
    pub pool: Arc<DbPool>,
    pub config: Arc<AppConfig>,
    /// Cached reqwest clients keyed by "proxy_url|timeout_secs" — building a
    /// client per request is expensive and defeats connection pooling.
    pub clients: Arc<Mutex<HashMap<String, reqwest::Client>>>,
    /// Sliding-window login-attempt tracker keyed by "ip:..." / "user:..." —
    /// brute-force protection for POST /api/auth/login(ip 维度在装配层
    /// 中间件、user 维度在 crates/aikun-admin/src/auth.rs,共享本表)。
    pub login_attempts: Arc<Mutex<HashMap<String, Vec<std::time::Instant>>>>,
    /// Sliding-window per-API-key request timestamps (keyed by api_keys.id) —
    /// enforces api_keys.rate_limit_rpm. In-memory only: a restart resets the
    /// window, which is acceptable for a per-minute limit.
    pub api_key_rate: Arc<Mutex<HashMap<String, Vec<std::time::Instant>>>>,
    /// 兑换码失败滑动窗口(keyed by user id):防爆破,10 分钟 5 次失败即 429。
    pub redeem_attempts: Arc<Mutex<HashMap<String, Vec<std::time::Instant>>>>,
    /// API key 并发在途计数(keyed by api_keys.id):max_concurrent 的执行层。
    /// 请求结束时由 ConcurrencyGuard 的 Drop 归还槽位;内存口径,重启清零。
    pub api_key_inflight: Arc<Mutex<HashMap<String, usize>>>,
    /// 关停信号:收到 SIGINT/SIGTERM 后取消,流式生成器据此主动收尾——
    /// 已捕获 usage 照常记账,而不是被 30s 强杀跳过 Drop 丢账。
    pub shutdown: tokio_util::sync::CancellationToken,
    /// 响应缓存(默认 off;后端默认进程内内存)。
    pub cache: Arc<cache::GatewayCache>,
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aikun=info,tower_http=info".into()),
        )
        .init();

    let config = Arc::new(AppConfig::load());
    info!("Starting Aikun on {}", config.host);

    if config.jwt_secret == AppConfig::default().jwt_secret {
        // 默认 secret 是公开源码里的常量,任何人都能伪造 admin JWT,
        // 只告警等于放行,直接拒绝启动。
        panic!(
            "AIKUN_JWT_SECRET is not set (or still the built-in default). \
             Set the AIKUN_JWT_SECRET environment variable to a long random string before starting."
        );
    }
    if config.jwt_secret.len() < 32 {
        // 弱 secret 可被爆破:它既是 HS256 签名密钥,缺省时又派生渠道 key
        // 的 AES 加密密钥,双重暴露面,拒绝启动。
        panic!(
            "AIKUN_JWT_SECRET is too short ({} chars, need at least 32). \
             Generate one with: openssl rand -hex 32",
            config.jwt_secret.len()
        );
    }

    let pool = Arc::new(DbPool::new(&config).expect("Failed to initialize database"));
    info!("Database initialized");

    // 运维逃生口:设置 AIKUN_RESET_ADMIN_PASSWORD 后,启动即重置 admin 密码
    // 并 token_version + 1(旧会话全部失效)。密码本身绝不打印到日志。
    if let Ok(new_password) = std::env::var("AIKUN_RESET_ADMIN_PASSWORD") {
        if new_password.is_empty() {
            tracing::warn!("AIKUN_RESET_ADMIN_PASSWORD 为空,忽略密码重置");
        } else {
            match crate::auth::hash_password(&new_password) {
                Ok(hash) => match pool.conn.lock() {
                    Ok(conn) => match crate::db::reset_admin_password(&conn, &hash) {
                        Ok(true) => info!("管理员密码已通过环境变量重置,请尽快修改并移除该变量"),
                        Ok(false) => tracing::warn!(
                            "AIKUN_RESET_ADMIN_PASSWORD 已设置,但库中不存在 admin 用户"
                        ),
                        Err(e) => tracing::error!("管理员密码重置失败: {}", e),
                    },
                    Err(_) => tracing::error!("管理员密码重置失败:数据库锁不可用"),
                },
                Err(e) => tracing::error!("管理员密码重置失败(哈希计算): {}", e),
            }
        }
    }

    let state = AppState {
        pool: pool.clone(),
        config: config.clone(),
        clients: Arc::new(Mutex::new(HashMap::new())),
        login_attempts: Arc::new(Mutex::new(HashMap::new())),
        api_key_rate: Arc::new(Mutex::new(HashMap::new())),
        redeem_attempts: Arc::new(Mutex::new(HashMap::new())),
        api_key_inflight: Arc::new(Mutex::new(HashMap::new())),
        shutdown: tokio_util::sync::CancellationToken::new(),
        cache: Arc::new(cache::GatewayCache::from_config(&config)),
    };

    // --- Auth tag(spec-first 新装配,crates/aikun-admin):裸挂,无鉴权中间件 ---
    // login 的 IP 维度限流与前端响应头归一在中间件层 —— 生成 trait 方法
    // 签名只有 body,拿不到对端地址,也表达不了自定义头(详见
    // crates/aikun-admin/src/auth.rs 文件头的装配契约)。
    let auth_service = aikun_admin::auth::AuthService::new(
        std::sync::Arc::new(DbAuthStore {
            pool: pool.clone(),
            config: config.clone(),
        }),
        state.login_attempts.clone(),
        config.trust_x_forwarded_for,
    );
    let auth_routes = aikun_openapi::openapi::server::router::auth_api_router(auth_service.clone())
        .layer(middleware::from_fn_with_state(
            auth_service.clone(),
            aikun_admin::auth::login_ip_guard,
        ))
        .layer(middleware::from_fn(
            aikun_admin::auth::frontend_header_fixup,
        ));

    // --- User tag(spec-first,crates/aikun-admin::user;鉴权分层见
    // crates/aikun-admin/src/user.rs 文件头的装配契约):auth_middleware 验证
    // JWT → claims_to_user_auth 把二进制 Claims 映射为 user::UserAuth 塞回
    // extensions → user_auth_scope 转入 task-local 供 impl 读取。 ---
    let user_service = aikun_admin::user::UserService::new(
        Arc::new(DbUserStore { pool: pool.clone() }),
        state.redeem_attempts.clone(),
        state.api_key_rate.clone(),
        env!("CARGO_PKG_VERSION"),
    );
    let user_routes = aikun_openapi::openapi::server::router::user_api_router(user_service.clone())
        .layer(middleware::from_fn(aikun_admin::user::user_auth_scope))
        .layer(middleware::from_fn(claims_to_user_auth))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // --- Admin tag(providers / users / billing / stats 四方法组经
    // aikun-admin::AdminService 单一 facade 委托;路由工厂只收一个
    // AdminApi 实现者)。与旧 /api/admin/** 同守护:auth_middleware +
    // require_admin;401/403 错误体已归一 problem+json(auth/mod.rs)。 ---
    let admin_service = aikun_admin::AdminService {
        providers: aikun_admin::admin_providers::AdminProvidersService::new(
            Arc::new(DbProviderStore { pool: pool.clone() }),
            Arc::new(GatewayProbes {
                pool: pool.clone(),
                clients: state.clients.clone(),
            }),
        ),
        users: aikun_admin::admin_users::AdminUsersService::new(Arc::new(DbAdminUsersStore {
            pool: pool.clone(),
        })),
        billing: aikun_admin::admin_billing::AdminBillingService::new(Arc::new(DbBillingStore {
            pool: pool.clone(),
        })),
        stats: aikun_admin::admin_stats::AdminStatsService::new(Arc::new(DbStatsStore {
            pool: pool.clone(),
        })),
    };
    let admin_routes =
        aikun_openapi::openapi::server::router::admin_api_router(admin_service.clone())
            .layer(middleware::from_fn(require_admin))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ));

    // --- Public routes (no auth) ---
    // Auth tag 的 /、/static、/api/auth/login 已迁 crates/aikun-admin,
    // 由上方 auth_routes 承担;管理面(User/Admin tag)同样由生成路由承担。
    let public_routes = Router::new().route("/api/health", get(health_check));

    // --- Relay routes /v1/**(JWT 或 API key;OpenAI 兼容面,契约冻结) ---
    let relay_routes = Router::new()
        .route(
            "/v1/chat/completions",
            post(api::proxy::chat::chat_completion),
        )
        .route("/v1/messages", post(api::proxy::messages::messages))
        .route("/v1/responses", post(api::proxy::responses::responses))
        .route("/v1/models", get(api::proxy::chat::list_models))
        // 其余 OpenAI 兼容端点的字节级透传(白名单见 passthrough.rs);
        // 通配路由排在显式路由之后,chat/messages/responses/models 不受影响。
        .route("/v1/{*path}", post(api::proxy::passthrough::passthrough))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // Spawn the health check background task under a supervisor: if the loop
    // ever panics or exits, log it and restart after a short backoff so
    // health tracking cannot silently die.
    let health_pool = pool.clone();
    let health_config = config.clone();
    let health_clients = state.clients.clone();
    tokio::spawn(async move {
        use futures_util::FutureExt;
        loop {
            let result =
                std::panic::AssertUnwindSafe(crate::channel::health::run_health_check_loop(
                    health_pool.clone(),
                    health_config.clone(),
                    health_clients.clone(),
                ))
                .catch_unwind()
                .await;
            match result {
                Ok(()) => {
                    tracing::error!("Health check loop exited unexpectedly — restarting in 5s")
                }
                Err(_) => tracing::error!("Health check loop panicked — restarting in 5s"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });

    // Same supervision for the 30-minute per-model live test loop.
    let model_test_pool = pool.clone();
    let model_test_clients = state.clients.clone();
    tokio::spawn(async move {
        use futures_util::FutureExt;
        loop {
            let result =
                std::panic::AssertUnwindSafe(crate::channel::model_test::run_model_test_loop(
                    model_test_pool.clone(),
                    model_test_clients.clone(),
                ))
                .catch_unwind()
                .await;
            match result {
                Ok(()) => tracing::error!("Model test loop exited unexpectedly — restarting in 5s"),
                Err(_) => tracing::error!("Model test loop panicked — restarting in 5s"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });

    // Combine all routes
    let shutdown_token = state.shutdown.clone();
    // 嵌套静态资产(/static/js/*.js)兜底:生成路由 /static/{path} 只匹配
    // 单段路径(matchit 的 {} 参数不含斜杠),多段路径在此分发到内嵌资产
    // 表;其余未匹配路径维持 axum 默认空 404(与迁移前一致)。
    let static_assets = auth_service.clone();
    let mut app = Router::new()
        .merge(public_routes)
        .merge(relay_routes)
        .merge(admin_routes.with_state(()))
        .merge(user_routes.with_state(()))
        .merge(auth_routes.with_state(()))
        .fallback(move |req: axum::extract::Request| {
            let svc = static_assets.clone();
            async move { svc.static_fallback(req).await }
        })
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // CORS:默认不加层(仅同源);仅当 CORS_ALLOWED_ORIGINS 设置了白名单时放行。
    if !config.cors_allowed_origins.is_empty() {
        let origins: Vec<axum::http::HeaderValue> = config
            .cors_allowed_origins
            .iter()
            .map(|o| {
                o.parse()
                    .unwrap_or_else(|_| panic!("CORS_ALLOWED_ORIGINS 中 {:?} 不是合法的 Origin", o))
            })
            .collect();
        app = app.layer(
            CorsLayer::new()
                .allow_origin(origins)
                .allow_methods(Any)
                .allow_headers(Any),
        );
        info!("CORS 白名单已启用: {:?}", config.cors_allowed_origins);
    }

    let listener = tokio::net::TcpListener::bind(config.host)
        .await
        .expect("Failed to bind address");

    info!("Server listening on {}", config.host);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(shutdown_token))
    .await
    .expect("Server failed");
}

/// Resolve when a shutdown signal arrives: Ctrl-C (SIGINT) or SIGTERM (the
/// default from systemd/docker). Whichever comes first wins.
async fn shutdown_signal(shutdown: tokio_util::sync::CancellationToken) {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!("Failed to install SIGTERM handler: {}", e);
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Shutdown signal (SIGINT) received — draining open connections"),
        _ = terminate => info!("Shutdown signal (SIGTERM) received — draining open connections"),
    }

    // 先取消在途流:流式生成器收到取消后走正常收尾(已捕获 usage 照常
    // 记账),通常秒级完成;30s 上限只是兜底,正常用不到。
    shutdown.cancel();

    // 排空连接最多等 30s,超时强制退出,避免进程被挂起的长连接拖住。
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        tracing::error!("Graceful shutdown timed out after 30s — forcing exit");
        std::process::exit(1);
    });
}

async fn health_check() -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "timestamp": chrono::Utc::now().to_rfc3339()
    }))
}

// ============================================================================
// 过渡期端口适配器(aikun-store 就位后整体收编,plan 步骤 8)
// ============================================================================

/// User tag 中间件:auth_middleware 已验证并把 `crate::auth::Claims` 放进
/// request extensions;此处映射为 `aikun_admin::user::UserAuth` 塞回
/// extensions(生成 trait 方法拿不到请求上下文,这是 claims 进入 impl 的
/// 通道,见 crates/aikun-admin/src/user.rs 文件头的装配契约)。
async fn claims_to_user_auth(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(c) = req.extensions().get::<crate::auth::Claims>().cloned() {
        req.extensions_mut().insert(aikun_admin::user::UserAuth {
            user_id: c.sub,
            username: c.username,
            role: c.role,
        });
    }
    next.run(req).await
}

// ---------- Auth tag 适配器(DbAuthStore,Auth 批已交付) ----------

// aikun-admin 的登录端口(aikun_admin::auth::AuthStore)由二进制的
// DbPool + auth 模块实现,SQL 与哈希/签发逻辑迁自旧 api::auth::login;
// aikun-store 就位后由 store 层收编(plan 步骤 8),届时删除本适配器。
struct DbAuthStore {
    pool: Arc<DbPool>,
    config: Arc<AppConfig>,
}

impl aikun_admin::auth::AuthStore for DbAuthStore {
    /// 查询错误与"用户不存在"一律 Ok(None)(旧实现 `.ok()` 语义,统一
    /// 401 防枚举);连接锁中毒 → Err(())。
    fn find_login_user(
        &self,
        username: &str,
    ) -> Result<Option<aikun_admin::auth::LoginUserRecord>, ()> {
        let conn = self.pool.read().lock().map_err(|_| ())?;
        Ok(conn
            .query_row(
                "SELECT id, username, password_hash, display_name, role, is_active, created_at, updated_at, token_version
                 FROM users WHERE username = ?1",
                rusqlite::params![username],
                |row| {
                    Ok(aikun_admin::auth::LoginUserRecord {
                        id: row.get(0)?,
                        username: row.get(1)?,
                        password_hash: row.get(2)?,
                        display_name: row.get(3)?,
                        role: row.get(4)?,
                        is_active: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                        token_version: row.get(8)?,
                    })
                },
            )
            .ok())
    }

    fn verify_password(&self, password: &str, hash: &str) -> bool {
        crate::auth::verify_password(password, hash).unwrap_or(false)
    }

    fn dummy_password_hash(&self) -> &'static str {
        // A valid Argon2 hash used to equalize response timing when the
        // requested username does not exist (prevents account-enumeration
        // via timing).迁自旧 api::auth::dummy_password_hash。
        static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        DUMMY.get_or_init(|| {
            crate::auth::hash_password("dummy-password").unwrap_or_else(|_| {
                // Valid PHC string for "password"; only used for timing equalization.
                "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$2hV3t0s4rVdXKZ2dKjSZUw".to_string()
            })
        })
    }

    fn create_token(&self, user: &aikun_admin::auth::LoginUserRecord) -> Result<String, ()> {
        crate::auth::create_token(
            &user.id,
            &user.username,
            &user.role,
            user.token_version,
            &self.config,
        )
        .map_err(|_| ())
    }
}

// ---------- User tag 适配器(DbUserStore;SQL 迁自旧 api/auth.rs、
// api/logs.rs、api/redemption.rs::redeem_code) ----------

use aikun_admin::user as user_port;

struct DbUserStore {
    pool: Arc<DbPool>,
}

/// api_keys 行 → 库内视角记录(key 只带掩码后缀;完整哈希永不出端口)。
const API_KEY_COLS: &str =
    "id, user_id, key_suffix, name, is_active, last_used_at, expires_at, models,
                            rate_limit_rpm, quota_daily_tokens, max_concurrent, created_at";

fn row_to_api_key_record(row: &rusqlite::Row) -> rusqlite::Result<user_port::ApiKeyRecord> {
    Ok(user_port::ApiKeyRecord {
        id: row.get(0)?,
        user_id: row.get(1)?,
        key_suffix: row.get(2)?,
        name: row.get(3)?,
        is_active: row.get(4)?,
        last_used_at: row.get(5)?,
        expires_at: row.get(6)?,
        models_json: row.get(7)?,
        rate_limit_rpm: row.get(8)?,
        quota_daily_tokens: row.get(9)?,
        max_concurrent: row.get(10)?,
        created_at: row.get(11)?,
    })
}

/// 日志过滤 → (WHERE 子句, 参数),列名带 l. 前缀(LEFT JOIN 歧义防护)。
/// 迁自旧 api/logs.rs::build_log_filters;success 已是 0/1。
fn user_log_where(
    filters: &user_port::LogFilters,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut conditions = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(uid) = &filters.user_id {
        conditions.push("l.user_id = ?");
        params.push(Box::new(uid.clone()));
    }
    if let Some(model) = &filters.model {
        conditions.push("l.model = ?");
        params.push(Box::new(model.clone()));
    }
    if let Some(s) = filters.success {
        conditions.push("l.success = ?");
        params.push(Box::new(s));
    }
    if let Some(since) = &filters.since {
        conditions.push("l.created_at >= ?");
        params.push(Box::new(since.clone()));
    }
    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {} ", conditions.join(" AND "))
    };
    (where_clause, params)
}

fn row_to_log_record(row: &rusqlite::Row) -> rusqlite::Result<user_port::LogRecord> {
    Ok(user_port::LogRecord {
        id: row.get(0)?,
        user_id: row.get(1)?,
        api_key_id: row.get(2)?,
        provider_id: row.get(3)?,
        username: row.get(16)?,
        provider_name: row.get(17)?,
        model: row.get(4)?,
        request_type: row.get(5)?,
        prompt_tokens: row.get(6)?,
        completion_tokens: row.get(7)?,
        total_tokens: row.get(8)?,
        cached_tokens: row.get(9)?,
        latency_ms: row.get(10)?,
        status_code: row.get(11)?,
        success: row.get(12)?,
        error_message: row.get(13)?,
        cost_micro: row.get(14)?,
        created_at: row.get(15)?,
    })
}

impl user_port::UserStore for DbUserStore {
    fn find_user_with_balance(
        &self,
        user_id: &str,
    ) -> Result<Option<user_port::UserBalanceRecord>, ()> {
        let conn = self.pool.read().lock().map_err(|_| ())?;
        Ok(conn
            .query_row(
                "SELECT id, username, display_name, role, is_active, created_at, updated_at, balance
                 FROM users WHERE id = ?1",
                rusqlite::params![user_id],
                |row| {
                    Ok(user_port::UserBalanceRecord {
                        id: row.get(0)?,
                        username: row.get(1)?,
                        display_name: row.get(2)?,
                        role: row.get(3)?,
                        is_active: row.get(4)?,
                        created_at: row.get(5)?,
                        updated_at: row.get(6)?,
                        balance_micro: row.get(7)?,
                    })
                },
            )
            .ok())
    }

    fn generate_api_key_material(&self) -> (String, String, String, String) {
        let id = uuid::Uuid::new_v4().to_string();
        let key = format!("sk-{:x}{:x}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let key_hash = crate::auth::hash_api_key(&key);
        // 明文后 4 位(掩码展示;库中不落完整明文)
        let key_suffix: String = key.chars().rev().take(4).collect();
        let key_suffix = key_suffix.chars().rev().collect();
        (id, key, key_hash, key_suffix)
    }

    fn insert_api_key(&self, row: &user_port::NewApiKey) -> Result<user_port::ApiKeyRecord, ()> {
        let conn = self.pool.conn.lock().map_err(|_| ())?;
        conn.execute(
            "INSERT INTO api_keys (id, user_id, key, key_suffix, name, expires_at, models, rate_limit_rpm, quota_daily_tokens, max_concurrent)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                row.id,
                row.user_id,
                row.key_hash,
                row.key_suffix,
                row.name,
                row.expires_at,
                row.models_json,
                row.rate_limit_rpm,
                row.quota_daily_tokens,
                row.max_concurrent
            ],
        )
        .map_err(|e| {
            tracing::error!("DbUserStore::insert_api_key: {e}")
        })?;
        conn.query_row(
            &format!("SELECT {API_KEY_COLS} FROM api_keys WHERE id = ?1"),
            rusqlite::params![row.id],
            row_to_api_key_record,
        )
        .map_err(|_| ())
    }

    fn list_api_keys(
        &self,
        target_user_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<user_port::ApiKeyRecord>), ()> {
        let conn = self.pool.read().lock().map_err(|_| ())?;
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM api_keys WHERE user_id = ?1",
                rusqlite::params![target_user_id],
                |r| r.get(0),
            )
            .map_err(|_| ())?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {API_KEY_COLS} FROM api_keys WHERE user_id = ?1
                 ORDER BY created_at DESC LIMIT ?2 OFFSET ?3"
            ))
            .map_err(|_| ())?;
        let rows = stmt
            .query_map(
                rusqlite::params![target_user_id, limit, offset],
                row_to_api_key_record,
            )
            .map_err(|_| ())?;
        Ok((total, rows.filter_map(|r| r.ok()).collect()))
    }

    fn update_api_key(
        &self,
        key_id: &str,
        owner: Option<&str>,
        updates: &user_port::ApiKeyUpdates,
    ) -> Result<Option<user_port::ApiKeyRecord>, ()> {
        let conn = self.pool.conn.lock().map_err(|_| ())?;
        let mut sets = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(name) = &updates.name {
            sets.push("name = ?");
            params.push(Box::new(name.clone()));
        }
        if let Some(active) = updates.is_active {
            sets.push("is_active = ?");
            params.push(Box::new(active as i32));
        }
        if let Some(exp) = &updates.expires_at {
            sets.push("expires_at = ?");
            // Some(None) = 清除(永不过期);Some(Some(s)) = 设置原文。
            params.push(Box::new(exp.clone()));
        }
        if let Some(models_json) = &updates.models_json {
            sets.push("models = ?");
            params.push(Box::new(models_json.clone()));
        }
        if let Some(rpm) = updates.rate_limit_rpm {
            sets.push("rate_limit_rpm = ?");
            params.push(Box::new(rpm));
        }
        if let Some(quota) = updates.quota_daily_tokens {
            sets.push("quota_daily_tokens = ?");
            params.push(Box::new(quota));
        }
        if let Some(mc) = updates.max_concurrent {
            sets.push("max_concurrent = ?");
            params.push(Box::new(mc));
        }
        // 属主过滤:非 admin 限自己的 key,未命中影响行数 0 → Ok(None)。
        let sql = if owner.is_some() {
            format!(
                "UPDATE api_keys SET {} WHERE id = ? AND user_id = ?",
                sets.join(", ")
            )
        } else {
            format!("UPDATE api_keys SET {} WHERE id = ?", sets.join(", "))
        };
        params.push(Box::new(key_id.to_string()));
        if let Some(uid) = owner {
            params.push(Box::new(uid.to_string()));
        }
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let n = conn
            .execute(&sql, refs.as_slice())
            .map_err(|e| tracing::error!("DbUserStore::update_api_key: {e}"))?;
        if n == 0 {
            return Ok(None);
        }
        Ok(conn
            .query_row(
                &format!("SELECT {API_KEY_COLS} FROM api_keys WHERE id = ?1"),
                rusqlite::params![key_id],
                row_to_api_key_record,
            )
            .ok())
    }

    fn delete_api_key(&self, key_id: &str, owner: Option<&str>) -> Result<i64, ()> {
        let conn = self.pool.conn.lock().map_err(|_| ())?;
        let n = if let Some(uid) = owner {
            conn.execute(
                "DELETE FROM api_keys WHERE id = ?1 AND user_id = ?2",
                rusqlite::params![key_id, uid],
            )
        } else {
            conn.execute(
                "DELETE FROM api_keys WHERE id = ?1",
                rusqlite::params![key_id],
            )
        }
        .map_err(|e| tracing::error!("DbUserStore::delete_api_key: {e}"))?;
        Ok(n as i64)
    }

    fn query_logs(
        &self,
        filters: &user_port::LogFilters,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<user_port::LogRecord>), ()> {
        let conn = self.pool.read().lock().map_err(|_| ())?;
        let (where_clause, mut params) = user_log_where(filters);
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let total: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM request_logs l {where_clause}"),
                refs.as_slice(),
                |r| r.get(0),
            )
            .map_err(|e| tracing::error!("DbUserStore::query_logs count: {e}"))?;
        params.push(Box::new(limit));
        params.push(Box::new(offset));
        let sql = format!(
            "SELECT l.id, l.user_id, l.api_key_id, l.provider_id, l.model, l.request_type,
                    l.prompt_tokens, l.completion_tokens, l.total_tokens, l.cached_tokens, l.latency_ms,
                    l.status_code, l.success, l.error_message, l.cost, l.created_at,
                    u.username, p.name
             FROM request_logs l
             LEFT JOIN users u ON u.id = l.user_id
             LEFT JOIN providers p ON p.id = l.provider_id
             {where_clause}ORDER BY l.created_at DESC LIMIT ? OFFSET ?"
        );
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| tracing::error!("DbUserStore::query_logs prepare: {e}"))?;
        let rows = stmt
            .query_map(refs.as_slice(), row_to_log_record)
            .map_err(|e| tracing::error!("DbUserStore::query_logs map: {e}"))?;
        Ok((total, rows.filter_map(|r| r.ok()).collect()))
    }

    fn query_log_stats(
        &self,
        filters: &user_port::LogFilters,
    ) -> Result<user_port::LogStatsRaw, ()> {
        let conn = self.pool.read().lock().map_err(|_| ())?;
        let (where_clause, params) = user_log_where(filters);
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        conn.query_row(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(total_tokens), 0), COALESCE(AVG(latency_ms), 0),
                        COALESCE(SUM(CASE WHEN success = 1 THEN 1 ELSE 0 END) * 100.0 / NULLIF(COUNT(*), 0), 0),
                        COALESCE(SUM(cost), 0)
                 FROM request_logs l {where_clause}"
            ),
            refs.as_slice(),
            |row| {
                Ok(user_port::LogStatsRaw {
                    total_requests: row.get(0)?,
                    total_tokens: row.get(1)?,
                    avg_latency_ms: row.get(2)?,
                    success_rate_pct: row.get(3)?,
                    total_cost_micro: row.get(4)?,
                })
            },
        )
        .map_err(|e| {
            tracing::error!("DbUserStore::query_log_stats: {e}")
        })
    }

    fn wallet_snapshot(
        &self,
        user_id: &str,
        since_date: &str,
    ) -> Result<user_port::WalletSnapshot, ()> {
        let conn = self.pool.read().lock().map_err(|_| ())?;
        let balance: i64 = conn
            .query_row(
                "SELECT balance FROM users WHERE id = ?1",
                rusqlite::params![user_id],
                |r| r.get(0),
            )
            .map_err(|_| ())?;
        // 每日序列:明细(request_logs)与聚合(usage_daily)按日合并
        // (purge 在同一事务里聚合 + 删除,两边相加不重复计数;迁自旧
        // api/logs.rs::wallet_daily)。
        let mut stmt = conn
            .prepare(
                "SELECT d, COALESCE(SUM(c), 0), COALESCE(SUM(t), 0), COALESCE(SUM(r), 0) FROM (
                     SELECT substr(created_at, 1, 10) AS d, SUM(cost) AS c,
                            SUM(total_tokens) AS t, COUNT(*) AS r
                     FROM request_logs
                     WHERE user_id = ?1 AND substr(created_at, 1, 10) >= ?2
                     GROUP BY d
                     UNION ALL
                     SELECT date AS d, cost AS c, tokens AS t, requests AS r
                     FROM usage_daily
                     WHERE user_id = ?1 AND date >= ?2
                 )
                 GROUP BY d ORDER BY d",
            )
            .map_err(|_| ())?;
        let daily = stmt
            .query_map(rusqlite::params![user_id, since_date], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(|_| ())?
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>();
        let mut stmt = conn
            .prepare(
                "SELECT model, COALESCE(SUM(cost), 0) AS c, COUNT(*)
                 FROM request_logs
                 WHERE user_id = ?1 AND substr(created_at, 1, 10) >= ?2
                 GROUP BY model ORDER BY c DESC LIMIT 8",
            )
            .map_err(|_| ())?;
        let top_models = stmt
            .query_map(rusqlite::params![user_id, since_date], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|_| ())?
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>();
        Ok(user_port::WalletSnapshot {
            balance_micro: balance,
            daily,
            top_models,
        })
    }

    fn available_prices(&self) -> Result<Vec<(String, Option<user_port::PriceTriple>)>, ()> {
        // 健康渠道提供的全部模型(已排序)逐个经 find_price 展开;
        // 未定价三价出 null(迁自旧 api/admin/billing.rs::list_public_prices)。
        let models = crate::channel::selector::list_available_models(&self.pool);
        let conn = self.pool.read().lock().map_err(|_| ())?;
        Ok(models
            .into_iter()
            .map(|m| {
                let price = crate::billing::find_price(&conn, &m);
                (m, price)
            })
            .collect())
    }

    fn hash_code(&self, code: &str) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(code.as_bytes()))
    }

    fn redeem(&self, user_id: &str, code_hash: &str) -> Result<Option<(i64, i64)>, ()> {
        let mut conn = self.pool.conn.lock().map_err(|_| ())?;
        (|| -> Result<Option<(i64, i64)>, rusqlite::Error> {
            let tx = conn.transaction()?;
            // 核销与加余额同事务:UPDATE 的 WHERE 同时覆盖状态与过期校验,
            // 影响行数 = 0 即"无效/已用/已禁用/已过期"(迁自旧 redemption.rs)。
            let n = tx.execute(
                "UPDATE redemption_codes SET status = 'used', used_by = ?1, used_at = datetime('now')
                 WHERE code_hash = ?2 AND status = 'unused'
                   AND (expires_at IS NULL OR expires_at > datetime('now'))",
                rusqlite::params![user_id, code_hash],
            )?;
            if n == 0 {
                return Ok(None);
            }
            let amount: i64 = tx.query_row(
                "SELECT amount FROM redemption_codes WHERE code_hash = ?1",
                rusqlite::params![code_hash],
                |row| row.get(0),
            )?;
            tx.execute(
                "UPDATE users SET balance = balance + ?1, updated_at = datetime('now') WHERE id = ?2",
                rusqlite::params![amount, user_id],
            )?;
            let balance: i64 = tx.query_row(
                "SELECT balance FROM users WHERE id = ?1",
                rusqlite::params![user_id],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT INTO billing_transactions (id, user_id, amount, balance_after, kind, note)
                 VALUES (?1, ?2, ?3, ?4, 'recharge', '兑换码充值')",
                rusqlite::params![uuid::Uuid::new_v4().to_string(), user_id, amount, balance],
            )?;
            tx.commit()?;
            Ok(Some((amount, balance)))
        })()
        .map_err(|e| {
            tracing::error!("DbUserStore::redeem: {e}")
        })
    }
}

// ---------- Admin·Users 适配器(DbAdminUsersStore;SQL 迁自旧
// api/admin/users.rs) ----------

use aikun_admin::admin_users as users_port;

struct DbAdminUsersStore {
    pool: Arc<DbPool>,
}

const ADMIN_USER_COLS: &str =
    "id, username, display_name, role, is_active, created_at, updated_at, balance";

fn row_to_admin_user_record(row: &rusqlite::Row) -> rusqlite::Result<users_port::AdminUserRecord> {
    Ok(users_port::AdminUserRecord {
        id: row.get(0)?,
        username: row.get(1)?,
        display_name: row.get(2)?,
        role: row.get(3)?,
        is_active: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        balance_micro: row.get(7)?,
    })
}

fn is_unique_violation(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(err, _)
            if err.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

/// 是否为唯一活跃 admin(旧 api/admin/users.rs::is_last_active_admin:
/// 计数查询失败按 0 的 fail-open 口径保留)。
fn last_active_admin(conn: &rusqlite::Connection, user_id: &str) -> bool {
    let target: Option<(String, bool)> = conn
        .query_row(
            "SELECT role, is_active FROM users WHERE id = ?1",
            rusqlite::params![user_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    match target {
        Some((role, is_active)) if role == "admin" && is_active => {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM users WHERE role = 'admin' AND is_active = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            count <= 1
        }
        _ => false,
    }
}

impl users_port::AdminUsersStore for DbAdminUsersStore {
    fn list_users(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<users_port::AdminUserRecord>, i64), users_port::UsersStoreError> {
        let conn = self
            .pool
            .read()
            .lock()
            .map_err(|_| users_port::UsersStoreError::Unavailable)?;
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .map_err(|_| users_port::UsersStoreError::Failed)?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {ADMIN_USER_COLS} FROM users
                 ORDER BY created_at DESC, rowid DESC LIMIT ?1 OFFSET ?2"
            ))
            .map_err(|_| users_port::UsersStoreError::Failed)?;
        let rows = stmt
            .query_map(rusqlite::params![limit, offset], row_to_admin_user_record)
            .map_err(|_| users_port::UsersStoreError::Failed)?;
        Ok((rows.filter_map(|r| r.ok()).collect(), total))
    }

    fn find_user(
        &self,
        id: &str,
    ) -> Result<Option<users_port::AdminUserRecord>, users_port::UsersStoreError> {
        let conn = self
            .pool
            .read()
            .lock()
            .map_err(|_| users_port::UsersStoreError::Unavailable)?;
        Ok(conn
            .query_row(
                &format!("SELECT {ADMIN_USER_COLS} FROM users WHERE id = ?1"),
                rusqlite::params![id],
                row_to_admin_user_record,
            )
            .ok())
    }

    fn insert_user(
        &self,
        user: &users_port::NewUser,
    ) -> Result<users_port::InsertOutcome, users_port::UsersStoreError> {
        let conn = self
            .pool
            .conn
            .lock()
            .map_err(|_| users_port::UsersStoreError::Unavailable)?;
        let id = uuid::Uuid::new_v4().to_string();
        match conn.execute(
            "INSERT INTO users (id, username, password_hash, display_name, role)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                id,
                user.username,
                user.password_hash,
                user.display_name,
                user.role
            ],
        ) {
            Ok(_) => Ok(users_port::InsertOutcome::Created(id)),
            Err(e) if is_unique_violation(&e) => Ok(users_port::InsertOutcome::UsernameExists),
            Err(e) => {
                tracing::error!("DbAdminUsersStore::insert_user: {e}");
                Err(users_port::UsersStoreError::Failed)
            }
        }
    }

    fn insert_users_batch(
        &self,
        users: &[users_port::PreparedUser],
    ) -> Result<Vec<users_port::BatchInsertItem>, users_port::UsersStoreError> {
        let conn = self
            .pool
            .conn
            .lock()
            .map_err(|_| users_port::UsersStoreError::Unavailable)?;
        // 一把锁内逐条 INSERT,不包整体事务(单项失败不影响其余)。
        let mut out = Vec::with_capacity(users.len());
        for p in users {
            let id = uuid::Uuid::new_v4().to_string();
            let outcome = match conn.execute(
                "INSERT INTO users (id, username, password_hash, display_name, role)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![id, p.username, p.password_hash, p.display_name, p.role],
            ) {
                Ok(_) => users_port::BatchInsertItem::Created(id),
                Err(e) if is_unique_violation(&e) => users_port::BatchInsertItem::UsernameExists,
                Err(e) => {
                    tracing::error!("DbAdminUsersStore::insert_users_batch {}: {e}", p.username);
                    users_port::BatchInsertItem::Failed
                }
            };
            out.push(outcome);
        }
        Ok(out)
    }

    fn update_user(
        &self,
        id: &str,
        patch: &users_port::UserPatch,
    ) -> Result<users_port::UpdateOutcome, users_port::UsersStoreError> {
        let conn = self
            .pool
            .conn
            .lock()
            .map_err(|_| users_port::UsersStoreError::Unavailable)?;
        // last_admin 保护:同一把写锁内先判定再更新(旧 users.rs:486-496)。
        if patch.demotes_or_disables && last_active_admin(&conn, id) {
            return Ok(users_port::UpdateOutcome::LastAdmin);
        }
        let mut sets = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(name) = &patch.display_name {
            sets.push("display_name = ?");
            params.push(Box::new(name.clone()));
        }
        if let Some(hash) = &patch.password_hash {
            sets.push("password_hash = ?");
            params.push(Box::new(hash.clone()));
            // 改密后递增 token_version,踢掉该用户全部已签发 JWT。
            sets.push("token_version = token_version + 1");
        }
        if let Some(active) = patch.is_active {
            sets.push("is_active = ?");
            params.push(Box::new(active as i32));
        }
        if let Some(role) = &patch.role {
            sets.push("role = ?");
            params.push(Box::new(role.clone()));
        }
        sets.push("updated_at = datetime('now')");
        params.push(Box::new(id.to_string()));
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let n = conn
            .execute(
                &format!("UPDATE users SET {} WHERE id = ?", sets.join(", ")),
                refs.as_slice(),
            )
            .map_err(|e| {
                tracing::error!("DbAdminUsersStore::update_user: {e}");
                users_port::UsersStoreError::Failed
            })?;
        if n == 0 {
            return Ok(users_port::UpdateOutcome::NotFound);
        }
        let row = conn
            .query_row(
                &format!("SELECT {ADMIN_USER_COLS} FROM users WHERE id = ?1"),
                rusqlite::params![id],
                row_to_admin_user_record,
            )
            .map_err(|_| users_port::UsersStoreError::Failed)?;
        Ok(users_port::UpdateOutcome::Updated(Box::new(row)))
    }

    fn disable_user(
        &self,
        id: &str,
    ) -> Result<users_port::DisableOutcome, users_port::UsersStoreError> {
        let conn = self
            .pool
            .conn
            .lock()
            .map_err(|_| users_port::UsersStoreError::Unavailable)?;
        if last_active_admin(&conn, id) {
            return Ok(users_port::DisableOutcome::LastAdmin);
        }
        let n = conn
            .execute(
                "UPDATE users SET is_active = 0, updated_at = datetime('now') WHERE id = ?1",
                rusqlite::params![id],
            )
            .map_err(|e| {
                tracing::error!("DbAdminUsersStore::disable_user: {e}");
                users_port::UsersStoreError::Failed
            })?;
        if n == 0 {
            return Ok(users_port::DisableOutcome::NotFound);
        }
        Ok(users_port::DisableOutcome::Disabled)
    }

    fn hash_password(&self, password: &str) -> Result<String, ()> {
        crate::auth::hash_password(password).map_err(|_| ())
    }

    fn random_password(&self) -> String {
        crate::auth::generate_random_password()
    }
}

// ---------- Admin·Billing 适配器(DbBillingStore;SQL 迁自旧
// api/admin/billing.rs 与 api/redemption.rs 管理端三 op) ----------

use aikun_admin::admin_billing as billing_port;

struct DbBillingStore {
    pool: Arc<DbPool>,
}

const PRICE_COLS: &str =
    "id, model, prompt_price, completion_price, cached_price, created_at, updated_at";

fn row_to_price_record(row: &rusqlite::Row) -> rusqlite::Result<billing_port::PriceRecord> {
    Ok(billing_port::PriceRecord {
        id: row.get(0)?,
        model: row.get(1)?,
        prompt_price: row.get(2)?,
        completion_price: row.get(3)?,
        cached_price: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn row_to_tx_record(row: &rusqlite::Row) -> rusqlite::Result<billing_port::TxRecord> {
    Ok(billing_port::TxRecord {
        id: row.get(0)?,
        user_id: row.get(1)?,
        username: row.get(2)?,
        amount_micro: row.get(3)?,
        balance_after_micro: row.get(4)?,
        kind: row.get(5)?,
        note: row.get(6)?,
        created_at: row.get(7)?,
    })
}

fn row_to_redemption_record(
    row: &rusqlite::Row,
) -> rusqlite::Result<billing_port::RedemptionCodeRecord> {
    Ok(billing_port::RedemptionCodeRecord {
        id: row.get(0)?,
        code_suffix: row.get(1)?,
        amount_micro: row.get(2)?,
        batch: row.get(3)?,
        status: row.get(4)?,
        used_by_username: row.get(5)?,
        used_at: row.get(6)?,
        expires_at: row.get(7)?,
        note: row.get(8)?,
        created_at: row.get(9)?,
        expired: row.get(10)?,
    })
}

/// 兑换码码字符集:去除易混淆的 0/O/1/I(迁自旧 api/redemption.rs)。
const CODE_CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

impl billing_port::BillingStore for DbBillingStore {
    fn all_prices(
        &self,
    ) -> Result<Vec<billing_port::PriceRecord>, billing_port::BillingStoreError> {
        let conn = self
            .pool
            .read()
            .lock()
            .map_err(|_| billing_port::BillingStoreError::Unavailable)?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {PRICE_COLS} FROM model_prices ORDER BY model"
            ))
            .map_err(|e| billing_port::BillingStoreError::Failed(e.to_string()))?;
        let rows = stmt
            .query_map([], row_to_price_record)
            .map_err(|e| billing_port::BillingStoreError::Failed(e.to_string()))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    fn all_channel_models(&self) -> std::collections::HashSet<String> {
        // 查询失败静默返回空集(旧 channel_models 的 if let Ok 语义)。
        let mut set = std::collections::HashSet::new();
        if let Ok(conn) = self.pool.read().lock()
            && let Ok(mut stmt) = conn.prepare("SELECT models FROM providers")
            && let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0))
        {
            for r in rows.flatten() {
                if let Ok(list) = serde_json::from_str::<Vec<String>>(&r) {
                    set.extend(list);
                }
            }
        }
        set
    }

    fn insert_price(
        &self,
        model: &str,
        prompt_price: f64,
        completion_price: f64,
        cached_price: Option<f64>,
    ) -> Result<billing_port::PriceRecord, billing_port::BillingStoreError> {
        let conn = self
            .pool
            .conn
            .lock()
            .map_err(|_| billing_port::BillingStoreError::Unavailable)?;
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO model_prices (id, model, prompt_price, completion_price, cached_price)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, model, prompt_price, completion_price, cached_price],
        )
        .map_err(|e| {
            if is_unique_violation(&e) {
                billing_port::BillingStoreError::Constraint
            } else {
                tracing::error!("DbBillingStore::insert_price: {e}");
                billing_port::BillingStoreError::Failed(e.to_string())
            }
        })?;
        conn.query_row(
            &format!("SELECT {PRICE_COLS} FROM model_prices WHERE id = ?1"),
            rusqlite::params![id],
            row_to_price_record,
        )
        .map_err(|e| billing_port::BillingStoreError::Failed(e.to_string()))
    }

    fn update_price(
        &self,
        id: &str,
        model: Option<&str>,
        prompt_price: Option<f64>,
        completion_price: Option<f64>,
        cached_price: Option<Option<f64>>,
    ) -> Result<Option<billing_port::PriceRecord>, billing_port::BillingStoreError> {
        let conn = self
            .pool
            .conn
            .lock()
            .map_err(|_| billing_port::BillingStoreError::Unavailable)?;
        let mut sets = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(m) = model {
            sets.push("model = ?");
            params.push(Box::new(m.to_string()));
        }
        if let Some(p) = prompt_price {
            sets.push("prompt_price = ?");
            params.push(Box::new(p));
        }
        if let Some(c) = completion_price {
            sets.push("completion_price = ?");
            params.push(Box::new(c));
        }
        // cached_price 双层 Option:缺省不改动;Some(None) 清回 NULL;
        // Some(Some(v)) 正常更新。
        if let Some(k) = cached_price {
            sets.push("cached_price = ?");
            params.push(Box::new(k));
        }
        sets.push("updated_at = datetime('now')");
        params.push(Box::new(id.to_string()));
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let n = conn
            .execute(
                &format!("UPDATE model_prices SET {} WHERE id = ?", sets.join(", ")),
                refs.as_slice(),
            )
            .map_err(|e| {
                if is_unique_violation(&e) {
                    billing_port::BillingStoreError::Constraint
                } else {
                    tracing::error!("DbBillingStore::update_price: {e}");
                    billing_port::BillingStoreError::Failed(e.to_string())
                }
            })?;
        if n == 0 {
            return Ok(None);
        }
        Ok(conn
            .query_row(
                &format!("SELECT {PRICE_COLS} FROM model_prices WHERE id = ?1"),
                rusqlite::params![id],
                row_to_price_record,
            )
            .ok())
    }

    fn delete_price(&self, id: &str) -> Result<bool, billing_port::BillingStoreError> {
        let conn = self
            .pool
            .conn
            .lock()
            .map_err(|_| billing_port::BillingStoreError::Unavailable)?;
        conn.execute(
            "DELETE FROM model_prices WHERE id = ?1",
            rusqlite::params![id],
        )
        .map(|n| n > 0)
        .map_err(|e| {
            tracing::error!("DbBillingStore::delete_price: {e}");
            billing_port::BillingStoreError::Failed(e.to_string())
        })
    }

    fn adjust_balance(
        &self,
        user_id: &str,
        amount_micro: i64,
        kind: &str,
        note: &str,
    ) -> Result<Option<i64>, billing_port::BillingStoreError> {
        let mut conn = self
            .pool
            .conn
            .lock()
            .map_err(|_| billing_port::BillingStoreError::Unavailable)?;
        (|| -> Result<Option<i64>, rusqlite::Error> {
            // UPDATE 余额与 INSERT 流水同一事务:失败整体回滚,不留
            // "余额变了但没流水"的中间态(迁自旧 adjust_balance)。
            let tx = conn.transaction()?;
            let updated = tx.execute(
                "UPDATE users SET balance = balance + ?1, updated_at = datetime('now') WHERE id = ?2",
                rusqlite::params![amount_micro, user_id],
            )?;
            if updated == 0 {
                return Ok(None);
            }
            let balance: i64 = tx.query_row(
                "SELECT balance FROM users WHERE id = ?1",
                rusqlite::params![user_id],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT INTO billing_transactions (id, user_id, amount, balance_after, kind, note)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    uuid::Uuid::new_v4().to_string(),
                    user_id,
                    amount_micro,
                    balance,
                    kind,
                    note
                ],
            )?;
            tx.commit()?;
            Ok(Some(balance))
        })()
        .map_err(|e| {
            tracing::error!("DbBillingStore::adjust_balance: {e}");
            billing_port::BillingStoreError::Failed(e.to_string())
        })
    }

    fn list_transactions(
        &self,
        user_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<billing_port::TxRecord>), billing_port::BillingStoreError> {
        let conn = self
            .pool
            .read()
            .lock()
            .map_err(|_| billing_port::BillingStoreError::Unavailable)?;
        let uid = user_id.unwrap_or("").trim();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let where_clause = if uid.is_empty() {
            ""
        } else {
            params.push(Box::new(uid.to_string()));
            "WHERE t.user_id = ?"
        };
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let total: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM billing_transactions t {where_clause}"),
                refs.as_slice(),
                |r| r.get(0),
            )
            .map_err(|e| billing_port::BillingStoreError::Failed(e.to_string()))?;
        params.push(Box::new(limit));
        params.push(Box::new(offset));
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT t.id, t.user_id, u.username, t.amount, t.balance_after, t.kind, t.note, t.created_at
                 FROM billing_transactions t LEFT JOIN users u ON u.id = t.user_id
                 {where_clause}
                 ORDER BY t.created_at DESC, t.rowid DESC LIMIT ? OFFSET ?"
            ))
            .map_err(|e| billing_port::BillingStoreError::Failed(e.to_string()))?;
        let rows = stmt
            .query_map(refs.as_slice(), row_to_tx_record)
            .map_err(|e| billing_port::BillingStoreError::Failed(e.to_string()))?;
        Ok((total, rows.filter_map(|r| r.ok()).collect()))
    }

    fn list_redemption_codes(
        &self,
        batch: Option<&str>,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<billing_port::RedemptionCodeRecord>), billing_port::BillingStoreError>
    {
        let conn = self
            .pool
            .read()
            .lock()
            .map_err(|_| billing_port::BillingStoreError::Unavailable)?;
        let batch = batch.unwrap_or("").trim();
        // expired 是显示态:库中 status 仍为 unused,按过期时间判定;
        // 状态片段只做白名单内的固定字符串拼接,无注入面。
        let status_sql = match status.unwrap_or("") {
            "" => "",
            "expired" => {
                " AND c.status = 'unused' AND c.expires_at IS NOT NULL AND c.expires_at <= datetime('now')"
            }
            "unused" => " AND c.status = 'unused'",
            "used" => " AND c.status = 'used'",
            "disabled" => " AND c.status = 'disabled'",
            // 生成层已枚举校验,防御直调。
            _ => "",
        };
        let where_sql = format!("(?1 = '' OR c.batch = ?1){status_sql}");
        let total: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM redemption_codes c WHERE {where_sql}"),
                rusqlite::params![batch],
                |r| r.get(0),
            )
            .map_err(|e| billing_port::BillingStoreError::Failed(e.to_string()))?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT c.id, c.code_suffix, c.amount, c.batch, c.status, u.username,
                        c.used_at, c.expires_at, c.note, c.created_at,
                        (c.expires_at IS NOT NULL AND c.expires_at <= datetime('now'))
                 FROM redemption_codes c LEFT JOIN users u ON u.id = c.used_by
                 WHERE {where_sql}
                 ORDER BY c.created_at DESC, c.rowid DESC LIMIT ?2 OFFSET ?3"
            ))
            .map_err(|e| billing_port::BillingStoreError::Failed(e.to_string()))?;
        let rows = stmt
            .query_map(
                rusqlite::params![batch, limit, offset],
                row_to_redemption_record,
            )
            .map_err(|e| billing_port::BillingStoreError::Failed(e.to_string()))?;
        Ok((total, rows.filter_map(|r| r.ok()).collect()))
    }

    fn insert_redemption_codes(
        &self,
        rows: &[billing_port::NewRedemptionCode],
    ) -> Result<(), billing_port::BillingStoreError> {
        let mut conn = self
            .pool
            .conn
            .lock()
            .map_err(|_| billing_port::BillingStoreError::Unavailable)?;
        (|| -> Result<(), rusqlite::Error> {
            // 整批一个事务:中途失败整体回滚,避免哈希落库而明文从未送达。
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO redemption_codes (id, code_hash, code_suffix, amount, batch, expires_at, note)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                for r in rows {
                    stmt.execute(rusqlite::params![
                        uuid::Uuid::new_v4().to_string(),
                        r.code_hash,
                        r.code_suffix,
                        r.amount_micro,
                        r.batch,
                        r.expires_at,
                        r.note,
                    ])?;
                }
            }
            tx.commit()
        })()
        .map_err(|e| {
            if is_unique_violation(&e) {
                billing_port::BillingStoreError::Constraint
            } else {
                tracing::error!("DbBillingStore::insert_redemption_codes: {e}");
                billing_port::BillingStoreError::Failed(e.to_string())
            }
        })
    }

    fn disable_redemption_code(
        &self,
        id: &str,
    ) -> Result<billing_port::DisableOutcome, billing_port::BillingStoreError> {
        let conn = self
            .pool
            .conn
            .lock()
            .map_err(|_| billing_port::BillingStoreError::Unavailable)?;
        let n = conn
            .execute(
                "UPDATE redemption_codes SET status = 'disabled' WHERE id = ?1 AND status = 'unused'",
                rusqlite::params![id],
            )
            .map_err(|e| {
                tracing::error!("DbBillingStore::disable_redemption_code: {e}");
                billing_port::BillingStoreError::Failed(e.to_string())
            })?;
        if n == 1 {
            return Ok(billing_port::DisableOutcome::Disabled);
        }
        // 影响行数 0:回查存在性区分"非 unused"与"不存在"。
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM redemption_codes WHERE id = ?1)",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if exists {
            Ok(billing_port::DisableOutcome::NotUnused)
        } else {
            Ok(billing_port::DisableOutcome::NotFound)
        }
    }

    fn generate_code(&self) -> String {
        use rand::Rng;
        let mut rng = rand::rng();
        let pick = |rng: &mut rand::rngs::ThreadRng| {
            CODE_CHARSET[rng.random_range(0..CODE_CHARSET.len())] as char
        };
        let seg = |rng: &mut rand::rngs::ThreadRng| (0..4).map(|_| pick(rng)).collect::<String>();
        format!(
            "AK-{}-{}-{}-{}",
            seg(&mut rng),
            seg(&mut rng),
            seg(&mut rng),
            seg(&mut rng)
        )
    }

    fn hash_code(&self, code: &str) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(code.as_bytes()))
    }
}

// ---------- Admin·Providers 适配器(DbProviderStore + GatewayProbes;
// SQL 迁自旧 api/admin/providers.rs,探测复用 channel 层传输/适配器) ----------

use aikun_admin::admin_providers as providers_port;

struct DbProviderStore {
    pool: Arc<DbPool>,
}

struct GatewayProbes {
    pool: Arc<DbPool>,
    clients: Arc<Mutex<HashMap<String, reqwest::Client>>>,
}

/// providers 配置列(复制/插入用,与 providers_port::ProviderConfig 一一对应)。
const PROVIDER_CONFIG_COLS: &str = "name, provider_type, openai_base_url, anthropic_base_url,
    api_key, key_hash, models, priority, weight, max_retries, timeout_secs, proxy_url,
    model_mapping, protocols, default_protocol, note, website_url, channel_kind";

fn row_to_provider_config(row: &rusqlite::Row) -> rusqlite::Result<providers_port::ProviderConfig> {
    Ok(providers_port::ProviderConfig {
        name: row.get(0)?,
        provider_type: row.get(1)?,
        openai_base_url: row.get(2)?,
        anthropic_base_url: row.get(3)?,
        api_key_stored: row.get(4)?,
        key_hash: row.get(5)?,
        models_json: row.get(6)?,
        priority: row.get(7)?,
        weight: row.get(8)?,
        max_retries: row.get(9)?,
        timeout_secs: row.get(10)?,
        proxy_url: row.get(11)?,
        model_mapping_json: row.get(12)?,
        protocols_json: row.get(13)?,
        default_protocol: row.get(14)?,
        note: row.get(15)?,
        website_url: row.get(16)?,
        channel_kind: row.get(17)?,
    })
}

fn insert_provider_row(
    conn: &rusqlite::Connection,
    id: &str,
    c: &providers_port::ProviderConfig,
) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO providers (id, {PROVIDER_CONFIG_COLS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)"
        ),
        rusqlite::params![
            id,
            c.name,
            c.provider_type,
            c.openai_base_url,
            c.anthropic_base_url,
            c.api_key_stored,
            c.key_hash,
            c.models_json,
            c.priority,
            c.weight,
            c.max_retries,
            c.timeout_secs,
            c.proxy_url,
            c.model_mapping_json,
            c.protocols_json,
            c.default_protocol,
            c.note,
            c.website_url,
            c.channel_kind
        ],
    )?;
    Ok(())
}

#[async_trait::async_trait]
impl providers_port::ProviderStore for DbProviderStore {
    fn new_provider_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn encrypt_key(&self, plaintext: &str) -> String {
        self.pool.cipher.encrypt(plaintext)
    }

    fn decrypt_key(&self, stored: &str) -> String {
        crate::crypto::decrypt_or_plain(&self.pool.cipher, stored)
    }

    fn hash_key(&self, key: &str) -> String {
        crate::auth::hash_api_key(key)
    }

    async fn list_providers(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<aikun_channels::Provider>, i64), ()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.read().lock().map_err(|_| ())?;
            let total: i64 = conn
                .query_row("SELECT COUNT(*) FROM providers", [], |r| r.get(0))
                .map_err(|_| ())?;
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {} FROM providers ORDER BY priority DESC, name ASC LIMIT ?1 OFFSET ?2",
                    crate::models::PROVIDER_SELECT_COLUMNS
                ))
                .map_err(|_| ())?;
            let rows = stmt
                .query_map(
                    rusqlite::params![limit, offset],
                    crate::models::row_to_provider,
                )
                .map_err(|_| ())?;
            Ok((rows.filter_map(|r| r.ok()).collect(), total))
        })
        .await
        .map_err(|_| ())?
    }

    async fn find_provider(&self, id: &str) -> Result<Option<aikun_channels::Provider>, ()> {
        let pool = self.pool.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.read().lock().map_err(|_| ())?;
            Ok(conn
                .query_row(
                    &format!(
                        "SELECT {} FROM providers WHERE id = ?1",
                        crate::models::PROVIDER_SELECT_COLUMNS
                    ),
                    rusqlite::params![id],
                    crate::models::row_to_provider,
                )
                .ok())
        })
        .await
        .map_err(|_| ())?
    }

    async fn read_provider_config(
        &self,
        id: &str,
    ) -> Result<Option<providers_port::ProviderConfig>, ()> {
        let pool = self.pool.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.read().lock().map_err(|_| ())?;
            Ok(conn
                .query_row(
                    &format!("SELECT {PROVIDER_CONFIG_COLS} FROM providers WHERE id = ?1"),
                    rusqlite::params![id],
                    row_to_provider_config,
                )
                .ok())
        })
        .await
        .map_err(|_| ())?
    }

    async fn insert_provider(
        &self,
        id: &str,
        config: &providers_port::ProviderConfig,
    ) -> Result<(), ()> {
        let pool = self.pool.clone();
        let id = id.to_string();
        let config = config.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.conn.lock().map_err(|_| ())?;
            insert_provider_row(&conn, &id, &config)
                .map_err(|e| tracing::error!("DbProviderStore::insert_provider: {e}"))
        })
        .await
        .map_err(|_| ())?
    }

    async fn find_key_owner(&self, key_hash: &str, exclude_id: &str) -> Result<Option<String>, ()> {
        let pool = self.pool.clone();
        let key_hash = key_hash.to_string();
        let exclude_id = exclude_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.read().lock().map_err(|_| ())?;
            Ok(conn
                .query_row(
                    "SELECT name FROM providers WHERE key_hash = ?1 AND id != ?2 LIMIT 1",
                    rusqlite::params![key_hash, exclude_id],
                    |row| row.get(0),
                )
                .ok())
        })
        .await
        .map_err(|_| ())?
    }

    async fn update_provider(
        &self,
        id: &str,
        patch: &providers_port::ProviderPatch,
    ) -> Result<u64, ()> {
        let pool = self.pool.clone();
        let id = id.to_string();
        let patch = patch.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.conn.lock().map_err(|_| ())?;
            let mut sets: Vec<&'static str> = Vec::new();
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            let push = |sets: &mut Vec<&'static str>,
                        params: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
                        set: &'static str,
                        v: String| {
                sets.push(set);
                params.push(Box::new(v));
            };
            if let Some(v) = &patch.name {
                push(&mut sets, &mut params, "name = ?", v.clone());
            }
            if let Some(v) = &patch.provider_type {
                push(&mut sets, &mut params, "provider_type = ?", v.clone());
            }
            if let Some(v) = &patch.openai_base_url {
                push(&mut sets, &mut params, "openai_base_url = ?", v.clone());
            }
            if let Some(v) = &patch.anthropic_base_url {
                push(&mut sets, &mut params, "anthropic_base_url = ?", v.clone());
            }
            if let Some(v) = &patch.note {
                push(&mut sets, &mut params, "note = ?", v.clone());
            }
            if let Some(v) = &patch.website_url {
                push(&mut sets, &mut params, "website_url = ?", v.clone());
            }
            if let Some(v) = &patch.channel_kind {
                push(&mut sets, &mut params, "channel_kind = ?", v.clone());
            }
            if let Some(v) = &patch.api_key_stored {
                push(&mut sets, &mut params, "api_key = ?", v.clone());
            }
            if let Some(v) = &patch.key_hash {
                push(&mut sets, &mut params, "key_hash = ?", v.clone());
            }
            if let Some(v) = &patch.models_json {
                push(&mut sets, &mut params, "models = ?", v.clone());
            }
            if let Some(v) = patch.priority {
                sets.push("priority = ?");
                params.push(Box::new(v));
            }
            if let Some(v) = patch.weight {
                sets.push("weight = ?");
                params.push(Box::new(v));
            }
            if let Some(active) = patch.is_active {
                sets.push("is_active = ?");
                params.push(Box::new(active as i32));
                if active {
                    // 手动重启用:清自动禁用状态(旧实现语义)。
                    sets.push("disabled_reason = ''");
                    sets.push("consecutive_failures = 0");
                }
            }
            if let Some(v) = patch.max_retries {
                sets.push("max_retries = ?");
                params.push(Box::new(v));
            }
            if let Some(v) = patch.timeout_secs {
                sets.push("timeout_secs = ?");
                params.push(Box::new(v));
            }
            if let Some(v) = &patch.proxy_url {
                push(&mut sets, &mut params, "proxy_url = ?", v.clone());
            }
            if let Some(v) = &patch.model_mapping_json {
                push(&mut sets, &mut params, "model_mapping = ?", v.clone());
            }
            if let Some(v) = &patch.protocols_json {
                push(&mut sets, &mut params, "protocols = ?", v.clone());
            }
            if let Some(v) = &patch.default_protocol {
                push(&mut sets, &mut params, "default_protocol = ?", v.clone());
            }
            sets.push("updated_at = datetime('now')");
            params.push(Box::new(id.clone()));
            let refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();
            conn.execute(
                &format!("UPDATE providers SET {} WHERE id = ?", sets.join(", ")),
                refs.as_slice(),
            )
            .map(|n| n as u64)
            .map_err(|e| tracing::error!("DbProviderStore::update_provider: {e}"))
        })
        .await
        .map_err(|_| ())?
    }

    async fn delete_provider(&self, id: &str) -> Result<u64, ()> {
        let pool = self.pool.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.conn.lock().map_err(|_| ())?;
            conn.execute("DELETE FROM providers WHERE id = ?1", rusqlite::params![id])
                .map(|n| n as u64)
                .map_err(|e| tracing::error!("DbProviderStore::delete_provider: {e}"))
        })
        .await
        .map_err(|_| ())?
    }

    async fn update_health(&self, id: &str, status: &str, checked_at: &str) -> Result<(), ()> {
        let pool = self.pool.clone();
        let id = id.to_string();
        let status = status.to_string();
        let checked_at = checked_at.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.conn.lock().map_err(|_| ())?;
            // 刻意不更新 latency_ms:ping 延迟不代表真实请求延迟(与后台
            // 健康检查循环同口径)。
            conn.execute(
                "UPDATE providers SET health_status = ?1, last_health_check = ?2 WHERE id = ?3",
                rusqlite::params![status, checked_at, id],
            )
            .map(|_| ())
            .map_err(|e| tracing::error!("DbProviderStore::update_health: {e}"))
        })
        .await
        .map_err(|_| ())?
    }

    async fn record_model_health(
        &self,
        provider_id: &str,
        model: &str,
        ok: bool,
        latency_ms: f64,
        error: Option<&str>,
    ) {
        let pool = self.pool.clone();
        let provider_id = provider_id.to_string();
        let model = model.to_string();
        let error = error.map(str::to_string);
        // model_health upsert + 渠道 EMA 记账(与代理转发路径同口径);
        // 尽力而为,失败只留痕。
        let _ = tokio::task::spawn_blocking(move || {
            crate::channel::model_test::record_model_health(
                &pool,
                &provider_id,
                &model,
                ok,
                latency_ms,
                error.as_deref(),
            )
        })
        .await;
    }

    async fn list_model_health(
        &self,
        provider_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<providers_port::ModelHealthRow>, i64), ()> {
        let pool = self.pool.clone();
        let provider_id = provider_id.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            let conn = pool.read().lock().map_err(|_| ())?;
            let row_map = |row: &rusqlite::Row| {
                Ok(providers_port::ModelHealthRow {
                    provider_id: row.get(0)?,
                    model: row.get(1)?,
                    status: row.get(2)?,
                    latency_ms: row.get(3)?,
                    error: row.get(4)?,
                    checked_at: row.get(5)?,
                })
            };
            let (total, rows) = match &provider_id {
                Some(p) => {
                    let total: i64 = conn
                        .query_row(
                            "SELECT COUNT(*) FROM model_health WHERE provider_id = ?1",
                            rusqlite::params![p],
                            |r| r.get(0),
                        )
                        .map_err(|_| ())?;
                    let mut stmt = conn
                        .prepare(
                            "SELECT provider_id, model, status, latency_ms, error, checked_at
                             FROM model_health WHERE provider_id = ?1
                             ORDER BY provider_id, model LIMIT ?2 OFFSET ?3",
                        )
                        .map_err(|_| ())?;
                    let rows = stmt
                        .query_map(rusqlite::params![p, limit, offset], row_map)
                        .map_err(|_| ())?;
                    (total, rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
                }
                None => {
                    let total: i64 = conn
                        .query_row("SELECT COUNT(*) FROM model_health", [], |r| r.get(0))
                        .map_err(|_| ())?;
                    let mut stmt = conn
                        .prepare(
                            "SELECT provider_id, model, status, latency_ms, error, checked_at
                             FROM model_health
                             ORDER BY provider_id, model LIMIT ?1 OFFSET ?2",
                        )
                        .map_err(|_| ())?;
                    let rows = stmt
                        .query_map(rusqlite::params![limit, offset], row_map)
                        .map_err(|_| ())?;
                    (total, rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
                }
            };
            Ok((rows, total))
        })
        .await
        .map_err(|_| ())?
    }

    async fn find_provider_credentials(&self, id: &str) -> Result<Option<(String, String)>, ()> {
        let pool = self.pool.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.read().lock().map_err(|_| ())?;
            Ok(conn
                .query_row(
                    "SELECT api_key, proxy_url FROM providers WHERE id = ?1",
                    rusqlite::params![id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .ok())
        })
        .await
        .map_err(|_| ())?
    }
}

/// 从单条上游 SSE 事件 data 提取增量文本(口径同
/// channel::model_test 的 extract_delta;私有函数,适配器侧复制)。
fn sse_delta_text(protocol: aikun_protocol::Protocol, data: &str) -> String {
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    match protocol {
        aikun_protocol::Protocol::Anthropic => {
            if v["type"].as_str() == Some("content_block_delta") {
                return v["delta"]["text"].as_str().unwrap_or("").to_string();
            }
            String::new()
        }
        aikun_protocol::Protocol::OpenAi | aikun_protocol::Protocol::Responses => {
            v["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string()
        }
    }
}

#[async_trait::async_trait]
impl providers_port::ProviderProbes for GatewayProbes {
    async fn check_health(&self, probe: providers_port::HealthProbe) -> (String, f64) {
        let protocol = aikun_protocol::Protocol::parse(&probe.protocol)
            .unwrap_or_else(|| aikun_protocol::Protocol::for_provider_type(&probe.protocol));
        crate::channel::health::check_provider_health(
            &self.clients,
            &probe.base_url,
            &probe.api_key,
            protocol,
            probe.timeout_secs,
            &probe.proxy_url,
            &probe.channel_kind,
        )
        .await
    }

    async fn fetch_upstream_models(
        &self,
        probe: providers_port::FetchModelsProbe,
    ) -> Result<Vec<String>, providers_port::FetchModelsError> {
        use aikun_protocol::Protocol;
        // URL 与鉴权头由渠道适配器决定(与对话/健康检查同口径)。
        let protocol = match Protocol::parse(&probe.protocol) {
            Some(p @ (Protocol::OpenAi | Protocol::Anthropic)) => p,
            _ => {
                return Err(providers_port::FetchModelsError::Connect {
                    message: "invalid protocol".to_string(),
                });
            }
        };
        let client = crate::channel::transport::cached_client(
            &self.clients,
            &probe.proxy_url,
            "fetch-models",
            15,
        );
        let adapter = aikun_channels::registry().resolve(&probe.channel_kind);
        let url = adapter.models_url(&probe.base_url, protocol);
        let mut upstream_req = client.get(&url);
        for (k, v) in adapter.auth_headers(&probe.api_key, protocol) {
            upstream_req = upstream_req.header(k, v);
        }
        let resp = match upstream_req.send().await {
            Ok(r) => r,
            Err(e) => {
                // 只留错误描述:传输层错误对象含完整 URL(可能带敏感路径)。
                return Err(providers_port::FetchModelsError::Connect {
                    message: e.without_url().to_string(),
                });
            }
        };
        let status = resp.status().as_u16();
        // 8MB 响应上限(与对话/直通/实测同口径)。
        let body: Value = match crate::channel::transport::read_json_limited(
            resp,
            crate::channel::transport::MAX_RESP_BODY,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                return Err(providers_port::FetchModelsError::ReadFailed(e.zh()));
            }
        };
        if status != 200 {
            let message = body["error"]["message"]
                .as_str()
                .unwrap_or("upstream rejected the request")
                .to_string();
            return Err(providers_port::FetchModelsError::UpstreamStatus { status, message });
        }
        // OpenAI 与 Anthropic 均为 {"data":[{"id":...}]}。
        let models: Vec<String> = body["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["id"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if models.is_empty() {
            return Err(providers_port::FetchModelsError::NoModels);
        }
        Ok(models)
    }

    async fn probe_model(
        &self,
        provider: aikun_channels::Provider,
        protocol: String,
        model: String,
        messages: Vec<Value>,
        max_tokens: u32,
    ) -> providers_port::ModelProbeOutcome {
        let protocol = aikun_protocol::Protocol::parse(&protocol)
            .unwrap_or_else(|| aikun_protocol::Protocol::for_provider_type(&protocol));
        let probe = crate::channel::model_test::probe_model(
            &self.clients,
            &provider,
            protocol,
            &model,
            messages,
            max_tokens,
            false,
        )
        .await;
        providers_port::ModelProbeOutcome {
            ok: probe.ok,
            latency_ms: probe.latency_ms,
            status: probe.status,
            error: probe.error,
            snippet: probe.snippet,
            response: probe.response,
        }
    }

    fn probe_model_stream(
        &self,
        provider: aikun_channels::Provider,
        protocol: String,
        model: String,
        messages: Vec<Value>,
        max_tokens: u32,
    ) -> axum::response::sse::Sse<aikun_openapi::openapi::server::errors::ServerEventStream> {
        use aikun_openapi::openapi::server::errors::sse_response;
        use axum::response::sse::Event;
        use futures_util::StreamExt;
        use std::convert::Infallible;

        let protocol = aikun_protocol::Protocol::parse(&protocol)
            .unwrap_or_else(|| aikun_protocol::Protocol::for_provider_type(&protocol));
        let protocol_str = protocol.as_str().to_string();
        let clients = self.clients.clone();
        let pool = self.pool.clone();
        // 探针请求体(口径同 channel::model_test 的 build_probe_body)。
        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "messages": messages,
            "stream": true,
        });
        if protocol != aikun_protocol::Protocol::Anthropic {
            body["max_completion_tokens"] = json!(max_tokens);
        }

        let stream = async_stream::stream! {
            // 流式探针的 deadline / SSE 缓冲 / 可见文本上限同
            // channel::model_test 的流式分支。
            const STREAM_DEADLINE_SECS: u64 = 120;
            const STREAM_BUF_CAP: usize = 1024 * 1024;
            const STREAM_TEXT_CAP: usize = 64 * 1024;

            let latency;
            let mut text = String::new();
            let mut events: u32 = 0;
            // Some((status, message)) = 传输失败 / 非 200;None = 流式正常收尾。
            let mut fatal: Option<(u16, String)> = None;
            let mut stream_error: Option<String> = None;

            match crate::channel::transport::send_request(
                &clients, &provider, protocol, body, true, 120,
            )
            .await
            {
                Err((err_body, status, lat)) => {
                    latency = lat;
                    fatal = Some((
                        status,
                        err_body["error"]["message"]
                            .as_str()
                            .unwrap_or("upstream request failed")
                            .to_string(),
                    ));
                }
                Ok((resp, lat)) => {
                    latency = lat;
                    let status = resp.status().as_u16();
                    if status != 200 {
                        let v = crate::channel::transport::read_json_limited(
                            resp,
                            crate::channel::transport::MAX_RESP_BODY,
                        )
                        .await
                        .unwrap_or_else(|_| json!({}));
                        fatal = Some((
                            status,
                            v["error"]["message"]
                                .as_str()
                                .unwrap_or("upstream error")
                                .to_string(),
                        ));
                    } else {
                        let idle = std::time::Duration::from_secs(
                            provider.timeout_secs.clamp(1, 600) as u64,
                        );
                        let deadline = tokio::time::Instant::now()
                            + std::time::Duration::from_secs(STREAM_DEADLINE_SECS);
                        let mut upstream = std::pin::pin!(resp.bytes_stream());
                        let mut buf: Vec<u8> = Vec::new();
                        loop {
                            let now = tokio::time::Instant::now();
                            if now >= deadline {
                                stream_error = Some(format!(
                                    "流式响应超过整体 {STREAM_DEADLINE_SECS}s 上限"
                                ));
                                break;
                            }
                            let wait = idle.min(deadline - now);
                            let item = match tokio::time::timeout(wait, upstream.next()).await {
                                Ok(Some(item)) => item,
                                Ok(None) => break,
                                Err(_) => {
                                    stream_error = Some(
                                        if tokio::time::Instant::now() >= deadline {
                                            format!(
                                                "流式响应超过整体 {STREAM_DEADLINE_SECS}s 上限"
                                            )
                                        } else {
                                            format!("流式响应中断：超过 {}s 无数据", wait.as_secs())
                                        },
                                    );
                                    break;
                                }
                            };
                            match item {
                                Ok(bytes) => {
                                    buf.extend_from_slice(&bytes);
                                    for ev in aikun_protocol::parse_sse_events(&mut buf) {
                                        events += 1;
                                        let delta = sse_delta_text(protocol, &ev.data);
                                        if delta.is_empty() {
                                            continue;
                                        }
                                        if text.len() < STREAM_TEXT_CAP {
                                            text.push_str(&delta);
                                            if text.len() > STREAM_TEXT_CAP {
                                                let mut end = STREAM_TEXT_CAP;
                                                while !text.is_char_boundary(end) {
                                                    end -= 1;
                                                }
                                                text.truncate(end);
                                            }
                                        }
                                        // 事件契约:每个上游增量一个 delta 事件,
                                        // data 为 {"text":"..."}。
                                        yield Ok::<Event, Infallible>(
                                            Event::default()
                                                .event("delta")
                                                .data(json!({"text": delta}).to_string()),
                                        );
                                    }
                                    if buf.len() > STREAM_BUF_CAP {
                                        stream_error = Some("流式缓冲超过 1MB 上限".to_string());
                                        break;
                                    }
                                }
                                Err(e) => {
                                    stream_error = Some(format!("流式传输出错: {e}"));
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            let ok = fatal.is_none() && stream_error.is_none() && events > 0;
            let error_msg = fatal
                .as_ref()
                .map(|(_, m)| m.clone())
                .or(stream_error.clone());
            let mut result = json!({
                "ok": ok,
                "latency_ms": latency,
                "protocol": protocol_str,
                "stream": true,
            });
            let snippet: String = text.chars().take(120).collect();
            if !snippet.is_empty() {
                result["snippet"] = json!(snippet);
            }
            if let Some((status, message)) = &fatal {
                result["status"] = json!(status);
                result["error"] = json!(message);
            } else if let Some(err) = &stream_error {
                result["status"] = json!(0);
                result["error"] = json!(err);
            }
            // 末条 result 事件(data 为 ModelTestResult 形状)。
            yield Ok::<Event, Infallible>(
                Event::default().event("result").data(result.to_string()),
            );

            // 流结束落 model_health + EMA 记账(与非流式路径同口径;
            // 尽力而为,失败只留痕)。
            let pid = provider.id.clone();
            let model_for_record = model.clone();
            let err_for_record = error_msg.clone();
            let _ = tokio::task::spawn_blocking(move || {
                crate::channel::model_test::record_model_health(
                    &pool,
                    &pid,
                    &model_for_record,
                    ok,
                    latency,
                    err_for_record.as_deref(),
                )
            })
            .await;
        };
        sse_response(stream)
    }
}

// ---------- Admin·Stats 适配器(DbStatsStore;SQL 迁自旧
// api/logs.rs::usage_stats / admin_stats) ----------

use aikun_admin::admin_stats as stats_port;

struct DbStatsStore {
    pool: Arc<DbPool>,
}

impl stats_port::StatsStore for DbStatsStore {
    fn usage_snapshot(&self) -> Result<stats_port::UsageSnapshot, ()> {
        let conn = self.pool.read().lock().map_err(|_| ())?;
        // created_at 前 10 位即 YYYY-MM-DD(RFC3339 与 SQLite datetime 通用);
        // 一次查询拆出 today/week/month 三组计数(近 30 天窗口)。
        let today = chrono::Utc::now().date_naive();
        let fmt = |d: chrono::NaiveDate| d.format("%Y-%m-%d").to_string();
        let today_s = fmt(today);
        let week_s = fmt(today - chrono::Duration::days(6));
        let month_s = fmt(today - chrono::Duration::days(29));

        let (month_req, month_tok, today_req, today_tok, week_req, week_tok) = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(total_tokens), 0),
                        COALESCE(SUM(CASE WHEN substr(created_at, 1, 10) = ?1 THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN substr(created_at, 1, 10) = ?1 THEN total_tokens ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN substr(created_at, 1, 10) >= ?2 THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN substr(created_at, 1, 10) >= ?2 THEN total_tokens ELSE 0 END), 0)
                 FROM request_logs
                 WHERE substr(created_at, 1, 10) >= ?3",
                rusqlite::params![today_s, week_s, month_s],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .map_err(|e| {
                tracing::error!("DbStatsStore::usage_snapshot totals: {e}")
            })?;

        let daily = conn
            .prepare(
                "SELECT substr(created_at, 1, 10) AS d, COUNT(*), COALESCE(SUM(total_tokens), 0),
                        COALESCE(AVG(latency_ms), 0),
                        COALESCE(SUM(CASE WHEN success = 1 THEN 1 ELSE 0 END) * 100.0 / NULLIF(COUNT(*), 0), 0)
                 FROM request_logs
                 WHERE substr(created_at, 1, 10) >= ?1
                 GROUP BY d",
            )
            .and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![month_s], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, f64>(3)?,
                        row.get::<_, f64>(4)?,
                    ))
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
            })
            .map_err(|e| {
                tracing::error!("DbStatsStore::usage_snapshot daily: {e}")
            })?;

        let top_models = conn
            .prepare(
                "SELECT model, COUNT(*) AS c, COALESCE(SUM(total_tokens), 0)
                 FROM request_logs
                 WHERE substr(created_at, 1, 10) >= ?1
                 GROUP BY model ORDER BY c DESC LIMIT 8",
            )
            .and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![month_s], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
            })
            .map_err(|e| tracing::error!("DbStatsStore::usage_snapshot top_models: {e}"))?;

        Ok(stats_port::UsageSnapshot {
            today: (today_req, today_tok),
            week: (week_req, week_tok),
            month: (month_req, month_tok),
            daily,
            top_models,
        })
    }

    fn global_counts(&self) -> Result<stats_port::GlobalCounts, ()> {
        let conn = self.pool.read().lock().map_err(|_| ())?;
        conn.query_row(
            "SELECT (SELECT COUNT(*) FROM api_keys),
                    (SELECT COUNT(*) FROM users),
                    (SELECT COUNT(*) FROM providers),
                    (SELECT COUNT(*) FROM request_logs WHERE substr(created_at, 1, 10) = date('now'))",
            [],
            |row| {
                Ok(stats_port::GlobalCounts {
                    api_keys: row.get(0)?,
                    users: row.get(1)?,
                    providers: row.get(2)?,
                    requests_today: row.get(3)?,
                })
            },
        )
        .map_err(|e| {
            tracing::error!("DbStatsStore::global_counts: {e}")
        })
    }
}
