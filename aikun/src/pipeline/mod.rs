//! 请求管线(roadmap-v0.5 目标架构的 `pipeline/`):准入 → 选路(渠道亲和 / 加权评分)→ 协议转换 → 渠道适配器发送
//! → 故障转移 → 响应/流式转换回客户端协议 → 记账。
//!
//! 三个标准入站端点(/v1/chat/completions、/v1/messages、/v1/responses)共用
//! [`proxy_completion`];通用端点(embeddings/moderations/images/rerank)作为
//! `NoConversion + OpenAiOnly + NoStream` 模式经 [`passthrough_completion`]
//! 并入同一管线(阶段 2)——准入、选路、故障转移、记账、request_id 全部
//! 同口径,无第二套实现。
//!
//! 模块分工(阶段 2 拆分):[`context`](RequestCtx) 收敛上下文,
//! [`admission`] 准入,[`dispatch`] 候选迭代,[`attempt`] 单次尝试,
//! [`classify`] 失败分类(9.4 硬边界),[`stream`] 流式转发,
//! [`error`](GatewayError) 统一错误形状。

use std::sync::Arc;
use std::time::Instant;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use tracing::{debug, info_span, warn};

use crate::AppState;
use crate::auth::{AuthContext, Claims};
use crate::channel::selector::spawn_record_failure;
use crate::channel::transport::extract_model;
use aikun_protocol::{Protocol, protocol_error};

pub mod admission;
mod attempt;
mod classify;
pub(crate) mod context;
mod dispatch;
pub(crate) mod error;
mod stream;

use context::{Conversion, RequestCtx};
use error::GatewayError;

/// 可缓存请求的响应头:HIT(命中)/ MISS(未命中,已写入缓存)。
pub(crate) const CACHE_HEADER: &str = "x-aikun-cache";

/// 响应头:请求追踪号(9.6),与 request_logs.request_id、span 同值。
pub(crate) const REQUEST_ID_HEADER: &str = "x-request-id";

/// 允许字节级透传的路径白名单(/v1/ 之后):都是 body 带 model 的 JSON 端点。
/// 不放行 files/batches 等无 model 的端点——无法按模型选路,且会把渠道
/// 凭证暴露给上游的账号级接口。multipart 端点(audio/*)同样不在此列。
const ALLOWED_PASSTHROUGH_PATHS: &[&str] =
    &["embeddings", "moderations", "images/generations", "rerank"];

/// Shared proxy pipeline for both client protocols (OpenAI / Anthropic).
///
/// Selects a provider, converts the request to the provider's native protocol
/// when needed, forwards it, and converts the response (JSON or SSE stream)
/// back to the client protocol. On retryable failures (transport errors,
/// timeouts, 408/429/5xx, and 401/403 which indicate a dead channel) the
/// request automatically fails over to the next available provider — but
/// only while nothing has been written to the client yet (9.4).
pub async fn proxy_completion(
    state: AppState,
    claims: Claims,
    auth_ctx: AuthContext,
    client_protocol: Protocol,
    body: Value,
    headers: &axum::http::HeaderMap,
) -> Response {
    pipeline_entry(
        state,
        claims,
        auth_ctx,
        Conversion::Standard,
        client_protocol,
        body,
        headers,
    )
    .await
}

/// 通用端点的管线入口(阶段 2 并入):字节级透传 = NoConversion +
/// OpenAiOnly + NoStream——不做协议转换,按 model 选路、应用模型映射、
/// 换渠道凭证原样转发,响应 JSON 原样回传。计费:响应 JSON 里有 usage
/// 就按价格表折算(embeddings 有),没有的(images/moderations)按 0 元
/// 记账,日志照常落库。
pub async fn passthrough_completion(
    state: AppState,
    claims: Claims,
    auth_ctx: AuthContext,
    path: String,
    body: Value,
    headers: &axum::http::HeaderMap,
) -> Response {
    if !ALLOWED_PASSTHROUGH_PATHS.contains(&path.as_str()) {
        // 白名单外:404(不进管线、不触上游;仍按 9.6 附 X-Request-Id)
        let resp = (
            StatusCode::NOT_FOUND,
            Json(protocol_error(
                Protocol::OpenAi,
                "not_found_error",
                &format!("Endpoint /v1/{} is not supported by this gateway", path),
            )),
        )
            .into_response();
        return with_request_id(resp, &uuid::Uuid::new_v4().to_string());
    }
    pipeline_entry(
        state,
        claims,
        auth_ctx,
        Conversion::Passthrough { path },
        Protocol::OpenAi,
        body,
        headers,
    )
    .await
}

