mod api;
mod auth;
mod billing;
mod config;
mod crypto;
mod db;
mod models;
mod proxy;
mod router;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{delete, get, patch, post},
    Router,
    Json,
};
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::auth::{auth_middleware, require_admin};
use crate::config::AppConfig;
use crate::db::DbPool;

/// Frontend assets, embedded into the binary at compile time — the
/// executable is fully self-contained and reads no static files at runtime.
/// The HTML shell references these under /static/*.
const INDEX_HTML: &str = include_str!("frontend/index.html");

/// 前端全自托管、无外链脚本,CSP 收敛为仅允许自身来源。
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'";

/// (path, content) for every embedded frontend asset.
const STATIC_ASSETS: &[(&str, &str)] = &[
    ("style.css", include_str!("frontend/style.css")),
    ("js/core.js", include_str!("frontend/js/core.js")),
    ("js/dashboard.js", include_str!("frontend/js/dashboard.js")),
    ("js/wallet.js", include_str!("frontend/js/wallet.js")),
    ("js/providers.js", include_str!("frontend/js/providers.js")),
    ("js/users.js", include_str!("frontend/js/users.js")),
    ("js/billing.js", include_str!("frontend/js/billing.js")),
    ("js/keys.js", include_str!("frontend/js/keys.js")),
    ("js/models.js", include_str!("frontend/js/models.js")),
    ("js/test.js", include_str!("frontend/js/test.js")),
    ("js/logs.js", include_str!("frontend/js/logs.js")),
    ("js/docs.js", include_str!("frontend/js/docs.js")),
    ("js/init.js", include_str!("frontend/js/init.js")),
];

