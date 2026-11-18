//! 记账:请求日志 + 计费扣款。阶段 S:日志插入、日用量计数器累加、
//! 余额扣减在**同一个 SurrealDB 事务**里完成(`BillingStore::
//! record_request` 的单次 `BEGIN…COMMIT` 查询;红线语义不变,
//! docs/surreal-migration.md §六.2)。价格折算仍在事务外(进程内价格
//! 缓存),`cost` 为 0 时不写余额,api_key 为 None 不计计数器。
//!
//! 阶段 2:全部落账路径收敛到 [`RequestLogParams`](request_id 贯穿,9.6),
//! 取代此前 15 参长参数列表;`StreamAbortGuard` 的 Drop 路径经
//! [`spawn_request_log_for_guard`] 复用同一入参形状。

use std::sync::Arc;

use tracing::warn;

use aikun_store::surreal::billing_store::{BillingStore, RequestLedgerEntry};

use crate::AppState;

/// 一次落账的全部入参(request_id + 审计维度 + 用量 + 结果)。
#[derive(Debug, Clone)]
pub(crate) struct RequestLogParams {
    /// 请求追踪号(9.6):同一请求多渠道重试的各次落账共享同一 id。
    pub request_id: String,
    pub user_id: String,
    pub api_key_id: Option<String>,
    pub provider_id: Option<String>,
    pub model: String,
    /// 日志口径:客户端协议名(openai/anthropic/responses)或透传端点路径
    /// (embeddings 等)。
    pub request_type: String,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
    pub cached_tokens: i32,
    pub latency_ms: i32,
    pub status_code: i32,
    pub success: bool,
    pub error_message: Option<String>,
}

impl RequestLogParams {
    /// 失败路径的便捷构造:零用量、不成功。
    #[allow(clippy::too_many_arguments)]
    pub fn failure(
        request_id: &str,
        user_id: String,
        api_key_id: Option<String>,
        provider_id: Option<String>,
        model: &str,
        request_type: &str,
        latency_ms: i32,
        status_code: i32,
        error_message: Option<String>,
    ) -> Self {
        Self {
            request_id: request_id.to_string(),
            user_id,
            api_key_id,
            provider_id,
            model: model.to_string(),
            request_type: request_type.to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            cached_tokens: 0,
            latency_ms,
            status_code,
            success: false,
            error_message,
        }
    }
}

/// Fire-and-forget 版本:失败路径与流中途断连(StreamAbortGuard)等无法
/// await 的场景,投递为独立异步任务;成功计费路径必须改用
/// `request_log_sync` 等待落库,进程退出时 fire-and-forget 会丢账。
pub(crate) fn spawn_request_log(state: &AppState, p: RequestLogParams) {
    let billing = state.store.billing.clone();
    let store_id = state.store.id;
    tokio::spawn(async move {
        insert_request_log(billing.as_ref(), store_id, &p, true).await;
    });
}

/// Fire-and-forget 版本(存储句柄入参;StreamAbortGuard 的 Drop 上下文
/// 没有完整 AppState,只有 Arc<AppStore>)。
pub(crate) fn spawn_request_log_for_guard(
    store: &Arc<crate::store::AppStore>,
    p: RequestLogParams,
) {
    let billing = store.billing.clone();
    let store_id = store.id;
    tokio::spawn(async move {
        insert_request_log(billing.as_ref(), store_id, &p, true).await;
    });
}

/// 计费级同步版本:调用方 await,函数返回时日志与扣费已落库。
/// 用于非流式成功路径——响应返回给用户前费用必须已记账。
pub(crate) async fn request_log_sync(state: &AppState, p: RequestLogParams) {
    insert_request_log(state.store.billing.as_ref(), state.store.id, &p, true).await;
}

/// 命中响应缓存时的记账:provider_id 为 NULL(未触达任何上游)、
/// status 200。`billable = false`(AIKUN_CACHE_HIT_BILLING=free)时只记日志
/// 与用量、不扣费。调用方 await,返回时已落库。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_cache_hit(
    state: &AppState,
    request_id: &str,
    user_id: String,
    api_key_id: Option<String>,
    model: &str,
    request_type: &str,
    usage: (i32, i32, i32, i32),
    billable: bool,
) {
    let (p, c, t, k) = usage;
    let params = RequestLogParams {
        request_id: request_id.to_string(),
        user_id,
        api_key_id,
        provider_id: None,
        model: model.to_string(),
        request_type: request_type.to_string(),
        prompt_tokens: p,
        completion_tokens: c,
        total_tokens: t,
        cached_tokens: k,
        latency_ms: 0,
        status_code: 200,
        success: true,
        error_message: None,
    };
    insert_request_log(
        state.store.billing.as_ref(),
        state.store.id,
        &params,
        billable,
    )
    .await;
}

/// 记账主体:价格折算(进程内缓存,事务外)→ 单事务落库(日志、计数器
/// 与余额扣减三者同事务,红线 1;实现见 aikun-store 的 SurrealBilling)。
///
/// 失败必须可见:此前静默吞错曾导致迁移中间态下日志全丢。
async fn insert_request_log(
    billing: &dyn BillingStore,
    store_id: u64,
    p: &RequestLogParams,
    billable: bool,
) {
    // 计费:有用量即按价格表折算费用(整数微元)。上游 token 已真实消耗,
    // 客户端中途断连(499)或流中途错误(502)时已捕获的 usage 照扣;
    // 429/400/上游错误路径 total_tokens 本就为 0,不受影响。
    // 有用量但无价格配置:静默免费必须可见。
    let cost = if billable && p.total_tokens > 0 {
        match crate::billing::find_price_cached(billing, store_id, &p.model).await {
            Some(prices) => crate::billing::compute_cost(
                p.prompt_tokens,
                p.cached_tokens,
                p.completion_tokens,
                prices,
            ),
            None => {
                warn!(
                    "No price configured for model '{}' — {} tokens billed at 0",
                    p.model, p.total_tokens
                );
                0
            }
        }
    } else {
        0
    };

    let entry = RequestLedgerEntry {
        request_id: Some(p.request_id.clone()),
        user_id: p.user_id.clone(),
        api_key_id: p.api_key_id.clone(),
        provider_id: p.provider_id.clone(),
        model: p.model.clone(),
        request_type: p.request_type.clone(),
        prompt_tokens: p.prompt_tokens,
        completion_tokens: p.completion_tokens,
        total_tokens: p.total_tokens,
        cached_tokens: p.cached_tokens,
        latency_ms: p.latency_ms,
        status_code: p.status_code,
        success: p.success,
        error_message: p.error_message.clone(),
        cost_micro: cost,
    };
    if let Err(e) = billing.record_request(entry).await {
        warn!("Failed to record request log / billing: {e}");
    }
}