/// 管线主体:准入(共用同一口径)→ 缓存 → 整体 deadline 内的候选迭代。
async fn pipeline_entry(
    state: AppState,
    claims: Claims,
    auth_ctx: AuthContext,
    conversion: Conversion,
    client_protocol: Protocol,
    body: Value,
    headers: &axum::http::HeaderMap,
) -> Response {
    // ---- request_id + span(9.6):入口生成,贯穿日志、落库与响应头 ----
    let request_id = uuid::Uuid::new_v4().to_string();
    let span = info_span!(
        "proxy_request",
        request_id = %request_id,
        protocol = client_protocol.as_str(),
        model = tracing::field::Empty,
        channel = tracing::field::Empty,
        cache_hit = tracing::field::Empty,
        ttft_ms = tracing::field::Empty,
        total_ms = tracing::field::Empty,
        status = tracing::field::Empty,
    );

    let model = extract_model(&body);
    span.record("model", model.as_str());
    // 热路径每请求日志用 debug 级:生产默认 info 级下保持安静,
    // 高并发时不至于让 stdout 日志 I/O 成为瓶颈。
    debug!(
        request_id = %request_id,
        "Proxy completion: protocol={} model={} user={}{}",
        client_protocol,
        model,
        claims.username,
        if auth_ctx.api_key_id.is_some() {
            " (api-key)"
        } else {
            ""
        }
    );

    let request_type = conversion.request_type(client_protocol);
    let is_stream = !matches!(conversion, Conversion::Passthrough { .. })
        && body["stream"].as_bool().unwrap_or(false);

    // 与 attempt_loop 共享的已尝试渠道(按选择顺序):整体 deadline 取消
    // 在途尝试时,对最后在试渠道补记一次失败——被取消的尝试不会走任何
    // record_failure,不补记的话"假死"渠道在指标里完全隐形。入口持一份
    // 克隆,超时兜底在 ctx 已 Drop 后仍可回读。
    let tried_order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut ctx = RequestCtx {
        request_id: request_id.clone(),
        user_id: claims.sub.clone(),
        api_key_id: auth_ctx.api_key_id.clone(),
        client_protocol,
        model: model.clone(),
        conversion,
        body,
        is_stream,
        tried_order: tried_order.clone(),
        concurrency_guard: None,
        cache_key: None,
        // 9.2:关停派生的子令牌——流转发循环 select 它;客户端断开经
        // 生成器 Drop 生效(见 stream 模块头注释)。
        cancel: state.shutdown.child_token(),
        span: span.clone(),
        started: Instant::now(),
    };

    // ---- 准入(全部拒绝路径都写失败日志,保证审计完整) ----
    // 缺 model:统一拒绝空串与 "unknown"(旧 chat 只拦 unknown、旧透传只拦
    // 空串,合并后取并集)。
    if model.is_empty() || model == "unknown" {
        return admission_reject(&state, &ctx, &span, GatewayError::MissingModel);
    }
    // Enforce the API key's model whitelist (if any): exact match, or prefix
    // match when the entry ends with '*' (e.g. "claude-*" allows claude-*).
    if auth_ctx.allowed_models.is_some()
        && !admission::model_allowed(&auth_ctx.allowed_models, &model)
    {
        return admission_reject(
            &state,
            &ctx,
            &span,
            GatewayError::ModelNotAllowed(model.clone()),
        );
    }
    // 余额预检:余额 <= 0 直接 402 拒绝。放在 key 限额检查之前:余额不足
    // 应稳定返回 402,而不是先消耗 RPM 窗口与并发槽位、刷满限额后漂移成
    // 429。只做入口预检、不做预留金——并发在途请求仍可能把单请求扣成
    // 负数(用量先发生费用后结算),这是有意的取舍,扣费侧注释见
    // insert_request_log。
    if !admission::has_positive_balance(&state, &claims.sub).await {
        return admission_reject(&state, &ctx, &span, GatewayError::InsufficientBalance);
    }
    // API key 的每分钟限流、每日 token 额度与并发上限(仅 /v1 的 key 调用;
    // JWT 会话无此概念)。并发槽位 guard 随 ctx 存活到响应完全结束。
    if let Some(key_id) = &auth_ctx.api_key_id {
        match admission::enforce_key_limits(&state, key_id).await {
            Err(msg) => {
                return admission_reject(&state, &ctx, &span, GatewayError::KeyLimit(msg));
            }
            Ok(guard) => ctx.concurrency_guard = guard,
        }
    }

    // ---- 响应缓存:准入全部通过后才查,命中时不触达任何上游,但照常
    // 记日志(按 AIKUN_CACHE_HIT_BILLING 计费)。透传端点不缓存。 ----
    if matches!(ctx.conversion, Conversion::Standard) {
        ctx.cache_key = state.cache.key_for(
            client_protocol,
            &claims.sub,
            auth_ctx.api_key_id.as_deref(),
            &ctx.body,
            headers,
        );
    }
    if let Some(key) = &ctx.cache_key
        && let Some(hit) = state.cache.get(key).await
    {
        debug!(
            request_id = %request_id,
            "Cache hit: protocol={} model={} user={}",
            client_protocol, model, claims.username
        );
        span.record("cache_hit", true);
        crate::billing::ledger::record_cache_hit(
            &state,
            &request_id,
            claims.sub.clone(),
            auth_ctx.api_key_id.clone(),
            &model,
            &request_type,
            hit.usage,
            state.cache.hit_billing == crate::config::CacheHitBilling::Full,
        )
        .await;
        span.record("total_ms", 0u64);
        span.record("status", 200u16);
        // ctx(含并发槽位)在此 drop,槽位归还。
        return with_request_id(
            (StatusCode::OK, [(CACHE_HEADER, "HIT")], Json(hit.body)).into_response(),
            &request_id,
        );
    }
    span.record("cache_hit", false);

    // ---- 整体 deadline ----
    // 单次尝试的超时取全局与渠道级 timeout_secs 的较大者:渠道允许配到
    // 600s,整体 deadline 若只按全局值算会把它静默截断。取值改走渠道拓扑
    // 快照(roadmap 阶段 3;与旧 SELECT MAX(timeout_secs) WHERE is_active=1
    // 同值),探测/取值仍在 blocking 线程。
    let max_channel_timeout = crate::channel::registry::shared()
        .current(&state.store)
        .await
        .max_timeout_secs();
    let per_attempt_secs = state
        .config
        .request_timeout_secs
        .max(max_channel_timeout.max(0) as u64);
    let max_attempts = state.config.max_retries.max(1);
    // 重试循环外包整体超时:避免 max_attempts × 单渠道超时让一次请求挂
    // 数分钟,超时按 504 返回并写失败日志。下限 300s;若单次超时 × 尝试
    // 次数超过下限则随之放大,保证全局与渠道级超时配置都实际生效。
    let overall_timeout = std::time::Duration::from_secs(300).max(std::time::Duration::from_secs(
        max_attempts as u64 * per_attempt_secs + 15,
    ));

    let req_start = ctx.started;
    match tokio::time::timeout(overall_timeout, dispatch::attempt_loop(state.clone(), ctx)).await {
        Ok(resp) => {
            // 非流式:响应即终点,此处打总耗时与状态;流式的两个字段由
            // stream 模块的记账收尾在流真正结束时打(生成器持有 ctx.span)。
            if !is_stream {
                span.record("total_ms", req_start.elapsed().as_millis() as u64);
                span.record("status", resp.status().as_u16());
            }
            with_request_id(resp, &request_id)
        }
        Err(_) => {
            let total_ms = req_start.elapsed().as_millis() as u64;
            span.record("total_ms", total_ms);
            span.record("status", 504u16);
            warn!(
                request_id = %request_id,
                "Overall retry deadline ({}s) exceeded: model={} attempts={}",
                overall_timeout.as_secs(),
                model,
                max_attempts
            );
            let last_provider = tried_order.lock().ok().and_then(|t| t.last().cloned());
            if let Some(pid) = last_provider.clone() {
                spawn_record_failure(
                    state.store.clone(),
                    pid,
                    req_start.elapsed().as_millis() as f64,
                    0,
                    state.config.auto_disable_threshold,
                    true,
                );
            }
            let err = GatewayError::OverallTimeout(overall_timeout.as_secs());
            crate::billing::ledger::spawn_request_log(
                &state,
                crate::billing::ledger::RequestLogParams::failure(
                    &request_id,
                    claims.sub.clone(),
                    auth_ctx.api_key_id.clone(),
                    last_provider,
                    &model,
                    &request_type,
                    req_start.elapsed().as_millis() as i32,
                    504,
                    err.log_message(),
                ),
            );
            with_request_id(err.into_response(client_protocol), &request_id)
        }
    }
}