/// Shared application state — all handlers receive this via `State<AppState>`.
#[derive(Clone)]
pub struct AppState {
    pub pool: Arc<DbPool>,
    pub config: Arc<AppConfig>,
    /// Cached reqwest clients keyed by "proxy_url|timeout_secs" — building a
    /// client per request is expensive and defeats connection pooling.
    pub clients: Arc<Mutex<HashMap<String, reqwest::Client>>>,
    /// Sliding-window login-attempt tracker keyed by "ip:..." / "user:..." —
    /// brute-force protection for /api/login.
    pub login_attempts: Arc<Mutex<HashMap<String, Vec<std::time::Instant>>>>,
    /// Sliding-window per-API-key request timestamps (keyed by api_keys.id) —
    /// enforces api_keys.rate_limit_rpm. In-memory only: a restart resets the
    /// window, which is acceptable for a per-minute limit.
    pub api_key_rate: Arc<Mutex<HashMap<String, Vec<std::time::Instant>>>>,
    /// 关停信号:收到 SIGINT/SIGTERM 后取消,流式生成器据此主动收尾——
    /// 已捕获 usage 照常记账,而不是被 30s 强杀跳过 Drop 丢账。
    pub shutdown: tokio_util::sync::CancellationToken,
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
                        Ok(true) => info!(
                            "管理员密码已通过环境变量重置,请尽快修改并移除该变量"
                        ),
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
        shutdown: tokio_util::sync::CancellationToken::new(),
    };

    // --- Public routes (no auth) ---
    let public_routes = Router::new()
        .route("/", get(serve_frontend))
        .route("/static/{*path}", get(serve_static))
        .route("/api/health", get(health_check))
        .route("/api/login", post(api::auth::login));

    // --- Protected routes (auth required) ---
    let protected_routes = Router::new()
        .route("/api/me", get(api::auth::get_current_user))
        .route("/api/version", get(version))
        .route("/api/api-keys", get(api::auth::list_api_keys))
        .route("/api/api-keys", post(api::auth::create_api_key))
        .route("/api/api-keys/{id}", delete(api::auth::delete_api_key))
        .route("/api/api-keys/{id}", patch(api::auth::update_api_key))
        .route("/api/logs", get(api::logs::list_logs))
        .route("/api/logs/stats", get(api::logs::log_stats))
        .route("/api/wallet", get(api::logs::wallet_stats))
        .route("/v1/chat/completions", post(api::proxy::chat::chat_completion))
        .route("/v1/messages", post(api::proxy::messages::messages))
        .route("/v1/responses", post(api::proxy::responses::responses))
        .route("/v1/models", get(api::proxy::chat::list_models))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    // --- Admin routes (auth + admin role required) ---
    let admin_routes = Router::new()
        .route("/api/admin/users", get(api::admin::users::list_users))
        .route("/api/admin/users", post(api::admin::users::create_user))
        .route("/api/admin/users/batch", post(api::admin::users::create_users_batch))
        .route("/api/admin/users/{id}", get(api::admin::users::get_user))
        .route("/api/admin/users/{id}", patch(api::admin::users::update_user))
        .route("/api/admin/users/{id}", delete(api::admin::users::delete_user))
        .route("/api/admin/providers", get(api::admin::providers::list_providers))
        .route("/api/admin/providers", post(api::admin::providers::create_provider))
        .route("/api/admin/providers/fetch-models", post(api::admin::providers::fetch_upstream_models))
        .route("/api/admin/providers/{id}", get(api::admin::providers::get_provider))
        .route("/api/admin/providers/{id}", patch(api::admin::providers::update_provider))
        .route("/api/admin/providers/{id}", delete(api::admin::providers::delete_provider))
        .route("/api/admin/providers/{id}/test", post(api::admin::providers::test_provider))
        .route("/api/admin/providers/{id}/test-model", post(api::admin::providers::test_provider_model))
        .route("/api/admin/providers/{id}/duplicate", post(api::admin::providers::duplicate_provider))
        .route("/api/admin/model-health", get(api::admin::providers::list_model_health))
        .route("/api/admin/prices", get(api::admin::billing::list_prices))
        .route("/api/admin/prices", post(api::admin::billing::create_price))
        .route("/api/admin/prices/{id}", patch(api::admin::billing::update_price))
        .route("/api/admin/prices/{id}", delete(api::admin::billing::delete_price))
        .route("/api/admin/users/{id}/balance", post(api::admin::billing::adjust_balance))
        .route("/api/admin/billing/transactions", get(api::admin::billing::list_transactions))
        .route("/api/admin/usage-stats", get(api::logs::usage_stats))
        .route("/api/admin/stats", get(api::logs::admin_stats))
        .layer(middleware::from_fn(require_admin))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    // Spawn the health check background task under a supervisor: if the loop
    // ever panics or exits, log it and restart after a short backoff so
    // health tracking cannot silently die.
    let health_pool = pool.clone();
    let health_config = config.clone();
    let health_clients = state.clients.clone();
    tokio::spawn(async move {
        use futures_util::FutureExt;
        loop {
            let result = std::panic::AssertUnwindSafe(crate::router::health::run_health_check_loop(
                health_pool.clone(),
                health_config.clone(),
                health_clients.clone(),
            ))
            .catch_unwind()
            .await;
            match result {
                Ok(()) => tracing::error!("Health check loop exited unexpectedly — restarting in 5s"),
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
            let result = std::panic::AssertUnwindSafe(crate::router::model_test::run_model_test_loop(
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
    let mut app = Router::new()
        .merge(public_routes)
        .merge(protected_routes)
        .merge(admin_routes)
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
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
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
        "timestamp": chrono::Utc::now().to_rfc3339()
    }))
}

/// 服务版本号。挂在受保护路由下:除 /api/health 外的接口一律需要授权。
async fn version() -> impl IntoResponse {
    Json(json!({ "version": env!("CARGO_PKG_VERSION") }))
}

async fn serve_frontend() -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            ("content-type", "text/html; charset=utf-8"),
            ("cache-control", "no-cache, no-store, must-revalidate"),
            ("pragma", "no-cache"),
            ("content-security-policy", CSP),
        ],
        INDEX_HTML,
    )
}

/// Serve an embedded frontend asset by its /static/ path. 404 for anything
/// not in the embedded table, so no filesystem access ever happens.
async fn serve_static(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> impl IntoResponse {
    let asset = STATIC_ASSETS.iter().find(|(name, _)| *name == path);
    match asset {
        Some((_, content)) => {
            let mime = if path.ends_with(".css") {
                "text/css; charset=utf-8"
            } else {
                "text/javascript; charset=utf-8"
            };
            (
                StatusCode::OK,
                [
                    ("content-type", mime),
                    ("cache-control", "no-cache, no-store, must-revalidate"),
                    ("content-security-policy", CSP),
                ],
                *content,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}