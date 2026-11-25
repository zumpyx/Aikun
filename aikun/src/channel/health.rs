use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::future::join_all;
use tracing::info;

use crate::channel::transport::{cached_client, sanitize_log_url, sanitize_proxy_url};
use crate::config::AppConfig;
use crate::store::AppStore;

/// Check the health of a single provider by making a lightweight request to the models endpoint.
#[allow(clippy::too_many_arguments)]
pub async fn check_provider_health(
    clients: &Mutex<HashMap<String, reqwest::Client>>,
    base_url: &str,
    api_key: &str,
    protocol: aikun_protocol::Protocol,
    timeout_secs: u64,
    proxy_url: &str,
    channel_kind: &str,
    connect_timeout_secs: u64,
) -> (String, f64) {
    let start = Instant::now();
    let client = cached_client(
        clients,
        proxy_url,
        base_url,
        timeout_secs,
        connect_timeout_secs,
    );

    // URL 与鉴权头由渠道适配器决定(见 aikun_channels)。
    let adapter = aikun_channels::registry().resolve(channel_kind);
    let url = adapter.models_url(base_url, protocol);
    // 日志脱敏:URL 可能带 userinfo 或 query(如 ?key=...),不落明文。
    let log_url = sanitize_log_url(&url);

    let mut req = client.get(&url);
    for (k, v) in adapter.auth_headers(api_key, protocol) {
        req = req.header(k, v);
    }
    for (k, v) in adapter.extra_headers(protocol) {
        req = req.header(k, v);
    }

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

/// 清理保留期外的请求日志(实现已收口进 Surreal 计费域
/// `purge_expired_logs`:按 (user, 日) 聚合进 usage_daily 再删,对账依据
/// 永久保留;分批/幂等同口径,docs/surreal-migration.md §五)。
async fn purge_old_request_logs(store: &Arc<AppStore>, retention_days: u32) {
    match store.billing.purge_expired_logs(retention_days).await {
        Ok(n) if n > 0 => {
            info!(
                "Purged {n} request logs older than {retention_days} days (aggregated into usage_daily)"
            );
        }
        Ok(_) => {}
        Err(e) => tracing::error!("Failed to purge old request logs: {e}"),
    }
}

/// Run periodic health checks on all active providers.
/// This runs in a background tokio task. 阶段 S:读写都是 async Surreal
/// 查询(启用渠道快照密文出库,探测前解一次)。
pub async fn run_health_check_loop(
    store: Arc<AppStore>,
    config: Arc<AppConfig>,
    clients: Arc<Mutex<HashMap<String, reqwest::Client>>>,
) {
    let interval = std::time::Duration::from_secs(config.health_check_interval);
    // 启动即跑首轮,再按 interval 周期执行——否则启动后最长一个 interval
    // 内零探测,而 model_test 循环的注释假设 health ping 覆盖启动期。
    loop {
        run_health_check_round(&store, &config, &clients).await;
        tokio::time::sleep(interval).await;
    }
}

/// 一轮健康检查:日志清理 + 并发探测所有启用渠道 + 持久化结果。
async fn run_health_check_round(
    store: &Arc<AppStore>,
    config: &AppConfig,
    clients: &Arc<Mutex<HashMap<String, reqwest::Client>>>,
) {
    info!("Running periodic health check...");

    // Enforce the log retention policy on every round.
    purge_old_request_logs(store, config.log_retention_days).await;

    // 启用渠道快照(api_key 为密文,出库解密一次;协议选择口径与旧
    // SQL 的 COALESCE(NULLIF(default_protocol,''), provider_type) 一致)。
    // (id, openai_url, anthropic_url, api_key 明文, protocol, timeout, proxy, kind)
    #[allow(clippy::type_complexity)]
    let providers: Vec<(String, String, String, String, String, i32, String, String)> =
        match store.channels.active_channels().await {
            Ok(rows) => rows
                .into_iter()
                .map(|p| {
                    let protocol = if p.default_protocol.is_empty() {
                        p.provider_type.clone()
                    } else {
                        p.default_protocol.clone()
                    };
                    (
                        p.id,
                        p.openai_base_url,
                        p.anthropic_base_url,
                        crate::crypto::decrypt_or_plain(&store.cipher, &p.api_key),
                        protocol,
                        p.timeout_secs,
                        p.proxy_url,
                        p.channel_kind,
                    )
                })
                .collect(),
            Err(e) => {
                tracing::error!("Failed to query providers for health check: {e}");
                return;
            }
        };

    // Probe all providers concurrently (each one only reads its own config,
    // no DB access is held while awaiting). Each probe gets its own timeout
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
                        config.connect_timeout_secs,
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

    // Only record the ping outcome: latency_ms is the real-request EMA
    // used for routing and must not be overwritten by ping latency
    // (record_health_ping 只写 health_status + last_health_check)。
    // 健康态同时镜像到拓扑快照(该写不进表令牌;进程内直接同步,
    // 选路下一请求立即看到)。
    let snap = crate::channel::registry::shared().cached();
    for (id, status, _latency) in &results {
        if let Err(e) = store.channels.record_health_ping(id, status).await {
            tracing::error!("Failed to update provider health: {e}");
            continue;
        }
        snap.set_health(id, status);
    }
}

#[cfg(test)]
mod tests {
    // 阶段 S 切换:purge 的库内行为(聚合→删除、幂等、分批一致)已由
    // aikun-store 的 surreal::billing_store 单测覆盖(mem:// 直连);
    // 端到端由下一批恢复 e2e。健康探测的纯 HTTP 行为由 e2e_admin_providers
    // 的 mock 上游覆盖。
}
