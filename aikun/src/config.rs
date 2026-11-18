use std::collections::HashMap;
use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub host: SocketAddr,
    pub jwt_secret: String,
    /// 渠道 key 静态加密密钥;缺省时派生自 jwt_secret(见 crypto.rs)。
    pub encryption_key: Option<String>,
    pub jwt_expires_in: i64, // seconds
    /// 旧 SQLite 源库位置(仅随迁语义:显式设 `AIKUN_DATABASE_URL` 且值为
    /// `sqlite://` 时,嵌入式 SurrealKV 目录随迁同目录;2026-11-18 起
    /// 该变量名的**非 sqlite 值就是 SurrealDB 端点**,等价于 surreal_url,
    /// 见 [`resolve_surreal_endpoint`])。
    pub database_url: String,
    /// SurrealDB 连接端点(阶段 S):嵌入式 `surrealkv://<目录>` /
    /// 远程 `ws://`/`wss://`/`http://`/`https://`,可带
    /// `?ns=&db=&user=&pass=`。未设置 = 嵌入式 SurrealKV 默认
    /// `surrealkv://./data/aikun.surreal`(数据统一收进 `data/` 目录;
    /// 旧默认位置 `./aikun.surreal` 存在时自动沿用,见
    /// [`default_surreal_url`])。解析与校验见
    /// `aikun_store::surreal::Endpoint::parse`(非法即拒绝启动)。
    pub surreal_url: String,
    pub health_check_interval: u64, // seconds
    pub max_retries: u32,
    pub auto_disable_threshold: u32, // consecutive failures before auto-disable
    /// 渠道熔断阈值(9.5):连续计入健康的失败达到该次数 → 冷却期内
    /// 选路优先跳过(进程内,不取代自动禁用;429/客户端 4xx 不计入)。
    pub breaker_failure_threshold: u32,
    /// 渠道熔断冷却时长(秒,9.5);到期恢复可选(半开)。
    pub breaker_cooldown_secs: u64,
    pub request_timeout_secs: u64,
    /// TCP/TLS 连接超时(9.2 三类超时之一;原 15s 硬编码改为可配)。
    pub connect_timeout_secs: u64,
    /// 首字节(响应头)超时(9.2):流式请求等上游响应头的上界。
    pub ttfb_timeout_secs: u64,
    /// 流式 chunk 间空闲超时(9.2)。0 = 沿用渠道级 `timeout_secs`
    /// (回退 `request_timeout_secs`)的旧语义;> 0 为全局覆盖。
    pub idle_timeout_secs: u64,
    pub log_retention_days: u32,
    /// CORS 允许的来源白名单;为空表示不加 CORS 层(仅同源)。
    pub cors_allowed_origins: Vec<String>,
    /// 是否信任 X-Forwarded-For 头(仅在反向代理后开启)。
    pub trust_x_forwarded_for: bool,
    // ---- 响应缓存(见 src/cache)----
    pub cache_mode: CacheMode,
    pub cache_ttl_secs: u64,
    pub cache_max_entries: usize,
    pub cache_max_entry_bytes: usize,
    pub cache_scope: CacheScope,
    pub cache_hit_billing: CacheHitBilling,
}

/// 缓存模式:off(默认,与重构前一致)/ deterministic(仅 temperature == 0)/ always。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheMode {
    Off,
    Deterministic,
    Always,
}

/// 缓存作用域:user(默认,按用户隔离)/ global(全站共享,仅单租户建议)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheScope {
    User,
    Global,
}

