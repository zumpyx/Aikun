//! 记账:请求日志 + 计费扣款。阶段 S:日志插入、日用量计数器累加、
//! 余额扣减在**同一个 SurrealDB 事务**里完成(`BillingStore::
//! record_request` 的单次 `BEGIN…COMMIT` 查询;红线语义不变,
//! docs/surreal-migration.md §六.2)。价格折算仍在事务外(进程内价格
//! 缓存),`cost` 为 0 时不写余额,api_key 为 None 不计计数器。

use std::sync::Arc;

use tracing::warn;

use aikun_store::surreal::billing_store::{BillingStore, RequestLedgerEntry};

use crate::AppState;

/// Fire-and-forget 版本:失败路径与流中途断连(StreamAbortGuard)等无法
/// await 的场景,投递为独立异步任务;成功计费路径必须改用
/// `request_log_sync` 等待落库,进程退出时 fire-and-forget 会丢账。
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_request_log(
    state: &AppState,
    user_id: String,
    api_key_id: Option<String>,
    provider_id: Option<String>,
    model: String,
    request_type: &str,
    prompt_tokens: i32,
    completion_tokens: i32,
    total_tokens: i32,
    cached_tokens: i32,
    latency_ms: i32,
    status_code: i32,
    success: bool,
    error_message: Option<String>,
) {
    let billing = state.store.billing.clone();
    let store_id = state.store.id;
    let request_type = request_type.to_string();
    tokio::spawn(async move {
        insert_request_log(
            billing.as_ref(),
            store_id,
            &user_id,
            api_key_id.as_deref(),
            provider_id.as_deref(),
            &model,
            &request_type,
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cached_tokens,
            latency_ms,
            status_code,
            success,
            error_message,
            true,
        )
        .await;
    });
}

/// Fire-and-forget 版本(存储句柄入参;StreamAbortGuard 的 Drop 上下文
/// 没有完整 AppState,只有 Arc<AppStore>)。
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_request_log_for_guard(
    store: &Arc<crate::store::AppStore>,
    user_id: String,
    api_key_id: Option<String>,
    provider_id: Option<String>,
    model: String,
    request_type: &str,
    prompt_tokens: i32,
    completion_tokens: i32,
    total_tokens: i32,
    cached_tokens: i32,
    latency_ms: i32,
    status_code: i32,
    success: bool,
    error_message: Option<String>,
) {
    let billing = store.billing.clone();
    let store_id = store.id;
    let request_type = request_type.to_string();
    tokio::spawn(async move {
        insert_request_log(
            billing.as_ref(),
            store_id,
            &user_id,
            api_key_id.as_deref(),
            provider_id.as_deref(),
            &model,
            &request_type,
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cached_tokens,
            latency_ms,
            status_code,
            success,
            error_message,
            true,
        )
        .await;
    });
}

/// 计费级同步版本:调用方 await,函数返回时日志与扣费已落库。
/// 用于非流式成功路径——响应返回给用户前费用必须已记账。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn request_log_sync(
    state: &AppState,
    user_id: String,
    api_key_id: Option<String>,
    provider_id: Option<String>,
    model: String,
    request_type: &str,
    prompt_tokens: i32,
    completion_tokens: i32,
    total_tokens: i32,
    cached_tokens: i32,
    latency_ms: i32,
    status_code: i32,
    success: bool,
    error_message: Option<String>,
) {
    insert_request_log(
        state.store.billing.as_ref(),
        state.store.id,
        &user_id,
        api_key_id.as_deref(),
        provider_id.as_deref(),
        &model,
        request_type,
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cached_tokens,
        latency_ms,
        status_code,
        success,
        error_message,
        true,
    )
    .await;
}

/// 命中响应缓存时的记账:provider_id 为 NULL(未触达任何上游)、
/// status 200。`billable = false`(AIKUN_CACHE_HIT_BILLING=free)时只记日志
/// 与用量、不扣费。调用方 await,返回时已落库。
pub(crate) async fn record_cache_hit(
    state: &AppState,
    user_id: String,
    api_key_id: Option<String>,
    model: String,
    request_type: &str,
    usage: (i32, i32, i32, i32),
    billable: bool,
) {
    let (p, c, t, k) = usage;
    insert_request_log(
        state.store.billing.as_ref(),
        state.store.id,
        &user_id,
        api_key_id.as_deref(),
        None,
        &model,
        request_type,
        p,
        c,
        t,
        k,
        0,
        200,
        true,
        None,
        billable,
    )
    .await;
}

/// 记账主体:价格折算(进程内缓存,事务外)→ 单事务落库(日志、计数器
/// 与余额扣减三者同事务,红线 1;实现见 aikun-store 的 SurrealBilling)。
///
/// 失败必须可见:此前静默吞错曾导致迁移中间态下日志全丢。
#[allow(clippy::too_many_arguments)]
async fn insert_request_log(
    billing: &dyn BillingStore,
    store_id: u64,
    user_id: &str,
    api_key_id: Option<&str>,
    provider_id: Option<&str>,
    model: &str,
    request_type: &str,
    prompt_tokens: i32,
    completion_tokens: i32,
    total_tokens: i32,
    cached_tokens: i32,
    latency_ms: i32,
    status_code: i32,
    success: bool,
    error_message: Option<String>,
    billable: bool,
) {
    // 计费:有用量即按价格表折算费用(整数微元)。上游 token 已真实消耗,
    // 客户端中途断连(499)或流中途错误(502)时已捕获的 usage 照扣;
    // 429/400/上游错误路径 total_tokens 本就为 0,不受影响。
    // 有用量但无价格配置:静默免费必须可见。
    let cost = if billable && total_tokens > 0 {
        match crate::billing::find_price_cached(billing, store_id, model).await {
            Some(prices) => crate::billing::compute_cost(
                prompt_tokens,
                cached_tokens,
                completion_tokens,
                prices,
            ),
            None => {
                warn!(
                    "No price configured for model '{}' — {} tokens billed at 0",
                    model, total_tokens
                );
                0
            }
        }
    } else {
        0
    };

    let entry = RequestLedgerEntry {
        user_id: user_id.to_string(),
        api_key_id: api_key_id.map(str::to_string),
        provider_id: provider_id.map(str::to_string),
        model: model.to_string(),
        request_type: request_type.to_string(),
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cached_tokens,
        latency_ms,
        status_code,
        success,
        error_message,
        cost_micro: cost,
    };
    if let Err(e) = billing.record_request(entry).await {
        warn!("Failed to record request log / billing: {e}");
    }
}
