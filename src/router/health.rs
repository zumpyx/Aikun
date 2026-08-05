use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::future::join_all;
use rusqlite::params;
use tracing::info;

use crate::config::AppConfig;
use crate::db::DbPool;
use crate::proxy::client::{cached_client, sanitize_log_url, sanitize_proxy_url};

/// Build a proper API URL for health checks. 与聊天路径共用 build_api_url,
/// 保证带 query(如 ?api-version=...)或非 /v1 版本段(如 /v1beta)的
/// base_url 也能拼出正确的 /models 端点。
pub fn build_health_url(base_url: &str) -> String {
    crate::proxy::client::build_api_url(base_url, "/models")
}

/// Check the health of a single provider by making a lightweight request to the models endpoint.
pub async fn check_provider_health(
    clients: &Mutex<HashMap<String, reqwest::Client>>,
    base_url: &str,
    api_key: &str,
    provider_type: &str,
    timeout_secs: u64,
    proxy_url: &str,
) -> (String, f64) {
    let start = Instant::now();
    let client = cached_client(clients, proxy_url, base_url, timeout_secs);

    let url = build_health_url(base_url);
    // 日志脱敏:URL 可能带 userinfo 或 query(如 ?key=...),不落明文。
    let log_url = sanitize_log_url(&url);

    let req = client.get(&url);
    let req = match provider_type {
        // 同 proxy/client.rs 的 apply_auth:同时携带 x-api-key 与 Authorization
        // Bearer,兼容只检查 Authorization 的 Anthropic 上游。
        "anthropic" => req
            .header("x-api-key", api_key)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("anthropic-version", "2023-06-01"),
        _ => req.header("Authorization", format!("Bearer {}", api_key)),
    };

    let result = req.send().await;

    let latency = start.elapsed().as_millis() as f64;

    match result {
        Ok(resp) => {
            let status = resp.status();
            let elapsed = latency;
            if status.is_success() {
                info!("Health check OK: {} -> {} ({}ms)", log_url, status, elapsed as i64);
                ("healthy".to_string(), elapsed)
            } else if status.is_server_error()
                || status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                // 401/403 是凭证失效,与"响应慢"的 degraded 是两回事:与请求
                // 路径(record_failure 对 401/403 立即禁用)口径对齐,ping
                // 直接判不健康,提前拦下而不是等真实用户请求踩雷。
                info!("Health check unhealthy: {} -> {} ({}ms)", log_url, status, elapsed as i64);
                ("unhealthy".to_string(), elapsed)
            } else {
                info!("Health check degraded: {} -> {} ({}ms)", log_url, status, elapsed as i64);
                ("degraded".to_string(), elapsed)
            }
        }
        Err(e) => {
            info!(
                "Health check failed: {}{} -> {} ({}ms)",
                log_url,
                if proxy_url.is_empty() {
                    String::new()
                } else {
                    format!(" via proxy {}", sanitize_proxy_url(proxy_url))
                },
                e,
                latency as i64
            );
            ("unhealthy".to_string(), latency)
        }
    }
}