/// 命中缓存的计费口径:full(默认,按缓存的用量照常计费)/ free(不扣费,只记日志)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheHitBilling {
    Full,
    Free,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            host: SocketAddr::from(([127, 0, 0, 1], 3000)),
            jwt_secret: "aikun-secret-change-me".to_string(),
            encryption_key: None,
            jwt_expires_in: 86400 * 7, // 7 days
            database_url: "sqlite://aikun.db?mode=rwc".to_string(),
            surreal_url: default_surreal_url(),
            health_check_interval: 30,
            max_retries: 3,
            auto_disable_threshold: 5,
            breaker_failure_threshold: 3,
            breaker_cooldown_secs: 30,
            request_timeout_secs: 120,
            connect_timeout_secs: 15,
            ttfb_timeout_secs: 60,
            idle_timeout_secs: 0,
            log_retention_days: 30,
            cors_allowed_origins: Vec::new(),
            trust_x_forwarded_for: false,
            cache_mode: CacheMode::Off,
            cache_ttl_secs: 300,
            cache_max_entries: 10_000,
            cache_max_entry_bytes: 1024 * 1024,
            cache_scope: CacheScope::User,
            cache_hit_billing: CacheHitBilling::Full,
        }
    }
}

/// 配置项对照表:(CLI 参数名, 环境变量名, 说明)。
/// 环境变量一律带 AIKUN_ 前缀,避免与其他程序的同名变量混淆。
const SETTINGS: &[(&str, &str, &str)] = &[
    ("host", "AIKUN_HOST", "监听地址(IP:端口)"),
    ("jwt-secret", "AIKUN_JWT_SECRET", "JWT 签名密钥"),
    (
        "encryption-key",
        "AIKUN_ENCRYPTION_KEY",
        "渠道 key 静态加密密钥(缺省派生自 JWT secret)",
    ),
    (
        "jwt-expires-in",
        "AIKUN_JWT_EXPIRES_IN",
        "JWT 有效期(秒,1..=31536000)",
    ),
    (
        "database-url",
        "AIKUN_DATABASE_URL",
        "数据库端点(与 surreal-url 等价,surreal-url 优先):surrealkv://<目录> 或远程 ws:// 等;sqlite:// 旧值=源库位置随迁",
    ),
    (
        "surreal-url",
        "AIKUN_SURREAL_URL",
        "SurrealDB 端点(未设默认嵌入式 surrealkv://./data/aikun.surreal;远程如 ws://host:8000,可带 ?ns=&db=&user=&pass=)",
    ),
    (
        "health-check-interval",
        "AIKUN_HEALTH_CHECK_INTERVAL",
        "健康检查间隔(秒,≥5)",
    ),
    (
        "max-retries",
        "AIKUN_MAX_RETRIES",
        "单请求最大渠道尝试次数(≥1)",
    ),
    (
        "auto-disable-threshold",
        "AIKUN_AUTO_DISABLE_THRESHOLD",
        "连续失败多少次后自动禁用渠道(≥1)",
    ),
    (
        "breaker-failure-threshold",
        "AIKUN_BREAKER_FAILURE_THRESHOLD",
        "渠道熔断阈值(≥1,默认 3):连续失败达此次数后冷却期内选路优先跳过",
    ),
    (
        "breaker-cooldown-secs",
        "AIKUN_BREAKER_COOLDOWN_SECS",
        "渠道熔断冷却时长(秒,≥1,默认 30);到期恢复可选(半开)",
    ),
    (
        "request-timeout-secs",
        "AIKUN_REQUEST_TIMEOUT_SECS",
        "单次上游请求超时(秒,≥1)",
    ),
    (
        "connect-timeout-secs",
        "AIKUN_CONNECT_TIMEOUT_SECS",
        "上游 TCP/TLS 连接超时(秒,≥1,默认 15)",
    ),
    (
        "ttfb-timeout-secs",
        "AIKUN_TTFB_TIMEOUT_SECS",
        "流式请求等上游响应头的首字节超时(秒,≥1,默认 60)",
    ),
    (
        "idle-timeout-secs",
        "AIKUN_IDLE_TIMEOUT_SECS",
        "流式 chunk 间空闲超时(秒,0=沿用渠道级 timeout_secs,默认 0)",
    ),
    (
        "log-retention-days",
        "AIKUN_LOG_RETENTION_DAYS",
        "请求日志保留天数(≥1)",
    ),
    (
        "cors-allowed-origins",
        "AIKUN_CORS_ALLOWED_ORIGINS",
        "CORS 允许来源,逗号分隔",
    ),
    (
        "trust-x-forwarded-for",
        "AIKUN_TRUST_X_FORWARDED_FOR",
        "置 1 时信任 X-Forwarded-For(仅限反向代理后)",
    ),
    (
        "cache-mode",
        "AIKUN_CACHE_MODE",
        "响应缓存:off(默认)/ deterministic(仅 temperature=0)/ always",
    ),
    (
        "cache-ttl-secs",
        "AIKUN_CACHE_TTL_SECS",
        "缓存有效期(秒,≥1,默认 300)",
    ),
    (
        "cache-max-entries",
        "AIKUN_CACHE_MAX_ENTRIES",
        "内存缓存最大条目数(≥1,默认 10000)",
    ),
    (
        "cache-max-entry-bytes",
        "AIKUN_CACHE_MAX_ENTRY_BYTES",
        "单条缓存响应大小上限(字节,默认 1048576)",
    ),
    (
        "cache-scope",
        "AIKUN_CACHE_SCOPE",
        "缓存作用域:user(默认,按用户隔离)/ global",
    ),
    (
        "cache-hit-billing",
        "AIKUN_CACHE_HIT_BILLING",
        "命中缓存计费:full(默认,照常计费)/ free",
    ),
];

