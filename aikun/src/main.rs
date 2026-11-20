mod api;
mod auth;
mod billing;
mod cache;
mod channel;
mod config;
mod crypto;
mod mcp;
mod models;
mod pipeline;
mod relay;
mod store;
mod zcode;

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
use crate::store::AppStore;
use crate::zcode::{ZcodeClaimImpl, ZcodeOAuthImpl};

/// Shared application state — all handlers receive this via `State<AppState>`.
#[derive(Clone)]
pub struct AppState {
    /// SurrealDB 存储门面(阶段 S 默认切换:三域能力 + 渠道 key 解密器)。
    pub store: Arc<AppStore>,
    pub config: Arc<AppConfig>,
    /// Cached reqwest clients keyed by "proxy_url|timeout_secs" — building a
    /// client per request is expensive and defeats connection pooling.
    pub clients: Arc<Mutex<HashMap<String, reqwest::Client>>>,
    /// 进程内限流器聚合(阶段 3 Limiter 合并,取代此前四个散落的
    /// `Arc<Mutex<HashMap>>`):登录失败 / API key RPM / 兑换失败三个
    /// 滑动窗口 + 并发在途计数。内存口径,重启清零(分钟级窗口可接受);
    /// 组件语义(访问时清理 + 删除时清理 + 锁中毒恢复)见
    /// aikun-store/src/limiter.rs。
    pub limiters: aikun_store::limiter::Limiters,
    /// 关停信号:收到 SIGINT/SIGTERM 后取消,流式生成器据此主动收尾——
    /// 已捕获 usage 照常记账,而不是被 30s 强杀跳过 Drop 丢账。
    pub shutdown: tokio_util::sync::CancellationToken,
    /// 响应缓存(默认 off;后端默认进程内内存)。
    pub cache: Arc<cache::GatewayCache>,
}

fn main() {
    // 阶段 S 栈配置(集成批必办,docs/surreal-migration.md §一):嵌入式
    // SurrealKV 的查询执行在调用任务栈上内联递归,默认 2MiB tokio 线程
    // 栈执行 SCHEMA_SURQL 级别的多语句查询即溢出(store 单测实测复现),
    // 官方 engine::local 文档建议 ~10MiB——显式建运行时替换 #[tokio::main]
    // 宏默认值。
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(10 * 1024 * 1024)
        .build()
        .expect("failed to build tokio runtime");
    runtime.block_on(async_main());
}