/// Delete request logs older than the configured retention period.
/// Runs inside `spawn_blocking` since it holds the DB mutex.
/// 删除前先把将删明细按 (用户, 日) 聚合进 usage_daily(同一事务):
/// 明细随保留期清除后,余额对账(Σ充值 − Σ消费)仍有永久依据。
fn purge_old_request_logs(pool: &Arc<DbPool>, retention_days: u32) {
    let mut conn = match pool.conn.lock() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to lock DB for log purge: {}", e);
            return;
        }
    };
    let result = (|| -> Result<usize, rusqlite::Error> {
        let tx = conn.transaction()?;
        // request_logs.created_at 存的是 RFC3339(含 T),截止时间用同一格式比较
        tx.execute(
            "INSERT INTO usage_daily (user_id, date, requests, tokens, cost)
             SELECT user_id, substr(created_at, 1, 10), COUNT(*),
                    COALESCE(SUM(total_tokens), 0), COALESCE(SUM(cost), 0)
             FROM request_logs
             WHERE created_at < strftime('%Y-%m-%dT%H:%M:%S', 'now', '-' || ?1 || ' days')
             GROUP BY user_id, substr(created_at, 1, 10)
             ON CONFLICT(user_id, date) DO UPDATE SET
                requests = requests + excluded.requests,
                tokens = tokens + excluded.tokens,
                cost = cost + excluded.cost",
            params![retention_days],
        )?;
        let n = tx.execute(
            "DELETE FROM request_logs WHERE created_at < strftime('%Y-%m-%dT%H:%M:%S', 'now', '-' || ?1 || ' days')",
            params![retention_days],
        )?;
        tx.commit()?;
        Ok(n)
    })();
    match result {
        Ok(n) if n > 0 => {
            info!("Purged {} request logs older than {} days (aggregated into usage_daily)", n, retention_days);
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!("Failed to purge old request logs: {}", e);
        }
    }
}

/// Run periodic health checks on all active providers.
/// This runs in a background tokio task.
/// Design: DB lock is never held across an await point, ensuring the future is Send.
pub async fn run_health_check_loop(
    pool: Arc<DbPool>,
    config: Arc<AppConfig>,
    clients: Arc<Mutex<HashMap<String, reqwest::Client>>>,
) {
    let interval = std::time::Duration::from_secs(config.health_check_interval);
    // 启动即跑首轮,再按 interval 周期执行——否则启动后最长一个 interval
    // 内零探测,而 model_test 循环的注释假设 health ping 覆盖启动期。
    loop {
        run_health_check_round(&pool, &config, &clients).await;
        tokio::time::sleep(interval).await;
    }
}