impl AppConfig {
    /// 加载配置,来源与优先级:AIKUN_* 环境变量 > -- 命令行参数 > 默认值。
    pub fn load() -> Self {
        let args = parse_cli_args();
        // 环境变量优先;未设置时回退到同名 CLI 参数。
        let get = |key: &str, env: &str| -> Option<String> {
            std::env::var(env).ok().or_else(|| args.get(key).cloned())
        };

        let mut config = Self::default();
        if let Some(host) = get("host", "AIKUN_HOST") {
            // 解析失败直接报错退出,不静默回退,避免监听在错误的地址上。
            config.host = host
                .parse()
                .unwrap_or_else(|_| panic!("host {:?} 不是合法的监听地址(应为 IP:端口)", host));
        }
        if let Some(secret) = get("jwt-secret", "AIKUN_JWT_SECRET") {
            config.jwt_secret = secret;
        }
        // 空串视为未设置,回退到 JWT secret 派生(见 crypto.rs)。
        config.encryption_key =
            get("encryption-key", "AIKUN_ENCRYPTION_KEY").filter(|s| !s.is_empty());
        if let Some(val) = get("jwt-expires-in", "AIKUN_JWT_EXPIRES_IN") {
            // 要求 1..=1 年:非正值会签发已过期/永不过期的令牌,
            // 过大值则可能在做加法时溢出。
            config.jwt_expires_in = parse_range(
                &val,
                "jwt-expires-in",
                1,
                86400 * 365,
                config.jwt_expires_in,
            );
        }
        let database_env = get("database-url", "AIKUN_DATABASE_URL");
        // sqlite:// 开头的旧值只承载"源库位置"语义(随迁),不是 Surreal 端点;
        // 其余值(non-sqlite)按 SurrealDB 端点接纳——AIKUN_DATABASE_URL 与
        // AIKUN_SURREAL_URL 等价(2026-11-18 用户裁决:数据库连接一个名字
        // 就够),细名优先。
        if let Some(val) = &database_env {
            config.database_url = val.clone();
            if val.starts_with("sqlite://") {
                tracing::info!(
                    "AIKUN_DATABASE_URL 为 sqlite:// 旧值(源库位置语义):\
                     嵌入式 SurrealKV 目录将随迁同目录;迁移后请改设 \
                     SurrealDB 端点(本变量或 AIKUN_SURREAL_URL 均可)"
                );
            }
        }
        match resolve_surreal_endpoint(
            get("surreal-url", "AIKUN_SURREAL_URL").as_deref(),
            database_env.as_deref(),
        ) {
            EndpointChoice::Surreal(url) => config.surreal_url = url,
            EndpointChoice::FollowSqlite => {
                // 位置语义沿用:未显式指定 Surreal 端点、而旧库指向
                // 非默认位置(如 docker 卷挂载)时,嵌入式 SurrealKV 目录
                // 取旧库同目录,数据位置随迁、不落回工作目录。
                if let Some(dir) = sqlite_dir_of(&config.database_url) {
                    config.surreal_url = format!("surrealkv://{dir}/aikun.surreal");
                }
            }
            EndpointChoice::Default => {}
        }
        // 启动校验:非法端点直接拒绝启动(与 host 解析同口径,不静默回退
        // ——存错引擎/错库比拒启更难排查)。默认值同样过校验。
        if let Err(e) = aikun_store::surreal::Endpoint::parse(&config.surreal_url) {
            panic!("surreal-url {:?} 非法:{}", config.surreal_url, e);
        }
        if let Some(val) = get("health-check-interval", "AIKUN_HEALTH_CHECK_INTERVAL") {
            config.health_check_interval = parse_min(
                &val,
                "health-check-interval",
                5,
                config.health_check_interval,
            );
        }
        if let Some(val) = get("max-retries", "AIKUN_MAX_RETRIES") {
            config.max_retries = parse_min(&val, "max-retries", 1, config.max_retries);
        }
        if let Some(val) = get("auto-disable-threshold", "AIKUN_AUTO_DISABLE_THRESHOLD") {
            config.auto_disable_threshold = parse_min(
                &val,
                "auto-disable-threshold",
                1,
                config.auto_disable_threshold,
            );
        }
        if let Some(val) = get(
            "breaker-failure-threshold",
            "AIKUN_BREAKER_FAILURE_THRESHOLD",
        ) {
            config.breaker_failure_threshold = parse_min(
                &val,
                "breaker-failure-threshold",
                1,
                config.breaker_failure_threshold,
            );
        }
        if let Some(val) = get("breaker-cooldown-secs", "AIKUN_BREAKER_COOLDOWN_SECS") {
            config.breaker_cooldown_secs = parse_min(
                &val,
                "breaker-cooldown-secs",
                1,
                config.breaker_cooldown_secs,
            );
        }
        if let Some(val) = get("request-timeout-secs", "AIKUN_REQUEST_TIMEOUT_SECS") {
            config.request_timeout_secs =
                parse_min(&val, "request-timeout-secs", 1, config.request_timeout_secs);
        }
        // 9.2 三类独立超时:连接 / 首字节 / 流式空闲。与 9.9 同口径——
        // 非法值拒绝启动,不静默回退(idle 的 0 是合法哨兵,表示沿用
        // 渠道级 timeout_secs 的旧语义)。
        if let Some(val) = get("connect-timeout-secs", "AIKUN_CONNECT_TIMEOUT_SECS") {
            config.connect_timeout_secs =
                parse_min(&val, "connect-timeout-secs", 1, config.connect_timeout_secs);
        }
        if let Some(val) = get("ttfb-timeout-secs", "AIKUN_TTFB_TIMEOUT_SECS") {
            config.ttfb_timeout_secs =
                parse_min(&val, "ttfb-timeout-secs", 1, config.ttfb_timeout_secs);
        }
        if let Some(val) = get("idle-timeout-secs", "AIKUN_IDLE_TIMEOUT_SECS") {
            config.idle_timeout_secs =
                parse_min(&val, "idle-timeout-secs", 0, config.idle_timeout_secs);
        }
        if let Some(val) = get("log-retention-days", "AIKUN_LOG_RETENTION_DAYS") {
            config.log_retention_days =
                parse_min(&val, "log-retention-days", 1, config.log_retention_days);
        }
        if let Some(val) = get("cors-allowed-origins", "AIKUN_CORS_ALLOWED_ORIGINS") {
            config.cors_allowed_origins = val
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        config.trust_x_forwarded_for =
            get("trust-x-forwarded-for", "AIKUN_TRUST_X_FORWARDED_FOR").as_deref() == Some("1");
        if let Some(val) = get("cache-mode", "AIKUN_CACHE_MODE") {
            // 9.9:非法枚举拒绝启动(此前 warn 后回退 off;部署配错时进程
            // 看似在跑、实际用默认行为,问题拖到第一个请求才暴露)。
            config.cache_mode = match val.trim().to_ascii_lowercase().as_str() {
                "off" | "" => CacheMode::Off,
                "deterministic" => CacheMode::Deterministic,
                "always" => CacheMode::Always,
                other => {
                    panic!("cache-mode={other:?} 非法(可选 off / deterministic / always),拒绝启动");
                }
            };
        }
        if let Some(val) = get("cache-ttl-secs", "AIKUN_CACHE_TTL_SECS") {
            config.cache_ttl_secs = parse_min(&val, "cache-ttl-secs", 1, config.cache_ttl_secs);
        }
        if let Some(val) = get("cache-max-entries", "AIKUN_CACHE_MAX_ENTRIES") {
            config.cache_max_entries =
                parse_min(&val, "cache-max-entries", 1, config.cache_max_entries);
        }
        if let Some(val) = get("cache-max-entry-bytes", "AIKUN_CACHE_MAX_ENTRY_BYTES") {
            config.cache_max_entry_bytes = parse_min(
                &val,
                "cache-max-entry-bytes",
                1,
                config.cache_max_entry_bytes,
            );
        }
        if let Some(val) = get("cache-scope", "AIKUN_CACHE_SCOPE") {
            config.cache_scope = match val.trim().to_ascii_lowercase().as_str() {
                "global" => CacheScope::Global,
                "user" | "" => CacheScope::User,
                other => {
                    panic!("cache-scope={other:?} 非法(可选 user / global),拒绝启动");
                }
            };
        }
        if let Some(val) = get("cache-hit-billing", "AIKUN_CACHE_HIT_BILLING") {
            config.cache_hit_billing = match val.trim().to_ascii_lowercase().as_str() {
                "free" => CacheHitBilling::Free,
                "full" | "" => CacheHitBilling::Full,
                other => {
                    panic!("cache-hit-billing={other:?} 非法(可选 full / free),拒绝启动");
                }
            };
        }
        config
    }
}

/// 从 SQLite URL 揨取数据文件所在目录(:memory: / 无目录 / 空路径返回
/// None,表示沿用默认工作目录)。用于 SurrealKV 目录的位置语义沿用
/// (config.load 的 database_url 推导)。
/// [`resolve_surreal_endpoint`] 的判定结果。
#[derive(Debug, PartialEq, Eq)]
enum EndpointChoice {
    /// 显式端点(来自 AIKUN_SURREAL_URL,或 AIKUN_DATABASE_URL 的非 sqlite 值)。
    Surreal(String),
    /// 无显式端点,但 database_url 是 sqlite:// 旧值 → 位置随迁。
    FollowSqlite,
    /// 无任何显式配置 → 默认(含旧目录兼容,见 [`default_surreal_url`])。
    Default,
}

/// 两个变量名等价接纳 SurrealDB 端点(2026-11-18):
/// `AIKUN_SURREAL_URL` 优先;`AIKUN_DATABASE_URL` 的 **非 sqlite:// 值**
/// 等价于它(sqlite:// 开头的是旧源库位置,只触发随迁,不当端点解析)。
fn resolve_surreal_endpoint(
    surreal_env: Option<&str>,
    database_env: Option<&str>,
) -> EndpointChoice {
    if let Some(url) = surreal_env {
        return EndpointChoice::Surreal(url.to_string());
    }
    match database_env {
        Some(v) if !v.starts_with("sqlite://") => EndpointChoice::Surreal(v.to_string()),
        Some(_) => EndpointChoice::FollowSqlite,
        None => EndpointChoice::Default,
    }
}

/// 嵌入式默认数据目录(2026-11-18 起):`./data/aikun.surreal`。
/// **旧默认位置兼容**:`./aikun.surreal` 存在且新位置不存在时沿用旧
/// 位置——既有部署(0.6/0.7)升级后不落回空库;要迁移到 data/ 就把
/// 目录移动过来(或显式设 AIKUN_SURREAL_URL)。多级目录由 SurrealKV
/// 引擎自动创建(实测),无需手工 mkdir。
fn default_surreal_url() -> String {
    if std::path::Path::new("./aikun.surreal").exists()
        && !std::path::Path::new("./data/aikun.surreal").exists()
    {
        tracing::info!(
            "检测到旧默认数据目录 ./aikun.surreal,继续沿用(不迁移);\
             如需收进 data/ 请移动目录或显式设置 AIKUN_SURREAL_URL"
        );
        "surrealkv://./aikun.surreal".to_string()
    } else {
        "surrealkv://./data/aikun.surreal".to_string()
    }
}

fn sqlite_dir_of(database_url: &str) -> Option<String> {
    let without_scheme = database_url
        .strip_prefix("sqlite://")
        .unwrap_or(database_url);
    let path = without_scheme.split('?').next()?;
    if path.is_empty() || path.contains(":memory:") {
        return None;
    }
    let dir = std::path::Path::new(path).parent()?;
    if dir.as_os_str().is_empty() {
        return Some(".".to_string());
    }
    Some(dir.to_string_lossy().into_owned())
}

/// 解析 --key value / --key=value 形式的命令行参数。
/// 未知参数或缺值直接报错退出,防止拼写错误被静默忽略。
fn parse_cli_args() -> HashMap<String, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        std::process::exit(0);
    }
    collect_args(argv.into_iter())
}

