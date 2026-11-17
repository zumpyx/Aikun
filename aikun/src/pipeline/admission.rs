//! 准入检查:API Key 限额(每分钟请求数 / 每日 token / 并发)、余额预检、
//! 模型白名单。全部在选路之前完成,拒绝时不触达任何上游。

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
/// 并发槽位守卫:阶段 3 起由 aikun-store 的限流组件提供(Drop 释放)。
pub(crate) type ConcurrencyGuard = aikun_store::limiter::CounterGuard;

/// Per-API-key 限流/日额度/并发检查。返回 Err(message) 表示应以 429 拒绝;
/// Ok(guard) 携带并发槽位(未配置并发上限时为 None),调用方须把 guard
/// 持有到请求完全结束。
/// 限额配置与当日已用 token 经 Surreal 计费域一次取回(日用量读
/// api_key_usage_daily 计数器点查,与记账同事务维护——roadmap 阶段 3,
/// 替代 request_logs SUM 子查询);RPM 用内存滑动窗口(key 为 api_keys.id)。
/// 查询失败时放行——key 刚通过认证,行必然存在,瞬时的存储失败不应
/// 打断流量。阶段 S:async 化翻转后直接 await(docs/surreal-migration.md
/// §六),不再经 blocking 线程中转。
pub(crate) async fn enforce_key_limits(
    state: &AppState,
    key_id: &str,
) -> Result<Option<ConcurrencyGuard>, String> {
    let limits = state
        .store
        .billing
        .key_limits_and_usage(key_id, chrono::Utc::now().date_naive())
        .await
        .ok();

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
        // 原子"判定 + 记一次"(单次持锁);组件锁中毒恢复而非放行
        //(语义收紧点见 CHANGELOG:仅 panic 场景有差异)。
        if !state.limiters.api_key_rate.try_hit(
            key_id,
            std::time::Duration::from_secs(60),
            rpm as usize,
        ) {
            return Err(format!(
                "Rate limit exceeded for this API key ({} requests/minute)",
                rpm
            ));
        }
    }

    // 并发槽位最后占用:RPM/额度拒绝时不消耗槽位;槽位超限时 RPM 窗口已
    // 推入一格,与"被 429 的请求也计入限流"的既有口径一致。
    if max_concurrent > 0 {
        return match state
            .limiters
            .api_key_inflight
            .acquire(key_id, max_concurrent as usize)
        {
            Some(guard) => Ok(Some(guard)),
            None => Err(format!(
                "Concurrent request limit exceeded for this API key ({} in-flight)",
                max_concurrent
            )),
        };
    }

    Ok(None)
}

/// 余额预检:balance > 0 才放行。查询失败按放行处理(与 enforce_key_limits
/// 同为 fail-open):瞬时的存储失败不应打断流量,余额口径以扣费落库为准。
pub(crate) async fn has_positive_balance(state: &AppState, user_id: &str) -> bool {
    let result = state
        .store
        .identity
        .find_user(user_id)
        .await
        .ok()
        .flatten()
        .map(|u| u.balance_micro > 0);
    match result {
        Some(positive) => positive,
        None => {
            // fail-open 同样要留信号:存储故障期间余额拦截形同虚设。
            warn!("Balance precheck failed for user {user_id}, allowing request (fail-open)");
            true
        }
    }
}