/// 一轮健康检查:日志清理 + 并发探测所有启用渠道 + 持久化结果。
/// Design: DB lock is never held across an await point, ensuring the future is Send.
async fn run_health_check_round(
    pool: &Arc<DbPool>,
    config: &AppConfig,
    clients: &Arc<Mutex<HashMap<String, reqwest::Client>>>,
) {
    info!("Running periodic health check...");

        // Enforce the log retention policy on every round. The purge holds the
        // DB mutex while deleting, so run it on the blocking thread pool.
        {
            let pool = pool.clone();
            let retention_days = config.log_retention_days;
            tokio::task::spawn_blocking(move || purge_old_request_logs(&pool, retention_days));
        }

        let providers = {
            let conn = match pool.read().lock() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("Failed to lock DB for health check: {}", e);
                    return;
                }
            };

            let mut stmt = match conn.prepare(
                "SELECT id, openai_base_url, anthropic_base_url, api_key,
                        COALESCE(NULLIF(default_protocol, ''), provider_type),
                        timeout_secs, proxy_url FROM providers WHERE is_active = 1"
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to prepare health check query: {}", e);
                    return;
                }
            };

            let result: Vec<(String, String, String, String, String, i32, String)> = match stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    // api_key 出库解密(enc:v1: 前缀密文,明文兼容)
                    crate::crypto::decrypt_or_plain(&pool.cipher, &row.get::<_, String>(3)?),
                    row.get::<_, String>(4)?,
                    row.get::<_, i32>(5)?,
                    row.get::<_, String>(6)?,
                ))
            }) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(e) => {
                    tracing::error!("Failed to query providers: {}", e);
                    return;
                }
            };
            result
        }; // lock released

        // Probe all providers concurrently (each one only reads its own config,
        // no DB lock is held while awaiting). Each probe gets its own timeout
        // (min(timeout_secs, 60)) so one slow channel can never cancel the
        // whole round — a timed-out probe simply counts as unhealthy and is
        // persisted like any other result.
        let probes = join_all(providers.iter().map(
            |(id, openai_url, anthropic_url, api_key, protocol, timeout, proxy_url)| {
                let clients = clients.clone();
                async move {
                    // Clamp defensively: timeout_secs is validated 1..=600 at the
                    // API layer, but old rows may hold out-of-range values.
                    let probe_timeout = (*timeout).clamp(1, 600).min(60) as u64;
                    // 按默认协议选对应的上游地址,留空的一路回退到另一个。
                    let base_url = if protocol == "anthropic" {
                        if anthropic_url.is_empty() { openai_url } else { anthropic_url }
                    } else if openai_url.is_empty() {
                        anthropic_url
                    } else {
                        openai_url
                    };
                    let (status, latency) = match tokio::time::timeout(
                        std::time::Duration::from_secs(probe_timeout),
                        check_provider_health(
                            &clients, base_url, api_key, protocol, probe_timeout, proxy_url,
                        ),
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(_) => {
                            info!("Health check [{}]: timed out after {}s", id, probe_timeout);
                            ("unhealthy".to_string(), probe_timeout as f64 * 1000.0)
                        }
                    };
                    info!("Health check [{}]: status={}, latency={}ms", id, status, latency as i64);
                    (id.clone(), status, latency)
                }
            },
        ));

        let results: Vec<(String, String, f64)> = probes.await;

        {
            let conn = match pool.conn.lock() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("Failed to lock DB for health update: {}", e);
                    return;
                }
            };

            // Only record the ping outcome: latency_ms is the real-request EMA
            // used for routing and must not be overwritten by ping latency.
            let now = chrono::Utc::now().to_rfc3339();
            for (id, status, _latency) in &results {
                if let Err(e) = conn.execute(
                    "UPDATE providers SET health_status = ?1, last_health_check = ?2 WHERE id = ?3",
                    params![status, now, id],
                ) {
                    tracing::error!("Failed to update provider health: {}", e);
                }
            }
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pool() -> Arc<DbPool> {
        let config = crate::config::AppConfig {
            database_url: ":memory:".into(),
            ..Default::default()
        };
        let pool = Arc::new(DbPool::new(&config).unwrap());
        {
            let conn = pool.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO users (id, username, password_hash) VALUES ('u1', 'u1', 'h')",
                [],
            )
            .unwrap();
            // cost 为整数微元(1 元 = 1e6)
            let insert = |id: &str, date: &str, tokens: i64, cost: i64| {
                conn.execute(
                    "INSERT INTO request_logs (id, user_id, model, total_tokens, cost, created_at)
                     VALUES (?1, 'u1', 'm', ?2, ?3, ?4)",
                    params![id, tokens, cost, date],
                )
                .unwrap();
            };
            insert("l1", "2020-01-15T10:00:00+00:00", 100, 10_000);
            insert("l2", "2020-01-15T11:00:00+00:00", 200, 20_000);
            insert("l3", "2020-01-16T10:00:00+00:00", 50, 5_000);
            // 保留期内的一行不应被清理
            insert("l4", &chrono::Utc::now().to_rfc3339(), 10, 1_000);
        }
        pool
    }

    #[test]
    fn purge_aggregates_before_deleting() {
        let pool = test_pool();
        purge_old_request_logs(&pool, 30);
        {
            let conn = pool.conn.lock().unwrap();
            let remaining: i64 = conn
                .query_row("SELECT COUNT(*) FROM request_logs", [], |r| r.get(0))
                .unwrap();
            assert_eq!(remaining, 1);
            let (requests, tokens, cost): (i64, i64, i64) = conn
                .query_row(
                    "SELECT requests, tokens, cost FROM usage_daily WHERE user_id = 'u1' AND date = '2020-01-15'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!((requests, tokens), (2, 300));
            assert_eq!(cost, 30_000);
            let day2: i64 = conn
                .query_row(
                    "SELECT requests FROM usage_daily WHERE user_id = 'u1' AND date = '2020-01-16'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(day2, 1);
        }
        // 幂等:明细已删,再次 purge 不会重复累加
        purge_old_request_logs(&pool, 30);
        let conn = pool.conn.lock().unwrap();
        let total: i64 = conn
            .query_row("SELECT SUM(requests) FROM usage_daily", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 3);
    }
}