fn collect_args<I: Iterator<Item = String>>(it: I) -> HashMap<String, String> {
    let known: HashMap<&str, &str> = SETTINGS.iter().map(|(k, _, _)| (*k, *k)).collect();
    let mut map = HashMap::new();
    let mut it = it.peekable();
    while let Some(arg) = it.next() {
        let stripped = arg.strip_prefix("--").unwrap_or_else(|| {
            eprintln!(
                "无法识别的参数 {:?}(应为 --key value 或 --key=value),--help 查看用法",
                arg
            );
            std::process::exit(2);
        });
        let (key, value) = match stripped.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => {
                let v = it.next().unwrap_or_else(|| {
                    eprintln!("参数 --{} 缺少值", stripped);
                    std::process::exit(2);
                });
                (stripped.to_string(), v)
            }
        };
        if !known.contains_key(key.as_str()) {
            eprintln!("未知参数 --{},--help 查看支持的配置项", key);
            std::process::exit(2);
        }
        map.insert(key, value);
    }
    map
}

fn print_usage() {
    eprintln!("Aikun — 轻量 AI 网关\n");
    eprintln!("用法: aikun [--key value | --key=value ...]");
    eprintln!("配置优先级: AIKUN_* 环境变量 > 命令行参数 > 默认值\n");
    eprintln!("{:<26} {:<34} 说明", "参数", "环境变量");
    for (key, env, desc) in SETTINGS {
        eprintln!("--{:<24} {:<34} {}", key, env, desc);
    }
}

