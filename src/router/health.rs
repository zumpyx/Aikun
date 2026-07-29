use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::future::join_all;
use rusqlite::params;
use tracing::info;

use crate::config::AppConfig;
use crate::db::DbPool;
use crate::proxy::client::{cached_client, sanitize_proxy_url};

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

    let req = client.get(&url);
    let req = match provider_type {
        "anthropic" => req
            .header("x-api-key", api_key)
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
                info!("Health check OK: {} -> {} ({}ms)", url, status, elapsed as i64);
                ("healthy".to_string(), elapsed)
            } else if status.is_server_error() {
                info!("Health check server error: {} -> {} ({}ms)", url, status, elapsed as i64);
                ("unhealthy".to_string(), elapsed)
            } else {
                info!("Health check degraded: {} -> {} ({}ms)", url, status, elapsed as i64);
                ("degraded".to_string(), elapsed)
            }
        }
        Err(e) => {
            info!(
                "Health check failed: {}{} -> {} ({}ms)",
                url,
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
fn purge_old_request_logs(pool: &Arc<DbPool>, retention_days: u32) {
    let conn = match pool.conn.lock() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to lock DB for log purge: {}", e);
            return;
        }
    };
    match conn.execute(
        // request_logs.created_at 存的是 RFC3339（含 T），截止时间用同一格式比较
        "DELETE FROM request_logs WHERE created_at < strftime('%Y-%m-%dT%H:%M:%S', 'now', '-' || ?1 || ' days')",
        params![retention_days],
    ) {
        Ok(n) if n > 0 => {
            info!("Purged {} request logs older than {} days", n, retention_days);
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
    loop {
        tokio::time::sleep(interval).await;
        info!("Running periodic health check...");

        // Enforce the log retention policy on every round. The purge holds the
        // DB mutex while deleting, so run it on the blocking thread pool.
        {
            let pool = pool.clone();
            let retention_days = config.log_retention_days;
            tokio::task::spawn_blocking(move || purge_old_request_logs(&pool, retention_days));
        }

        let providers = {
            let conn = match pool.conn.lock() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("Failed to lock DB for health check: {}", e);
                    continue;
                }
            };

            let mut stmt = match conn.prepare(
                "SELECT id, base_url, api_key,
                        COALESCE(NULLIF(default_protocol, ''), provider_type),
                        timeout_secs, proxy_url FROM providers WHERE is_active = 1"
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to prepare health check query: {}", e);
                    continue;
                }
            };

            let result: Vec<(String, String, String, String, i32, String)> = match stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i32>(4)?,
                    row.get::<_, String>(5)?,
                ))
            }) {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(e) => {
                    tracing::error!("Failed to query providers: {}", e);
                    continue;
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
            |(id, base_url, api_key, provider_type, timeout, proxy_url)| {
                let clients = clients.clone();
                async move {
                    // Clamp defensively: timeout_secs is validated 1..=600 at the
                    // API layer, but old rows may hold out-of-range values.
                    let probe_timeout = (*timeout).clamp(1, 600).min(60) as u64;
                    let (status, latency) = match tokio::time::timeout(
                        std::time::Duration::from_secs(probe_timeout),
                        check_provider_health(
                            &clients, base_url, api_key, provider_type, probe_timeout, proxy_url,
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
                    continue;
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
}