/// 准入拒绝:写失败日志(零延迟、零用量)+ span 状态 + 错误响应。
fn admission_reject(
    state: &AppState,
    ctx: &RequestCtx,
    span: &tracing::Span,
    err: GatewayError,
) -> Response {
    span.record("status", err.status_code().as_u16());
    crate::billing::ledger::spawn_request_log(
        state,
        crate::billing::ledger::RequestLogParams::failure(
            &ctx.request_id,
            ctx.user_id.clone(),
            ctx.api_key_id.clone(),
            None,
            &ctx.model,
            &ctx.request_type(),
            0,
            err.log_status(),
            err.log_message(),
        ),
    );
    with_request_id(err.into_response(ctx.client_protocol), &ctx.request_id)
}

/// 给响应挂 `X-Request-Id`(9.6):所有 /v1 出口(成功、错误、SSE)统一。
fn with_request_id(mut resp: Response, request_id: &str) -> Response {
    if let Ok(v) = request_id.parse() {
        resp.headers_mut().insert(REQUEST_ID_HEADER, v);
    }
    resp
}

/// Rewrite the upstream body's model field using the channel's model mapping.
pub(crate) fn apply_model_mapping(provider: &crate::models::Provider, upstream_body: &mut Value) {
    if provider.model_mapping.is_empty() {
        return;
    }
    let mapping: std::collections::HashMap<String, String> =
        match serde_json::from_str(&provider.model_mapping) {
            Ok(m) => m,
            Err(_) => return,
        };
    if mapping.is_empty() {
        return;
    }
    let current = upstream_body["model"].as_str().unwrap_or("").to_string();
    // Exact match first, then the longest matching prefix key (deterministic,
    // unlike iterating a HashMap and taking the first prefix hit).
    let mapped = mapping.get(&current).or_else(|| {
        mapping
            .keys()
            .filter(|k| !k.is_empty() && current.starts_with(k.as_str()))
            .max_by_key(|k| k.len())
            .and_then(|k| mapping.get(k))
    });
    if let Some(target) = mapped {
        debug!("Model mapping: '{}' → '{}'", current, target);
        upstream_body["model"] = json!(target);
    }
}