async fn async_main() {
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

    // SurrealDB 装配:连接(嵌入式 SurrealKV / 远程 URL)→ schema 初始化
    // (幂等)→ 默认价格/admin 播种 → 计数器重建。任一步失败拒绝启动。
    let store = Arc::new(
        store::init(&config)
            .await
            .expect("Failed to initialize SurrealDB"),
    );
    info!("Database initialized (SurrealDB)");

    // 渠道熔断参数(9.5):选路层短周期冷却,进程内全局单例。
    channel::breaker::configure(
        config.breaker_failure_threshold,
        config.breaker_cooldown_secs,
    );

    // 运维逃生口:设置 AIKUN_RESET_ADMIN_PASSWORD 后,启动即重置 admin 密码
    // 并 token_version + 1(旧会话全部失效)。密码本身绝不打印到日志。
    if let Ok(new_password) = std::env::var("AIKUN_RESET_ADMIN_PASSWORD") {
        if new_password.is_empty() {
            tracing::warn!("AIKUN_RESET_ADMIN_PASSWORD 为空,忽略密码重置");
        } else {
            match crate::auth::hash_password(&new_password) {
                Ok(hash) => match store.identity.reset_admin_password(&hash).await {
                    Ok(true) => info!("管理员密码已通过环境变量重置,请尽快修改并移除该变量"),
                    Ok(false) => {
                        tracing::warn!("AIKUN_RESET_ADMIN_PASSWORD 已设置,但库中不存在 admin 用户")
                    }
                    Err(e) => tracing::error!("管理员密码重置失败: {e}"),
                },
                Err(e) => tracing::error!("管理员密码重置失败(哈希计算): {e}"),
            }
        }
    }

    let state = AppState {
        store: store.clone(),
        config: config.clone(),
        clients: Arc::new(Mutex::new(HashMap::new())),
        limiters: aikun_store::limiter::Limiters::new(),
        shutdown: tokio_util::sync::CancellationToken::new(),
        cache: Arc::new(cache::GatewayCache::from_config(&config)),
    };

    // --- Auth tag(spec-first 新装配,crates/aikun-admin):裸挂,无鉴权中间件 ---
    // login 的 IP 维度限流与前端响应头归一在中间件层 —— 生成 trait 方法
    // 签名只有 body,拿不到对端地址,也表达不了自定义头(详见
    // crates/aikun-admin/src/auth.rs 文件头的装配契约)。
    let auth_service = aikun_admin::auth::AuthService::new(
        std::sync::Arc::new(SurrealAuthStore {
            store: store.clone(),
            config: config.clone(),
        }),
        state.limiters.login_attempts.clone(),
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
        Arc::new(SurrealUserStore {
            store: store.clone(),
        }),
        state.limiters.redeem_attempts.clone(),
        state.limiters.api_key_rate.clone(),
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
            Arc::new(SurrealProviderStore {
                store: store.clone(),
            }),
            Arc::new(GatewayProbes {
                store: store.clone(),
                clients: state.clients.clone(),
                config: config.clone(),
            }),
            Arc::new(ZcodeOAuthImpl {
                base: config.zcode_oauth_base.clone(),
            }),
            Arc::new(ZcodeClaimImpl),
        ),
        users: aikun_admin::admin_users::AdminUsersService::new(Arc::new(SurrealAdminUsersStore {
            store: store.clone(),
        })),
        billing: aikun_admin::admin_billing::AdminBillingService::new(Arc::new(
            SurrealBillingStore {
                store: store.clone(),
            },
        )),
        stats: aikun_admin::admin_stats::AdminStatsService::new(Arc::new(SurrealStatsStore {
            store: store.clone(),
        })),
    };

    // zcode 每日自动领取(签到任务;AIKUN_ZCODE_AUTO_CLAIM 可关)
    crate::zcode::scheduler::spawn_auto_claim(store.clone(), admin_service.clone(), &config);
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
    // Proxy tag 8 个显式端点由 openapi-to-rust 生成(2026-11-18 纳入,
    // 装配层 relay::RelayService 实现生成 trait;/v1/{*path} 通配兜底是
    // OpenAPI 表达不了的形态,保留手写)。中间件链(外→内):
    // auth_middleware 验证并塞 extensions → relay_scope 取出连同请求头
    // 放 task_local → problem_to_vendor_error 把生成校验链的 problem+json
    // 错误体改写为厂商格式(管线自产错误本就是厂商格式,原样放行)。
    let relay_routes = aikun_openapi::openapi::server::router::proxy_api_router(
        relay::RelayService(state.clone()),
    )
    // 白名单外路径 404(白名单见 passthrough.rs;显式路由优先于通配)。
    // 通配 handler 吃 State<AppState>,独立建 Router 后供给 state 再 merge。
    .merge(
        Router::new()
            .route("/v1/{*path}", post(api::proxy::passthrough::passthrough))
            .with_state(state.clone()),
    )
    .layer(middleware::from_fn(relay::problem_to_vendor_error))
    .layer(middleware::from_fn(relay::relay_scope))
    .layer(middleware::from_fn_with_state(
        state.clone(),
        auth_middleware,
    ));

    // Spawn the health check background task under a supervisor: if the loop
    // ever panics or exits, log it and restart after a short backoff so
    // health tracking cannot silently die.
    let health_store = store.clone();
    let health_config = config.clone();
    let health_clients = state.clients.clone();
    tokio::spawn(async move {
        use futures_util::FutureExt;
        loop {
            let result =
                std::panic::AssertUnwindSafe(crate::channel::health::run_health_check_loop(
                    health_store.clone(),
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
    let model_test_store = store.clone();
    let model_test_config = config.clone();
    let model_test_clients = state.clients.clone();
    tokio::spawn(async move {
        use futures_util::FutureExt;
        loop {
            let result =
                std::panic::AssertUnwindSafe(crate::channel::model_test::run_model_test_loop(
                    model_test_store.clone(),
                    model_test_config.clone(),
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
        .with_state(());

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

    // HTTP 排空后显式关库:flush WAL → close。仅快照模式(sync=<间隔>,
    // 默认配置)下这是"正常重启零丢失"的保证——已提交但还在内存/WAL
    // 缓冲的写入在此全部落盘;不调用则依赖 Drop 时机,进程退出路径上
    // 不可靠。上游关库自带超时语义,这里再兜 15s 防拖死退出。
    match tokio::time::timeout(std::time::Duration::from_secs(15), store.shutdown()).await {
        Ok(()) => {}
        Err(_) => tracing::error!("SurrealDB 优雅关闭超时(15s),仅快照模式下最后一段写入可能丢失"),
    }
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

// ---------- Auth tag 适配器(SurrealDB;阶段 S 默认切换) ----------

/// aikun-admin 的登录端口由 Surreal 身份域实现;哈希/签发复用二进制
/// crate::auth(aikun-store 的 password 模块)。时间出参统一 RFC3339
/// (aikun-admin 的 sqlite_utc 助手兼容两种格式)。
struct SurrealAuthStore {
    store: Arc<AppStore>,
    config: Arc<AppConfig>,
}

#[async_trait::async_trait]
impl aikun_admin::auth::AuthStore for SurrealAuthStore {
    /// 查询错误与"用户不存在"一律 Ok(None)(旧实现 `.ok()` 语义,统一
    /// 401 防枚举)。
    async fn find_login_user(
        &self,
        username: &str,
    ) -> Result<Option<aikun_admin::auth::LoginUserRecord>, ()> {
        Ok(self
            .store
            .identity
            .find_login_user(username)
            .await
            .ok()
            .flatten()
            .map(|u| aikun_admin::auth::LoginUserRecord {
                id: u.id,
                username: u.username,
                password_hash: u.password_hash,
                display_name: u.display_name,
                role: u.role,
                is_active: u.is_active,
                created_at: u.created_at.to_rfc3339(),
                updated_at: u.updated_at.to_rfc3339(),
                token_version: u.token_version,
            }))
    }

    async fn verify_password(&self, password: &str, hash: &str) -> bool {
        // Argon2 CPU 密集:派发 blocking 线程池(语义同旧)。
        let (password, hash) = (password.to_string(), hash.to_string());
        tokio::task::spawn_blocking(move || {
            crate::auth::verify_password(&password, &hash).unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    }

    fn dummy_password_hash(&self) -> &'static str {
        // A valid Argon2 hash used to equalize response timing when the
        // requested username does not exist (prevents account-enumeration
        // via timing)。
        static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        DUMMY.get_or_init(|| {
            crate::auth::hash_password("dummy-password").unwrap_or_else(|_| {
                // Valid PHC string for "password"; only used for timing equalization.
                "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$2hV3t0s4rVdXKZ2dKjSZUw".to_string()
            })
        })
    }

    async fn create_token(&self, user: &aikun_admin::auth::LoginUserRecord) -> Result<String, ()> {
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

// ---------- User tag 适配器(SurrealUserStore;身份域 + 计费域) ----------

use aikun_admin::user as user_port;
use aikun_store::surreal::billing_store as sbilling;
use aikun_store::surreal::identity as sidentity;

struct SurrealUserStore {
    store: Arc<AppStore>,
}

/// Surreal api_key 行 → 端口记录(models 数组 → JSON 文本)。
fn api_key_record(row: &sidentity::ApiKeyRow) -> user_port::ApiKeyRecord {
    user_port::ApiKeyRecord {
        id: row.id.clone(),
        user_id: row.user_id.clone(),
        key_suffix: row.key_suffix.clone(),
        name: row.name.clone(),
        is_active: row.is_active,
        last_used_at: row.last_used_at.map(|t| t.to_rfc3339()),
        expires_at: row.expires_at.map(|t| t.to_rfc3339()),
        models_json: serde_json::to_string(&row.models).unwrap_or_else(|_| "[]".to_string()),
        rate_limit_rpm: row.rate_limit_rpm,
        quota_daily_tokens: row.quota_daily_tokens,
        max_concurrent: row.max_concurrent,
        created_at: row.created_at.to_rfc3339(),
    }
}

/// 端口日志过滤 → Surreal 计费域过滤(success 0/1 → bool;since 原文
/// 宽松解析为 datetime,不可解析忽略该条件)。
fn surreal_log_filters(f: &user_port::LogFilters) -> sbilling::LogFilters {
    sbilling::LogFilters {
        user_id: f.user_id.clone(),
        api_key_id: None,
        provider_id: None,
        model: f.model.clone(),
        success: f.success.map(|s| s == 1),
        since: f.since.as_deref().and_then(crate::auth::parse_expiry),
        until: None,
    }
}

#[async_trait::async_trait]
impl user_port::UserStore for SurrealUserStore {
    async fn find_user_with_balance(
        &self,
        user_id: &str,
    ) -> Result<Option<user_port::UserBalanceRecord>, ()> {
        Ok(self
            .store
            .identity
            .find_user(user_id)
            .await
            .ok()
            .flatten()
            .map(|u| user_port::UserBalanceRecord {
                id: u.id,
                username: u.username,
                display_name: u.display_name,
                role: u.role,
                is_active: u.is_active,
                created_at: u.created_at.to_rfc3339(),
                updated_at: u.updated_at.to_rfc3339(),
                balance_micro: u.balance_micro,
            }))
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

    async fn insert_api_key(
        &self,
        row: &user_port::NewApiKey,
    ) -> Result<user_port::ApiKeyRecord, ()> {
        // 建行(模型清单转原生数组)→ 一次性补齐过期时刻与三项限额
        // (桩契约 NewApiKey 只带核心列,补丁走 update_api_key)。
        let models: Vec<String> = serde_json::from_str(&row.models_json).unwrap_or_default();
        let created = self
            .store
            .identity
            .insert_api_key(sidentity::NewApiKey {
                id: Some(row.id.clone()),
                user_id: row.user_id.clone(),
                key_hash: row.key_hash.clone(),
                key_suffix: row.key_suffix.clone(),
                name: row.name.clone(),
                models,
            })
            .await
            .map_err(|e| tracing::error!("SurrealUserStore::insert_api_key: {e}"))?;
        let expires = row
            .expires_at
            .as_deref()
            .and_then(crate::auth::parse_expiry);
        let patch = sidentity::ApiKeyPatch {
            expires_at: Some(expires),
            rate_limit_rpm: Some(row.rate_limit_rpm),
            quota_daily_tokens: Some(row.quota_daily_tokens),
            max_concurrent: Some(row.max_concurrent),
            ..Default::default()
        };
        let row = self
            .store
            .identity
            .update_api_key(&created.id, None, patch)
            .await
            .map_err(|e| tracing::error!("SurrealUserStore::insert_api_key(patch): {e}"))?
            .unwrap_or(created);
        Ok(api_key_record(&row))
    }

    async fn list_api_keys(
        &self,
        target_user_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<user_port::ApiKeyRecord>), ()> {
        let (total, rows) = self
            .store
            .identity
            .list_api_keys(target_user_id, limit, offset)
            .await
            .map_err(|_| ())?;
        Ok((total, rows.iter().map(api_key_record).collect()))
    }

    async fn update_api_key(
        &self,
        key_id: &str,
        owner: Option<&str>,
        updates: &user_port::ApiKeyUpdates,
    ) -> Result<Option<user_port::ApiKeyRecord>, ()> {
        let patch = sidentity::ApiKeyPatch {
            name: updates.name.clone(),
            is_active: updates.is_active,
            // Some(None) = 清除(永不过期);Some(Some(s)) = 设置
            //(服务层已校验可解析的未来时刻)。
            expires_at: updates
                .expires_at
                .as_ref()
                .map(|e| e.as_deref().and_then(crate::auth::parse_expiry)),
            models: updates
                .models_json
                .as_ref()
                .map(|j| serde_json::from_str(j).unwrap_or_default()),
            rate_limit_rpm: updates.rate_limit_rpm,
            quota_daily_tokens: updates.quota_daily_tokens,
            max_concurrent: updates.max_concurrent,
        };
        Ok(self
            .store
            .identity
            .update_api_key(key_id, owner, patch)
            .await
            .map_err(|e| tracing::error!("SurrealUserStore::update_api_key: {e}"))?
            .as_ref()
            .map(api_key_record))
    }

    async fn delete_api_key(&self, key_id: &str, owner: Option<&str>) -> Result<i64, ()> {
        let deleted = self
            .store
            .identity
            .delete_api_key(key_id, owner)
            .await
            .map_err(|e| tracing::error!("SurrealUserStore::delete_api_key: {e}"))?;
        Ok(i64::from(deleted))
    }

    async fn query_logs(
        &self,
        filters: &user_port::LogFilters,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<user_port::LogRecord>), ()> {
        let (total, rows) = self
            .store
            .billing
            .query_logs(&surreal_log_filters(filters), limit, offset)
            .await
            .map_err(|e| tracing::error!("SurrealUserStore::query_logs: {e}"))?;
        Ok((
            total,
            rows.into_iter()
                .map(|r| user_port::LogRecord {
                    id: r.id,
                    user_id: r.user_id,
                    api_key_id: r.api_key_id,
                    provider_id: r.provider_id,
                    username: r.username,
                    provider_name: r.provider_name,
                    model: r.model,
                    request_type: r.request_type,
                    prompt_tokens: i64::from(r.prompt_tokens),
                    completion_tokens: i64::from(r.completion_tokens),
                    total_tokens: i64::from(r.total_tokens),
                    cached_tokens: i64::from(r.cached_tokens),
                    latency_ms: r.latency_ms,
                    status_code: r.status_code,
                    success: r.success,
                    error_message: r.error_message,
                    cost_micro: r.cost_micro,
                    created_at: r.created_at.to_rfc3339(),
                })
                .collect(),
        ))
    }

    async fn query_log_stats(
        &self,
        filters: &user_port::LogFilters,
    ) -> Result<user_port::LogStatsRaw, ()> {
        self.store
            .billing
            .log_stats(&surreal_log_filters(filters))
            .await
            .map(|s| user_port::LogStatsRaw {
                total_requests: s.total_requests,
                total_tokens: s.total_tokens,
                avg_latency_ms: s.avg_latency_ms,
                success_rate_pct: s.success_rate_pct,
                total_cost_micro: s.total_cost_micro,
            })
            .map_err(|e| {
                tracing::error!("SurrealUserStore::query_log_stats: {e}");
            })
    }

    async fn wallet_snapshot(
        &self,
        user_id: &str,
        since_date: &str,
    ) -> Result<user_port::WalletSnapshot, ()> {
        let balance_micro = self
            .store
            .identity
            .find_user(user_id)
            .await
            .map_err(|_| ())?
            .ok_or(())?
            .balance_micro;
        let series = self
            .store
            .billing
            .wallet_series(user_id, since_date)
            .await
            .map_err(|_| ())?;
        Ok(user_port::WalletSnapshot {
            balance_micro,
            daily: series.daily,
            top_models: series.top_models,
        })
    }

    async fn available_prices(&self) -> Result<Vec<(String, Option<user_port::PriceTriple>)>, ()> {
        // 健康渠道提供的全部模型(拓扑快照,已排序)逐个经价格缓存展开;
        // 未定价三价出 null。
        let models = crate::channel::registry::shared()
            .current(&self.store)
            .await
            .available_models();
        let rules = self.store.billing.price_rules().await.map_err(|_| ())?;
        Ok(models
            .into_iter()
            .map(|m| {
                let price = aikun_store::pricing::find_in_rules(&rules, &m);
                (m, price)
            })
            .collect())
    }

    fn hash_code(&self, code: &str) -> String {
        crate::auth::hash_api_key(code)
    }

    async fn redeem(&self, user_id: &str, code_hash: &str) -> Result<Option<(i64, i64)>, ()> {
        // 红线 2:核销事务原子性在计费域实现(单次 BEGIN…COMMIT)。
        match self.store.billing.redeem_code(user_id, code_hash).await {
            Ok(sbilling::RedeemOutcome::Redeemed {
                amount_micro,
                balance_after_micro,
            }) => Ok(Some((amount_micro, balance_after_micro))),
            Ok(sbilling::RedeemOutcome::Invalid) => Ok(None),
            Err(e) => {
                tracing::error!("SurrealUserStore::redeem: {e}");
                Err(())
            }
        }
    }
}

// ---------- Admin·Users 适配器(SurrealAdminUsersStore) ----------

use aikun_admin::admin_users as users_port;
use aikun_store::surreal::identity::UserRow;

struct SurrealAdminUsersStore {
    store: Arc<AppStore>,
}

fn admin_user_record(u: &UserRow) -> users_port::AdminUserRecord {
    users_port::AdminUserRecord {
        id: u.id.clone(),
        username: u.username.clone(),
        display_name: u.display_name.clone(),
        role: u.role.clone(),
        is_active: u.is_active,
        created_at: u.created_at.to_rfc3339(),
        updated_at: u.updated_at.to_rfc3339(),
        balance_micro: u.balance_micro,
    }
}

/// 是否为唯一活跃 admin(last_admin 409 守卫;旧 SQL 在同一把写锁内
/// 判定,Surreal 侧读-判定两查,窗口内并发的降级/禁用可能同时通过——
/// 管理端 PATCH 已被服务级 patch_lock 串行化,残余竞态仅剩绕过本服务
/// 的直写,语义可接受)。
async fn last_active_admin(store: &AppStore, user_id: &str) -> bool {
    match store.identity.find_user(user_id).await {
        Ok(Some(u)) if u.role == "admin" && u.is_active => matches!(
            store.identity.count_active_admins().await,
            Ok(n) if n <= 1
        ),
        _ => false,
    }
}

#[async_trait::async_trait]
impl users_port::AdminUsersStore for SurrealAdminUsersStore {
    async fn list_users(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<users_port::AdminUserRecord>, i64), users_port::UsersStoreError> {
        let (total, rows) = self
            .store
            .identity
            .list_users("", limit, offset)
            .await
            .map_err(|_| users_port::UsersStoreError::Unavailable)?;
        Ok((rows.iter().map(admin_user_record).collect(), total))
    }

    async fn find_user(
        &self,
        id: &str,
    ) -> Result<Option<users_port::AdminUserRecord>, users_port::UsersStoreError> {
        Ok(self
            .store
            .identity
            .find_user(id)
            .await
            .map_err(|_| users_port::UsersStoreError::Unavailable)?
            .as_ref()
            .map(admin_user_record))
    }

    async fn insert_user(
        &self,
        user: &users_port::NewUser,
    ) -> Result<users_port::InsertOutcome, users_port::UsersStoreError> {
        match self
            .store
            .identity
            .create_user(sidentity::NewUser {
                id: None,
                username: user.username.clone(),
                password_hash: user.password_hash.clone(),
                display_name: user.display_name.clone(),
                role: user.role.clone(),
            })
            .await
        {
            Ok(u) => Ok(users_port::InsertOutcome::Created(u.id)),
            Err(e) if e.to_string().contains("unique_user_username") => {
                Ok(users_port::InsertOutcome::UsernameExists)
            }
            Err(e) => {
                tracing::error!("SurrealAdminUsersStore::insert_user: {e}");
                Err(users_port::UsersStoreError::Failed)
            }
        }
    }

    async fn insert_users_batch(
        &self,
        users: &[users_port::PreparedUser],
    ) -> Result<Vec<users_port::BatchInsertItem>, users_port::UsersStoreError> {
        // 逐条插入(单项失败不影响其余,旧相位 2 语义;每条一次往返)。
        let mut out = Vec::with_capacity(users.len());
        for p in users {
            let outcome = match self
                .store
                .identity
                .create_user(sidentity::NewUser {
                    id: None,
                    username: p.username.clone(),
                    password_hash: p.password_hash.clone(),
                    display_name: p.display_name.clone(),
                    role: p.role.clone(),
                })
                .await
            {
                Ok(u) => users_port::BatchInsertItem::Created(u.id),
                Err(e) if e.to_string().contains("unique_user_username") => {
                    users_port::BatchInsertItem::UsernameExists
                }
                Err(e) => {
                    tracing::error!(
                        "SurrealAdminUsersStore::insert_users_batch {}: {e}",
                        p.username
                    );
                    users_port::BatchInsertItem::Failed
                }
            };
            out.push(outcome);
        }
        Ok(out)
    }

    async fn update_user(
        &self,
        id: &str,
        patch: &users_port::UserPatch,
    ) -> Result<users_port::UpdateOutcome, users_port::UsersStoreError> {
        // last_admin 保护:降级或禁用唯一活跃 admin 前置拦截。
        if patch.demotes_or_disables && last_active_admin(&self.store, id).await {
            return Ok(users_port::UpdateOutcome::LastAdmin);
        }
        match self
            .store
            .identity
            .update_user(
                id,
                patch.display_name.clone(),
                patch.role.clone(),
                patch.is_active,
                patch.password_hash.clone(),
            )
            .await
        {
            Ok(Some(u)) => Ok(users_port::UpdateOutcome::Updated(Box::new(
                admin_user_record(&u),
            ))),
            Ok(None) => Ok(users_port::UpdateOutcome::NotFound),
            Err(e) => {
                tracing::error!("SurrealAdminUsersStore::update_user: {e}");
                Err(users_port::UsersStoreError::Failed)
            }
        }
    }

    async fn disable_user(
        &self,
        id: &str,
    ) -> Result<users_port::DisableOutcome, users_port::UsersStoreError> {
        if last_active_admin(&self.store, id).await {
            return Ok(users_port::DisableOutcome::LastAdmin);
        }
        match self
            .store
            .identity
            .update_user(id, None, None, Some(false), None)
            .await
        {
            Ok(Some(_)) => Ok(users_port::DisableOutcome::Disabled),
            Ok(None) => Ok(users_port::DisableOutcome::NotFound),
            Err(e) => {
                tracing::error!("SurrealAdminUsersStore::disable_user: {e}");
                Err(users_port::UsersStoreError::Failed)
            }
        }
    }

    async fn hash_password(&self, password: &str) -> Result<String, ()> {
        let password = password.to_string();
        tokio::task::spawn_blocking(move || crate::auth::hash_password(&password))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())
    }

    fn random_password(&self) -> String {
        crate::auth::generate_random_password()
    }
}

// ---------- Admin·Billing 适配器(SurrealBillingStore) ----------

use aikun_admin::admin_billing as billing_port;

struct SurrealBillingStore {
    store: Arc<AppStore>,
}

/// 兑换码码字符集:去除易混淆的 0/O/1/I(迁自旧 api/redemption.rs)。
const CODE_CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// 价格行错误映射:唯一索引冲突 → Constraint(调用方 409/500 code_collision)。
fn price_err(e: aikun_store::StoreError) -> billing_port::BillingStoreError {
    if e.to_string().contains("unique_model_price_model") {
        billing_port::BillingStoreError::Constraint
    } else {
        tracing::error!("SurrealBillingStore price op failed: {e}");
        billing_port::BillingStoreError::Failed(e.to_string())
    }
}

fn price_record(r: &sbilling::PriceRecordRow) -> billing_port::PriceRecord {
    billing_port::PriceRecord {
        id: r.id.clone(),
        model: r.model.clone(),
        prompt_price: r.prompt_price,
        completion_price: r.completion_price,
        cached_price: r.cached_price,
        created_at: r.created_at.to_rfc3339(),
        updated_at: r.updated_at.to_rfc3339(),
    }
}

#[async_trait::async_trait]
impl billing_port::BillingStore for SurrealBillingStore {
    async fn all_prices(
        &self,
    ) -> Result<Vec<billing_port::PriceRecord>, billing_port::BillingStoreError> {
        self.store
            .billing
            .list_price_records()
            .await
            .map(|rows| rows.iter().map(price_record).collect())
            .map_err(price_err)
    }

    async fn all_channel_models(&self) -> std::collections::HashSet<String> {
        // 查询失败静默返回空集(旧 channel_models 的 if let Ok 语义)。
        self.store
            .channels
            .all_channel_models()
            .await
            .unwrap_or_default()
    }

    async fn insert_price(
        &self,
        model: &str,
        prompt_price: f64,
        completion_price: f64,
        cached_price: Option<f64>,
    ) -> Result<billing_port::PriceRecord, billing_port::BillingStoreError> {
        self.store
            .billing
            .insert_price_record(model, prompt_price, completion_price, cached_price)
            .await
            .map(|r| price_record(&r))
            .map_err(price_err)
    }

    async fn update_price(
        &self,
        id: &str,
        model: Option<&str>,
        prompt_price: Option<f64>,
        completion_price: Option<f64>,
        cached_price: Option<Option<f64>>,
    ) -> Result<Option<billing_port::PriceRecord>, billing_port::BillingStoreError> {
        self.store
            .billing
            .update_price_record(id, model, prompt_price, completion_price, cached_price)
            .await
            .map(|r| r.as_ref().map(price_record))
            .map_err(price_err)
    }

    async fn delete_price(&self, id: &str) -> Result<bool, billing_port::BillingStoreError> {
        self.store
            .billing
            .delete_price_record(id)
            .await
            .map_err(price_err)
    }

    async fn adjust_balance(
        &self,
        user_id: &str,
        amount_micro: i64,
        kind: &str,
        note: &str,
    ) -> Result<Option<i64>, billing_port::BillingStoreError> {
        // 先确认用户存在(端口 None → 404);余额写 + 流水同事务在计费域。
        match self.store.identity.find_user(user_id).await {
            Ok(Some(_)) => {}
            Ok(None) => return Ok(None),
            Err(e) => {
                tracing::error!("SurrealBillingStore::adjust_balance lookup: {e}");
                return Err(billing_port::BillingStoreError::Failed(e.to_string()));
            }
        }
        self.store
            .billing
            .adjust_balance(user_id, amount_micro, kind, note)
            .await
            .map(Some)
            .map_err(|e| {
                tracing::error!("SurrealBillingStore::adjust_balance: {e}");
                billing_port::BillingStoreError::Failed(e.to_string())
            })
    }

    async fn list_transactions(
        &self,
        user_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<billing_port::TxRecord>), billing_port::BillingStoreError> {
        let (total, rows) = self
            .store
            .billing
            .list_transactions_scoped(user_id, limit, offset)
            .await
            .map_err(|e| billing_port::BillingStoreError::Failed(e.to_string()))?;
        Ok((
            total,
            rows.into_iter()
                .map(|r| billing_port::TxRecord {
                    id: r.id,
                    user_id: r.user_id,
                    username: r.username,
                    amount_micro: r.amount_micro,
                    balance_after_micro: r.balance_after_micro,
                    kind: r.kind,
                    note: r.note,
                    created_at: r.created_at.to_rfc3339(),
                })
                .collect(),
        ))
    }

    async fn list_redemption_codes(
        &self,
        batch: Option<&str>,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<billing_port::RedemptionCodeRecord>), billing_port::BillingStoreError>
    {
        let (total, rows) = self
            .store
            .billing
            .list_redemption_codes(batch.unwrap_or(""), status, limit, offset)
            .await
            .map_err(|e| billing_port::BillingStoreError::Failed(e.to_string()))?;
        let now = chrono::Utc::now();
        Ok((
            total,
            rows.into_iter()
                .map(|r| {
                    let expired = r.status == "unused" && r.expires_at.is_some_and(|t| t <= now);
                    billing_port::RedemptionCodeRecord {
                        id: r.id,
                        code_suffix: r.code_suffix,
                        amount_micro: r.amount_micro,
                        batch: r.batch,
                        status: r.status,
                        used_by_username: r.used_by,
                        used_at: r.used_at.map(|t| t.to_rfc3339()),
                        expires_at: r.expires_at.map(|t| t.to_rfc3339()),
                        note: r.note,
                        created_at: r.created_at.to_rfc3339(),
                        expired,
                    }
                })
                .collect(),
        ))
    }

    async fn insert_redemption_codes(
        &self,
        rows: &[billing_port::NewRedemptionCode],
    ) -> Result<(), billing_port::BillingStoreError> {
        // expires_at 原文宽松解析(服务层已限定格式);整批一个事务
        //(计费域 create_redemption_codes,中途失败整体回滚)。
        let new: Vec<sbilling::NewRedemptionCode> = rows
            .iter()
            .map(|r| sbilling::NewRedemptionCode {
                code_hash: r.code_hash.clone(),
                code_suffix: r.code_suffix.clone(),
                amount_micro: r.amount_micro,
                batch: r.batch.clone(),
                expires_at: r.expires_at.as_deref().and_then(crate::auth::parse_expiry),
                note: r.note.clone(),
            })
            .collect();
        self.store
            .billing
            .create_redemption_codes(new)
            .await
            .map(|_| ())
            .map_err(|e| {
                if e.to_string().contains("unique_redemption_code_hash") {
                    billing_port::BillingStoreError::Constraint
                } else {
                    tracing::error!("SurrealBillingStore::insert_redemption_codes: {e}");
                    billing_port::BillingStoreError::Failed(e.to_string())
                }
            })
    }

    async fn disable_redemption_code(
        &self,
        id: &str,
    ) -> Result<billing_port::DisableOutcome, billing_port::BillingStoreError> {
        match self
            .store
            .billing
            .disable_redemption_code(id)
            .await
            .map_err(|e| billing_port::BillingStoreError::Failed(e.to_string()))?
        {
            true => Ok(billing_port::DisableOutcome::Disabled),
            // 影响行数 0:回查存在性区分"非 unused"与"不存在"。
            false => match self
                .store
                .billing
                .get_redemption_code(id)
                .await
                .map_err(|e| billing_port::BillingStoreError::Failed(e.to_string()))?
            {
                Some(_) => Ok(billing_port::DisableOutcome::NotUnused),
                None => Ok(billing_port::DisableOutcome::NotFound),
            },
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
        crate::auth::hash_api_key(code)
    }
}

// ---------- Admin·Providers 适配器(SurrealProviderStore + GatewayProbes;
// 渠道域经 aikun-store 的 ChannelStore) ----------

use aikun_admin::admin_providers as providers_port;
use aikun_store::surreal::channels_store as schannels;

struct SurrealProviderStore {
    store: Arc<AppStore>,
}

struct GatewayProbes {
    store: Arc<AppStore>,
    clients: Arc<Mutex<HashMap<String, reqwest::Client>>>,
    /// 9.2 三类超时取值(连接/首字节),探测路径与请求管线同源。
    config: Arc<AppConfig>,
}

#[async_trait::async_trait]
impl providers_port::ProviderStore for SurrealProviderStore {
    fn new_provider_id(&self) -> String {
        // 新建记录用 ULID(时间可排序;迁移旧行沿用旧 uuid,设计文档 §三)。
        ulid::Ulid::new().to_string()
    }

    fn encrypt_key(&self, plaintext: &str) -> String {
        self.store.cipher.encrypt(plaintext)
    }

    fn decrypt_key(&self, stored: &str) -> String {
        crate::crypto::decrypt_or_plain(&self.store.cipher, stored)
    }

    fn hash_key(&self, key: &str) -> String {
        crate::auth::hash_api_key(key)
    }

    async fn list_providers(
        &self,
        category: Option<&str>,
        kinds: Option<&[String]>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<aikun_channels::Provider>, i64), ()> {
        let (total, rows) = self
            .store
            .channels
            .list_providers_by_priority(category, kinds, limit, offset)
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::list_providers: {e}"))?;
        Ok((rows, total))
    }

    async fn upsert_proxy_task(
        &self,
        row: aikun_store::surreal::channels_store::ProxyTaskRow,
    ) -> Result<(), ()> {
        self.store
            .channels
            .upsert_proxy_task(row)
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::upsert_proxy_task: {e}"))
    }

    async fn list_proxy_tasks(
        &self,
        task_type: Option<&str>,
    ) -> Result<Vec<aikun_store::surreal::channels_store::ProxyTaskRow>, ()> {
        self.store
            .channels
            .list_proxy_tasks(task_type)
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::list_proxy_tasks: {e}"))
    }

    async fn create_proxy(
        &self,
        row: aikun_store::surreal::channels_store::NewProxyRow,
    ) -> Result<aikun_store::surreal::channels_store::ProxyRow, ()> {
        self.store
            .channels
            .create_proxy(row)
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::create_proxy: {e}"))
    }

    async fn delete_proxy(&self, id: &str) -> Result<bool, ()> {
        self.store
            .channels
            .delete_proxy(id)
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::delete_proxy: {e}"))
    }

    async fn list_proxies(
        &self,
    ) -> Result<Vec<aikun_store::surreal::channels_store::ProxyRow>, ()> {
        self.store
            .channels
            .list_proxies()
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::list_proxies: {e}"))
    }

    async fn find_provider(&self, id: &str) -> Result<Option<aikun_channels::Provider>, ()> {
        self.store
            .channels
            .get_provider(id)
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::find_provider: {e}"))
    }

    async fn read_provider_config(
        &self,
        id: &str,
    ) -> Result<Option<providers_port::ProviderConfig>, ()> {
        Ok(self
            .store
            .channels
            .read_provider_stored(id)
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::read_provider_config: {e}"))?
            .map(|c| providers_port::ProviderConfig {
                name: c.name,
                provider_type: c.provider_type,
                openai_base_url: c.openai_base_url,
                anthropic_base_url: c.anthropic_base_url,
                api_key_stored: c.api_key_stored,
                key_hash: c.key_hash,
                models_json: c.models_json,
                priority: c.priority,
                weight: c.weight,
                max_retries: c.max_retries,
                timeout_secs: c.timeout_secs,
                proxy_url: c.proxy_url,
                model_mapping_json: c.model_mapping_json,
                protocols_json: c.protocols_json,
                default_protocol: c.default_protocol,
                note: c.note,
                website_url: c.website_url,
                channel_kind: c.channel_kind,
            }))
    }

    async fn insert_provider(
        &self,
        id: &str,
        config: &providers_port::ProviderConfig,
    ) -> Result<(), ()> {
        // 密文与 key_hash 由服务层算好原样落库(管理面已加密)。
        self.store
            .channels
            .insert_provider_stored(
                id,
                &schannels::StoredProviderConfig {
                    name: config.name.clone(),
                    provider_type: config.provider_type.clone(),
                    openai_base_url: config.openai_base_url.clone(),
                    anthropic_base_url: config.anthropic_base_url.clone(),
                    api_key_stored: config.api_key_stored.clone(),
                    key_hash: config.key_hash.clone(),
                    models_json: config.models_json.clone(),
                    priority: config.priority,
                    weight: config.weight,
                    max_retries: config.max_retries,
                    timeout_secs: config.timeout_secs,
                    proxy_url: config.proxy_url.clone(),
                    model_mapping_json: config.model_mapping_json.clone(),
                    protocols_json: config.protocols_json.clone(),
                    default_protocol: config.default_protocol.clone(),
                    note: config.note.clone(),
                    website_url: config.website_url.clone(),
                    channel_kind: config.channel_kind.clone(),
                },
            )
            .await
            .map(|_| ())
            .map_err(|e| tracing::error!("SurrealProviderStore::insert_provider: {e}"))
    }

    async fn find_key_owner(&self, key_hash: &str, exclude_id: &str) -> Result<Option<String>, ()> {
        self.store
            .channels
            .find_key_owner(key_hash, exclude_id)
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::find_key_owner: {e}"))
    }

    async fn update_provider(
        &self,
        id: &str,
        patch: &providers_port::ProviderPatch,
    ) -> Result<u64, ()> {
        let stored = schannels::StoredProviderPatch {
            name: patch.name.clone(),
            provider_type: patch.provider_type.clone(),
            openai_base_url: patch.openai_base_url.clone(),
            anthropic_base_url: patch.anthropic_base_url.clone(),
            note: patch.note.clone(),
            website_url: patch.website_url.clone(),
            channel_kind: patch.channel_kind.clone(),
            api_key_stored: patch.api_key_stored.clone(),
            key_hash: patch.key_hash.clone(),
            models_json: patch.models_json.clone(),
            priority: patch.priority,
            weight: patch.weight,
            is_active: patch.is_active,
            max_retries: patch.max_retries,
            timeout_secs: patch.timeout_secs,
            proxy_url: patch.proxy_url.clone(),
            model_mapping_json: patch.model_mapping_json.clone(),
            protocols_json: patch.protocols_json.clone(),
            default_protocol: patch.default_protocol.clone(),
        };
        match self.store.channels.apply_provider_patch(id, stored).await {
            Ok(Some(_)) => Ok(1),
            Ok(None) => Ok(0),
            Err(e) => {
                tracing::error!("SurrealProviderStore::update_provider: {e}");
                Err(())
            }
        }
    }

    async fn delete_provider(&self, id: &str) -> Result<u64, ()> {
        let deleted = self
            .store
            .channels
            .delete_provider(id)
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::delete_provider: {e}"))?;
        Ok(u64::from(deleted))
    }

    async fn update_health(&self, id: &str, status: &str, _checked_at: &str) -> Result<(), ()> {
        // 只写 health_status + last_health_check(实现侧 time::now();
        // 刻意不更新 latency_ms——ping 延迟不代表真实请求延迟)。
        self.store
            .channels
            .record_health_ping(id, status)
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::update_health: {e}"))
    }

    async fn record_model_health(
        &self,
        provider_id: &str,
        model: &str,
        ok: bool,
        latency_ms: f64,
        error: Option<&str>,
    ) {
        // model_health upsert + 渠道 EMA 记账(与代理转发路径同口径);
        // 尽力而为,失败只留痕。
        let _ = crate::channel::model_test::record_model_health(
            &self.store,
            provider_id,
            model,
            ok,
            latency_ms,
            error,
        )
        .await;
    }

    async fn list_model_health(
        &self,
        provider_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<providers_port::ModelHealthRow>, i64), ()> {
        let (total, rows) = self
            .store
            .channels
            .list_model_health_scoped(provider_id, limit, offset)
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::list_model_health: {e}"))?;
        Ok((
            rows.into_iter()
                .map(|r| providers_port::ModelHealthRow {
                    provider_id: r.provider_id,
                    model: r.model,
                    status: r.status,
                    latency_ms: r.latency_ms,
                    error: r.error,
                    checked_at: r.checked_at.map(|t| t.to_rfc3339()),
                })
                .collect(),
            total,
        ))
    }

    async fn find_provider_credentials(&self, id: &str) -> Result<Option<(String, String)>, ()> {
        // 编辑态回退用:库存 (api_key 原文, proxy_url)。
        Ok(self
            .store
            .channels
            .read_provider_stored(id)
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::find_provider_credentials: {e}"))?
            .map(|c| {
                (
                    crate::crypto::decrypt_or_plain(&self.store.cipher, &c.api_key_stored),
                    c.proxy_url,
                )
            }))
    }

    async fn upsert_zcode_oauth(&self, provider_id: &str, token_enc: &str) -> Result<(), ()> {
        self.store
            .channels
            .upsert_zcode_oauth(aikun_store::surreal::channels_store::ZcodeOauthRow {
                provider_id: provider_id.to_string(),
                business_token: token_enc.to_string(),
                refreshed_at: String::new(),
            })
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::upsert_zcode_oauth: {e}"))
    }

    async fn find_zcode_oauth(&self, provider_id: &str) -> Result<Option<String>, ()> {
        Ok(self
            .store
            .channels
            .find_zcode_oauth(provider_id)
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::find_zcode_oauth: {e}"))?
            .map(|r| r.business_token))
    }

    async fn find_provider_id_by_key_hash(&self, key_hash: &str) -> Result<Option<String>, ()> {
        self.store
            .channels
            .find_provider_id_by_key_hash(key_hash)
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::find_provider_id_by_key_hash: {e}"))
    }

    async fn upsert_zcode_account(&self, provider_id: &str, info_json: &str) -> Result<(), ()> {
        self.store
            .channels
            .upsert_zcode_account(aikun_store::surreal::channels_store::ZcodeAccountRow {
                provider_id: provider_id.to_string(),
                info: info_json.to_string(),
                fetched_at: String::new(),
            })
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::upsert_zcode_account: {e}"))
    }

    async fn find_zcode_account(&self, provider_id: &str) -> Result<Option<(String, String)>, ()> {
        Ok(self
            .store
            .channels
            .find_zcode_account(provider_id)
            .await
            .map_err(|e| tracing::error!("SurrealProviderStore::find_zcode_account: {e}"))?
            .map(|r| (r.info, r.fetched_at)))
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
            self.config.connect_timeout_secs,
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
            self.config.connect_timeout_secs,
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
            self.config.connect_timeout_secs,
            self.config.ttfb_timeout_secs,
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
        let store = self.store.clone();
        let config = self.config.clone();
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
                &clients,
                &provider,
                protocol,
                body,
                true,
                120,
                config.connect_timeout_secs,
                config.ttfb_timeout_secs,
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
            let _ = crate::channel::model_test::record_model_health(
                &store,
                &pid,
                &model_for_record,
                ok,
                latency,
                err_for_record.as_deref(),
            )
            .await;
        };
        sse_response(stream)
    }
}

// ---------- Admin·Stats 适配器(SurrealStatsStore) ----------

use aikun_admin::admin_stats as stats_port;

struct SurrealStatsStore {
    store: Arc<AppStore>,
}

#[async_trait::async_trait]
impl stats_port::StatsStore for SurrealStatsStore {
    async fn usage_snapshot(&self) -> Result<stats_port::UsageSnapshot, ()> {
        let s = self
            .store
            .billing
            .usage_snapshot()
            .await
            .map_err(|e| tracing::error!("SurrealStatsStore::usage_snapshot: {e}"))?;
        Ok(stats_port::UsageSnapshot {
            today: s.today,
            week: s.week,
            month: s.month,
            daily: s.daily,
            top_models: s.top_models,
        })
    }

    async fn global_counts(&self) -> Result<stats_port::GlobalCounts, ()> {
        let stats = self
            .store
            .billing
            .system_stats()
            .await
            .map_err(|e| tracing::error!("SurrealStatsStore::global_counts: {e}"))?;
        let requests_today = self.store.billing.requests_today().await.unwrap_or(0);
        Ok(stats_port::GlobalCounts {
            api_keys: stats.api_keys,
            users: stats.users,
            providers: stats.providers,
            requests_today,
        })
    }
}
