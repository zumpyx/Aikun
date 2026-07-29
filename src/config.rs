use std::collections::HashMap;
use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub host: SocketAddr,
    pub jwt_secret: String,
    pub jwt_expires_in: i64,          // seconds
    pub database_url: String,
    pub health_check_interval: u64,   // seconds
    pub max_retries: u32,
    pub auto_disable_threshold: u32,  // consecutive failures before auto-disable
    pub request_timeout_secs: u64,
    pub log_retention_days: u32,
    /// CORS 允许的来源白名单;为空表示不加 CORS 层(仅同源)。
    pub cors_allowed_origins: Vec<String>,
    /// 是否信任 X-Forwarded-For 头(仅在反向代理后开启)。
    pub trust_x_forwarded_for: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            host: SocketAddr::from(([127, 0, 0, 1], 3000)),
            jwt_secret: "aikun-secret-change-me".to_string(),
            jwt_expires_in: 86400 * 7, // 7 days
            database_url: "sqlite://aikun.db?mode=rwc".to_string(),
            health_check_interval: 30,
            max_retries: 3,
            auto_disable_threshold: 5,
            request_timeout_secs: 120,
            log_retention_days: 30,
            cors_allowed_origins: Vec::new(),
            trust_x_forwarded_for: false,
        }
    }
}

/// 配置项对照表:(CLI 参数名, 环境变量名, 说明)。
/// 环境变量一律带 AIKUN_ 前缀,避免与其他程序的同名变量混淆。
const SETTINGS: &[(&str, &str, &str)] = &[
    ("host", "AIKUN_HOST", "监听地址(IP:端口)"),
    ("jwt-secret", "AIKUN_JWT_SECRET", "JWT 签名密钥"),
    ("jwt-expires-in", "AIKUN_JWT_EXPIRES_IN", "JWT 有效期(秒,1..=31536000)"),
    ("database-url", "AIKUN_DATABASE_URL", "数据库 URL(如 sqlite://aikun.db?mode=rwc)"),
    ("health-check-interval", "AIKUN_HEALTH_CHECK_INTERVAL", "健康检查间隔(秒,≥5)"),
    ("max-retries", "AIKUN_MAX_RETRIES", "单请求最大渠道尝试次数(≥1)"),
    ("auto-disable-threshold", "AIKUN_AUTO_DISABLE_THRESHOLD", "连续失败多少次后自动禁用渠道(≥1)"),
    ("request-timeout-secs", "AIKUN_REQUEST_TIMEOUT_SECS", "单次上游请求超时(秒,≥1)"),
    ("log-retention-days", "AIKUN_LOG_RETENTION_DAYS", "请求日志保留天数(≥1)"),
    ("cors-allowed-origins", "AIKUN_CORS_ALLOWED_ORIGINS", "CORS 允许来源,逗号分隔"),
    ("trust-x-forwarded-for", "AIKUN_TRUST_X_FORWARDED_FOR", "置 1 时信任 X-Forwarded-For(仅限反向代理后)"),
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
        if let Some(val) = get("jwt-expires-in", "AIKUN_JWT_EXPIRES_IN") {
            // 要求 1..=1 年:非正值会签发已过期/永不过期的令牌,
            // 过大值则可能在做加法时溢出。
            config.jwt_expires_in = parse_range(&val, "jwt-expires-in", 1, 86400 * 365, config.jwt_expires_in);
        }
        if let Some(val) = get("database-url", "AIKUN_DATABASE_URL") {
            config.database_url = val;
        }
        if let Some(val) = get("health-check-interval", "AIKUN_HEALTH_CHECK_INTERVAL") {
            config.health_check_interval = parse_min(&val, "health-check-interval", 5, config.health_check_interval);
        }
        if let Some(val) = get("max-retries", "AIKUN_MAX_RETRIES") {
            config.max_retries = parse_min(&val, "max-retries", 1, config.max_retries);
        }
        if let Some(val) = get("auto-disable-threshold", "AIKUN_AUTO_DISABLE_THRESHOLD") {
            config.auto_disable_threshold = parse_min(&val, "auto-disable-threshold", 1, config.auto_disable_threshold);
        }
        if let Some(val) = get("request-timeout-secs", "AIKUN_REQUEST_TIMEOUT_SECS") {
            config.request_timeout_secs = parse_min(&val, "request-timeout-secs", 1, config.request_timeout_secs);
        }
        if let Some(val) = get("log-retention-days", "AIKUN_LOG_RETENTION_DAYS") {
            config.log_retention_days = parse_min(&val, "log-retention-days", 1, config.log_retention_days);
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
        config
    }
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
            eprintln!("无法识别的参数 {:?}(应为 --key value 或 --key=value),--help 查看用法", arg);
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

/// 解析带下限的数值配置,非法或低于下限时回退默认值并 warn。
fn parse_min<T>(raw: &str, name: &str, min: T, default: T) -> T
where
    T: std::str::FromStr + PartialOrd + std::fmt::Display,
{
    match raw.parse::<T>() {
        Ok(v) if v >= min => v,
        _ => {
            tracing::warn!("{}={:?} 非法(需 ≥ {}),回退默认值 {}", name, raw, min, default);
            default
        }
    }
}

/// 解析带上下限的数值配置,越界或非法时回退默认值并 warn。
fn parse_range<T>(raw: &str, name: &str, min: T, max: T, default: T) -> T
where
    T: std::str::FromStr + PartialOrd + std::fmt::Display,
{
    match raw.parse::<T>() {
        Ok(v) if v >= min && v <= max => v,
        _ => {
            tracing::warn!("{}={:?} 非法(需在 {}..={}),回退默认值 {}", name, raw, min, max, default);
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_args_supports_both_forms() {
        let map = collect_args(
            vec!["--host".to_string(), "0.0.0.0:3001".to_string(), "--max-retries=5".to_string()]
                .into_iter(),
        );
        assert_eq!(map.get("host").map(String::as_str), Some("0.0.0.0:3001"));
        assert_eq!(map.get("max-retries").map(String::as_str), Some("5"));
    }
}