/// 读取上游 JSON 响应体(8MB 上限),错误转成对话路径的英文文案。
/// 统一实现见 [`crate::channel::transport::read_json_limited`]。
pub(crate) async fn read_json_limited(resp: reqwest::Response) -> Result<Value, String> {
    crate::channel::transport::read_json_limited(resp, crate::channel::transport::MAX_RESP_BODY)
        .await
        .map_err(|e| e.to_string())
}

/// 截断 JSON 值用于日志/审计:上游错误信封可能很大,只保留前 200 字符。
fn truncate_json(v: &Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    s.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Provider;

    fn provider_with_mapping(mapping: &str) -> Provider {
        Provider {
            id: "p".into(),
            name: "p".into(),
            provider_type: "openai".into(),
            openai_base_url: "http://x".into(),
            anthropic_base_url: "http://x".into(),
            api_key: "k".into(),
            key_decrypt_failed: false,
            models: "[]".into(),
            priority: 0,
            weight: 1.0,
            is_active: true,
            health_status: "healthy".into(),
            latency_ms: 0.0,
            error_rate: 0.0,
            last_health_check: None,
            max_retries: 3,
            timeout_secs: 120,
            proxy_url: String::new(),
            model_mapping: mapping.into(),
            consecutive_failures: 0,
            disabled_reason: String::new(),
            protocols: "[\"openai\"]".into(),
            default_protocol: "openai".into(),
            note: String::new(),
            website_url: String::new(),
            channel_kind: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn mapping_exact_match_wins() {
        let p = provider_with_mapping(r#"{"gpt-4": "real-gpt-4", "gpt-": "fallback"}"#);
        let mut body = json!({"model": "gpt-4"});
        apply_model_mapping(&p, &mut body);
        assert_eq!(body["model"], "real-gpt-4");
    }

    #[test]
    fn mapping_longest_prefix_wins() {
        let p = provider_with_mapping(
            r#"{"gpt-": "short", "gpt-4o": "six-chars", "gpt-4": "five-chars"}"#,
        );
        // "gpt-4o" (len 6) beats "gpt-4" (len 5) beats "gpt-" (len 4).
        let mut body = json!({"model": "gpt-4o-mini"});
        apply_model_mapping(&p, &mut body);
        assert_eq!(body["model"], "six-chars");

        let mut body = json!({"model": "gpt-4-turbo"});
        apply_model_mapping(&p, &mut body);
        assert_eq!(body["model"], "five-chars");
    }

    #[test]
    fn mapping_empty_key_never_matches() {
        let p = provider_with_mapping(r#"{"": "catch-all"}"#);
        let mut body = json!({"model": "anything"});
        apply_model_mapping(&p, &mut body);
        assert_eq!(body["model"], "anything");
    }

    #[test]
    fn mapping_no_match_or_malformed_is_noop() {
        let p = provider_with_mapping(r#"{"claude-": "x"}"#);
        let mut body = json!({"model": "gpt-4"});
        apply_model_mapping(&p, &mut body);
        assert_eq!(body["model"], "gpt-4");

        let p = provider_with_mapping("not json");
        apply_model_mapping(&p, &mut body);
        assert_eq!(body["model"], "gpt-4");

        let p = provider_with_mapping("");
        apply_model_mapping(&p, &mut body);
        assert_eq!(body["model"], "gpt-4");
    }

    /// 9.6:所有出口都带 X-Request-Id。
    #[test]
    fn with_request_id_sets_header() {
        let resp = with_request_id(
            (StatusCode::OK, Json(json!({"ok": 1}))).into_response(),
            "0b0f0f0f-1111-2222-3333-444444444444",
        );
        assert_eq!(
            resp.headers().get("x-request-id").unwrap(),
            "0b0f0f0f-1111-2222-3333-444444444444"
        );
    }

    /// 透传白名单:旧四端点放行,其余(含 audio 多部分端点)拒绝。
    #[test]
    fn passthrough_whitelist_unchanged() {
        for allowed in ["embeddings", "moderations", "images/generations", "rerank"] {
            assert!(ALLOWED_PASSTHROUGH_PATHS.contains(&allowed));
        }
        for denied in [
            "files",
            "audio/transcriptions",
            "batches",
            "chat/completions",
        ] {
            assert!(!ALLOWED_PASSTHROUGH_PATHS.contains(&denied));
        }
    }
}
