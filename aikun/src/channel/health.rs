use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::future::join_all;
use rusqlite::params;
use tracing::info;

use crate::channel::transport::{cached_client, sanitize_log_url, sanitize_proxy_url};
use crate::config::AppConfig;
use crate::db::DbPool;

/// Check the health of a single provider by making a lightweight request to the models endpoint.
pub async fn check_provider_health(
    clients: &Mutex<HashMap<String, reqwest::Client>>,
    base_url: &str,
    api_key: &str,
    protocol: aikun_protocol::Protocol,
    timeout_secs: u64,
    proxy_url: &str,
    channel_kind: &str,
) -> (String, f64) {
    let start = Instant::now();
    let client = cached_client(clients, proxy_url, base_url, timeout_secs);

    // URL 与鉴权头由渠道适配器决定(见 crate::channel::adapter)。
    let adapter = crate::channel::registry().resolve(channel_kind);
    let url = adapter.models_url(base_url, protocol);
    // 日志脱敏:URL 可能带 userinfo 或 query(如 ?key=...),不落明文。
    let log_url = sanitize_log_url(&url);

    let req = adapter.apply_auth(client.get(&url), api_key, protocol);

    let result = req.send().await;

    let latency = start.elapsed().as_millis() as f64;

    match result {
        Ok(resp) => {
            let status = resp.status();
            let elapsed = latency;
            if status.is_success() {
                info!(
                    "Health check OK: {} -> {} ({}ms)",
                    log_url, status, elapsed as i64
                );
                ("healthy".to_string(), elapsed)
            } else if status.is_server_error()
                || status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                // 401/403 是凭证失效,与"响应慢"的 degraded 是两回事:与请求
                // 路径(record_failure 对 401/403 立即禁用)口径对齐,ping
                // 直接判不健康,提前拦下而不是等真实用户请求踩雷。
                info!(
                    "Health check unhealthy: {} -> {} ({}ms)",
                    log_url, status, elapsed as i64
                );
                ("unhealthy".to_string(), elapsed)
            } else {
                info!(
                    "Health check degraded: {} -> {} ({}ms)",
                    log_url, status, elapsed as i64
                );
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

/// 每批清理的明细行数:批与批之间释放写锁,避免一次性 DELETE 数十万行
/// 长时间独占唯一的写连接、阻塞计费写入(roadmap-v0.5 阶段 0 修复 ③)。
const PURGE_BATCH: usize = 5000;

/// Delete request logs older than the configured retention period.
/// Runs inside `spawn_blocking` since it holds the DB mutex.
/// 删除前先把将删明细按 (用户, 日) 聚合进 usage_daily(每批同一事务):
/// 明细随保留期清除后,余额对账(Σ充值 − Σ消费)仍有永久依据。
pub(crate) fn purge_old_request_logs(pool: &Arc<DbPool>, retention_days: u32) {
    purge_old_request_logs_batched(pool, retention_days, PURGE_BATCH)
}

fn purge_old_request_logs_batched(pool: &Arc<DbPool>, retention_days: u32, batch: usize) {
    // 截止时间只算一次:分批期间 'now' 前移不应扩大清理范围。
    // request_logs.created_at 存的是 RFC3339(含 T),截止时间用同一格式比较。
    let cutoff: String = match pool.conn.lock() {
        Ok(conn) => match conn.query_row(
            "SELECT strftime('%Y-%m-%dT%H:%M:%S', 'now', '-' || ?1 || ' days')",
            params![retention_days],
            |r| r.get(0),
        ) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to compute log purge cutoff: {}", e);
                return;
            }
        },
        Err(e) => {
            tracing::error!("Failed to lock DB for log purge: {}", e);
            return;
        }
    };

    let mut total = 0usize;
    loop {
        // 每批单独加锁 + 单独事务;锁在本次循环体结束时释放。
        let result = (|| -> Result<usize, rusqlite::Error> {
            let mut conn = pool
                .conn
                .lock()
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            let tx = conn.transaction()?;
            let batch_rows =
                "SELECT rowid FROM request_logs WHERE created_at < ?1 ORDER BY rowid LIMIT ?2";
            tx.execute(
                &format!(
                    "INSERT INTO usage_daily (user_id, date, requests, tokens, cost)
                     SELECT user_id, substr(created_at, 1, 10), COUNT(*),
                            COALESCE(SUM(total_tokens), 0), COALESCE(SUM(cost), 0)
                     FROM request_logs
                     WHERE rowid IN ({batch_rows})
                     GROUP BY user_id, substr(created_at, 1, 10)
                     ON CONFLICT(user_id, date) DO UPDATE SET
                        requests = requests + excluded.requests,
                        tokens = tokens + excluded.tokens,
                        cost = cost + excluded.cost"
                ),
                params![cutoff, batch as i64],
            )?;
            let n = tx.execute(
                &format!("DELETE FROM request_logs WHERE rowid IN ({batch_rows})"),
                params![cutoff, batch as i64],
            )?;
            tx.commit()?;
            Ok(n)
        })();
        match result {
            Ok(n) => {
                total += n;
                if n < batch {
                    break;
                }
                // 让出写锁给计费 / 日志写入。
                std::thread::yield_now();
            }
            Err(e) => {
                tracing::error!("Failed to purge old request logs: {}", e);
                break;
            }
        }
    }
    if total > 0 {
        info!(
            "Purged {} request logs older than {} days (aggregated into usage_daily)",
            total, retention_days
        );
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
                        timeout_secs, proxy_url, channel_kind FROM providers WHERE is_active = 1",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to prepare health check query: {}", e);
                return;
            }
        };

        #[allow(clippy::type_complexity)]
        let result: Vec<(String, String, String, String, String, i32, String, String)> = match stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    // api_key 出库解密(enc:v1: 前缀密文,明文兼容)
                    crate::crypto::decrypt_or_plain(&pool.cipher, &row.get::<_, String>(3)?),
                    row.get::<_, String>(4)?,
                    row.get::<_, i32>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
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
        |(id, openai_url, anthropic_url, api_key, protocol, timeout, proxy_url, channel_kind)| {
            let clients = clients.clone();
            async move {
                // Clamp defensively: timeout_secs is validated 1..=600 at the
                // API layer, but old rows may hold out-of-range values.
                let probe_timeout = (*timeout).clamp(1, 600).min(60) as u64;
                // 按默认协议选对应的上游地址,留空的一路回退到另一个。
                // 默认协议列为空时退回 provider_type(azure/custom 等讲 OpenAI)。
                let protocol = aikun_protocol::Protocol::parse(protocol)
                    .unwrap_or_else(|| aikun_protocol::Protocol::for_provider_type(protocol));
                let base_url = crate::models::pick_base_url(protocol, openai_url, anthropic_url);
                let (status, latency) = match tokio::time::timeout(
                    std::time::Duration::from_secs(probe_timeout),
                    check_provider_health(
                        &clients,
                        base_url,
                        api_key,
                        protocol,
                        probe_timeout,
                        proxy_url,
                        channel_kind,
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
                info!(
                    "Health check [{}]: status={}, latency={}ms",
                    id, status, latency as i64
                );
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

    #[test]
    fn purge_in_small_batches_matches_single_pass() {
        // 回归(bug ③):分批清理(每批 1 行,跨日期跨批)的聚合结果与一次性
        // 清理一致,保留期内的行不受影响。
        let pool = test_pool();
        purge_old_request_logs_batched(&pool, 30, 1);
        let conn = pool.conn.lock().unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM request_logs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
        let rows: Vec<(String, i64, i64, i64)> = conn
            .prepare("SELECT date, requests, tokens, cost FROM usage_daily ORDER BY date")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("2020-01-15".to_string(), 2, 300, 30_000),
                ("2020-01-16".to_string(), 1, 50, 5_000),
            ]
        );
    }
}