/// 解析带下限的数值配置(9.9:非法或低于下限直接 panic 拒绝启动;
/// 2026-11 前是 warn 后回退默认值,属行为变化,已单列 CHANGELOG)。
fn parse_min<T>(raw: &str, name: &str, min: T, default: T) -> T
where
    T: std::str::FromStr + PartialOrd + std::fmt::Display,
{
    match raw.parse::<T>() {
        Ok(v) if v >= min => v,
        _ => {
            panic!("{name}={raw:?} 非法(需 ≥ {min};默认值 {default}),拒绝启动");
        }
    }
}

/// 解析带上下限的数值配置(9.9:越界或非法直接 panic 拒绝启动)。
fn parse_range<T>(raw: &str, name: &str, min: T, max: T, default: T) -> T
where
    T: std::str::FromStr + PartialOrd + std::fmt::Display,
{
    match raw.parse::<T>() {
        Ok(v) if v >= min && v <= max => v,
        _ => {
            panic!("{name}={raw:?} 非法(需在 {min}..={max};默认值 {default}),拒绝启动");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_args_supports_both_forms() {
        let map = collect_args(
            vec![
                "--host".to_string(),
                "0.0.0.0:3001".to_string(),
                "--max-retries=5".to_string(),
            ]
            .into_iter(),
        );
        assert_eq!(map.get("host").map(String::as_str), Some("0.0.0.0:3001"));
        assert_eq!(map.get("max-retries").map(String::as_str), Some("5"));
    }

    /// 9.9:数值配置非法时拒绝(panic)而不是回退默认值。
    #[test]
    fn parse_min_rejects_illegal_values() {
        assert_eq!(parse_min("5", "t", 1u64, 9), 5);
        assert_eq!(
            parse_min("0", "t", 0u64, 9),
            0,
            "0 对 min=0 合法(idle 哨兵)"
        );
        assert!(std::panic::catch_unwind(|| parse_min("abc", "t", 1u64, 9)).is_err());
        assert!(std::panic::catch_unwind(|| parse_min("0", "t", 1u64, 9)).is_err());
        assert!(std::panic::catch_unwind(|| parse_min("-3", "t", 1u64, 9)).is_err());
    }

    #[test]
    fn parse_range_rejects_out_of_range() {
        assert_eq!(parse_range("60", "t", 1u64, 100, 9), 60);
        assert!(std::panic::catch_unwind(|| parse_range("0", "t", 1u64, 100, 9)).is_err());
        assert!(std::panic::catch_unwind(|| parse_range("101", "t", 1u64, 100, 9)).is_err());
    }

    /// 两个变量名等价接纳 SurrealDB 端点;sqlite:// 旧值只触发随迁。
    #[test]
    fn surreal_endpoint_accepts_both_variable_names() {
        use EndpointChoice::*;
        // 细名优先
        assert_eq!(
            resolve_surreal_endpoint(Some("ws://h:8000"), Some("surrealkv://./x")),
            Surreal("ws://h:8000".to_string())
        );
        // 通用名的非 sqlite 值 = 端点(等价)
        assert_eq!(
            resolve_surreal_endpoint(None, Some("surrealkv:///data/aikun.surreal")),
            Surreal("surrealkv:///data/aikun.surreal".to_string())
        );
        assert_eq!(
            resolve_surreal_endpoint(None, Some("wss://db.example.com?ns=p")),
            Surreal("wss://db.example.com?ns=p".to_string())
        );
        // sqlite:// 旧值:随迁,不当端点
        assert_eq!(
            resolve_surreal_endpoint(None, Some("sqlite:///data/aikun.db?mode=rwc")),
            FollowSqlite
        );
        // 全空:默认
        assert_eq!(resolve_surreal_endpoint(None, None), Default);
    }

    /// 默认数据目录:旧位置存在且新位置不存在 → 沿用旧位置(升级兼容);
    /// 其余情况 → 新 data/ 目录。文件系统探测逻辑直接对 Path 斤两,
    /// 此处锁定字符串常量与兼容函数的存在性(探测分支由 e2e/实机覆盖)。
    #[test]
    fn default_surreal_url_prefers_legacy_when_present() {
        // 无旧目录的全新部署(常见 CI/临时目录):新默认
        if !std::path::Path::new("./aikun.surreal").exists() {
            assert_eq!(
                default_surreal_url(),
                "surrealkv://./data/aikun.surreal",
                "全新部署 → data/ 目录"
            );
        }
        // 常量锁定:两处路径不漂移
        assert!(default_surreal_url().starts_with("surrealkv://./"));
    }

    /// SurrealKV 目录的位置语义沿用:旧库显式指向非默认位置时,嵌入式
    /// 目录随迁到同目录;:memory: 无文件位置可继承(回退默认);
    /// 裸文件名 / 相对路径的同目录是工作目录(派生结果与默认值相同)。
    #[test]
    fn surreal_dir_follows_database_url_location() {
        assert_eq!(
            sqlite_dir_of("sqlite://aikun.db?mode=rwc"),
            Some(".".to_string())
        );
        assert_eq!(
            sqlite_dir_of("sqlite:///data/aikun.db?mode=rwc"),
            Some("/data".to_string())
        );
        assert_eq!(
            sqlite_dir_of("/var/lib/aikun/app.db"),
            Some("/var/lib/aikun".to_string())
        );
        assert_eq!(sqlite_dir_of(":memory:"), None);
        // 裸文件名的同目录 = "."(工作目录),派生出与默认一致的端点
        assert_eq!(sqlite_dir_of("aikun.db"), Some(".".to_string()));
    }
}
