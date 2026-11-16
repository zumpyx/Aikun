//! 准入检查:API Key 限额(每分钟请求数 / 每日 token / 并发)、余额预检、
//! 模型白名单。全部在选路之前完成,拒绝时不触达任何上游。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tracing::warn;

use aikun_store::KeyUsageSnapshot;

use crate::AppState;

/// 模型白名单判定:精确匹配,或以 `*` 结尾的前缀匹配(如 "claude-*")。
/// `None` 表示不限制。chat 请求与 /v1/models 过滤共用同一口径。
pub(crate) fn model_allowed(allowed: &Option<Vec<String>>, model: &str) -> bool {
    match allowed {
        None => true,
        Some(list) => list.iter().any(|m| match m.strip_suffix('*') {
            Some(prefix) => model.starts_with(prefix),
            None => m == model,
        }),
    }
}

/// API key 并发槽位:Drop 时归还在途计数。非流式请求在响应体读完、函数
/// 返回时释放;流式请求被 move 进 stream 生成器,持有到流结束/客户端断连/
/// 进程关停(生成器 Drop 全覆盖)。
pub(crate) struct ConcurrencyGuard {
    key_id: String,
    inflight: Arc<Mutex<HashMap<String, usize>>>,
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        let mut map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = map.get_mut(&self.key_id) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.key_id);
            }
        }
    }
}

/// Per-API-key 限流/日额度/并发检查。返回 Err(message) 表示应以 429 拒绝;
/// Ok(guard) 携带并发槽位(未配置并发上限时为 None),调用方须把 guard
/// 持有到请求完全结束。
/// 限额配置与当日已用 token 在 blocking 线程一次取回(仓储层
/// `KeyUsage` 能力:日用量读 api_key_usage_daily 计数器点查,与记账同
/// 事务维护——roadmap 阶段 3,替代 request_logs SUM 子查询);RPM 用内存
/// 滑动窗口(key 为 api_keys.id)。查询失败时放行——key 刚通过认证,
/// 行必然存在,瞬时的锁/IO 失败不应打断流量。
pub(crate) async fn enforce_key_limits(
    state: &AppState,
    key_id: &str,
) -> Result<Option<ConcurrencyGuard>, String> {
    let pool = state.pool.clone();
    let key = key_id.to_string();
    let limits = tokio::task::spawn_blocking(move || {
        use aikun_store::KeyUsage as _;
        pool.key_limits_and_usage(&key, chrono::Utc::now().date_naive())
            .ok()
    })
    .await
    .ok()
    .flatten();

    let Some(snapshot) = limits else {
        // fail-open 必须留信号:DB 故障(锁竞争/IO)会持续走这个分支,
        // 没有日志的话 RPM/日额度/并发三项限额静默失效而无人察觉。
        warn!(
            "API key limit query failed for key {}, allowing request without enforcement",
            key_id
        );
        return Ok(None);
    };
    let KeyUsageSnapshot {
        rate_limit_rpm: rpm,
        quota_daily_tokens: daily_quota,
        max_concurrent,
        tokens_spent: used_today,
    } = snapshot;

    if daily_quota > 0 && used_today >= daily_quota {
        return Err(format!(
            "Daily token quota exceeded for this API key ({}/{} tokens used today)",
            used_today, daily_quota
        ));
    }

    if rpm > 0 {
        // 锁中毒时跳过 RPM 检查放行,与整体 fail-open 口径一致。
        if let Ok(mut windows) = state.api_key_rate.lock() {
            let now = std::time::Instant::now();
            let window = windows.entry(key_id.to_string()).or_default();
            window.retain(|t| now.duration_since(*t).as_secs() < 60);
            if window.len() >= rpm as usize {
                // 空窗口的 key 顺手清掉,避免 map 随 key 数量无限增长(这里
                // 窗口非空不处理,空窗口在下一次请求 retain 后同样被清)。
                return Err(format!(
                    "Rate limit exceeded for this API key ({} requests/minute)",
                    rpm
                ));
            }
            window.push(now);
            windows.retain(|_, w| !w.is_empty());
        }
    }

    // 并发槽位最后占用:RPM/额度拒绝时不消耗槽位;槽位超限时 RPM 窗口已
    // 推入一格,与"被 429 的请求也计入限流"的既有口径一致。
    if max_concurrent > 0 {
        let mut map = state
            .api_key_inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let n = map.entry(key_id.to_string()).or_insert(0);
        if *n >= max_concurrent as usize {
            return Err(format!(
                "Concurrent request limit exceeded for this API key ({} in-flight)",
                max_concurrent
            ));
        }
        *n += 1;
        return Ok(Some(ConcurrencyGuard {
            key_id: key_id.to_string(),
            inflight: state.api_key_inflight.clone(),
        }));
    }

    Ok(None)
}

/// 余额预检:balance > 0 才放行。查询失败按放行处理(与 enforce_key_limits
/// 同为 fail-open):瞬时的锁/IO 失败不应打断流量,余额口径以扣费落库为准。
pub(crate) async fn has_positive_balance(state: &AppState, user_id: &str) -> bool {
    let pool = state.pool.clone();
    let uid = user_id.to_string();
    let uid_log = uid.clone();
    let result = tokio::task::spawn_blocking(move || {
        pool.read().lock().ok().and_then(|conn| {
            conn.query_row(
                "SELECT balance > 0 FROM users WHERE id = ?1",
                rusqlite::params![uid],
                |row| row.get::<_, bool>(0),
            )
            .ok()
        })
    })
    .await
    .ok()
    .flatten();
    match result {
        Some(positive) => positive,
        None => {
            // fail-open 同样要留信号:DB 故障期间余额拦截形同虚设。
            warn!(
                "Balance precheck failed for user {}, allowing request (fail-open)",
                uid_log
            );
            true
        }
    }
}
