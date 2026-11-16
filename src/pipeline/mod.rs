//! 请求管线(roadmap-v0.5 目标架构的 `pipeline/`):准入 → 选路(渠道亲和 / 加权评分)→ 协议转换 → 渠道插件发送
//! → 故障转移 → 响应/流式转换回客户端协议 → 记账。
//!
//! 三个入站端点(/v1/chat/completions、/v1/messages、/v1/responses)共用
//! [`proxy_completion`],只是 client_protocol 不同。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    http::StatusCode,
    response::{
        IntoResponse, Response,
    },
    Json,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::auth::{AuthContext, Claims};
use crate::billing::ledger::spawn_request_log;
use crate::pipeline::admission::{enforce_key_limits, has_positive_balance, model_allowed};
use crate::models::Provider;
use crate::channel::transport::extract_model;
use crate::protocol::{protocol_error, Protocol};
use crate::channel::selector::spawn_record_failure;
use crate::AppState;

pub mod admission;
mod dispatch;
mod stream;

/// 可缓存请求的响应头:HIT(命中)/ MISS(未命中,已写入缓存)。
pub(crate) const CACHE_HEADER: &str = "x-aikun-cache";

/// Shared proxy pipeline for both client protocols (OpenAI / Anthropic).
///
/// Selects a provider, converts the request to the provider's native protocol
/// when needed, forwards it, and converts the response (JSON or SSE stream)
/// back to the client protocol. On retryable failures (transport errors,
/// timeouts, 408/429/5xx, and 401/403 which indicate a dead channel) the
/// request automatically fails over to the next available provider.
pub async fn proxy_completion(
    state: AppState,
    claims: Claims,
    auth_ctx: AuthContext,
    client_protocol: Protocol,
    body: Value,
    headers: &axum::http::HeaderMap,
) -> Response {
    let model = extract_model(&body);
    if model == "unknown" {
        // 缺 model 的 400 也写一条失败日志，保证审计完整
        spawn_request_log(
            &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(), None,
            model.clone(), client_protocol.as_str(), 0, 0, 0, 0, 0, 400, false,
            Some("missing model in request body".to_string()),
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(protocol_error(client_protocol, "invalid_request_error", "Model field is required in request body")),
        )
            .into_response();
    }

    // Enforce the API key's model whitelist (if any): exact match, or prefix
    // match when the entry ends with '*' (e.g. "claude-*" allows claude-*).
    if auth_ctx.allowed_models.is_some() {
        if !model_allowed(&auth_ctx.allowed_models, &model) {
            spawn_request_log(
                &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(), None,
                model.clone(), client_protocol.as_str(), 0, 0, 0, 0, 0, 403, false,
                Some(format!("model '{}' not allowed for this API key", model)),
            );
            return (
                StatusCode::FORBIDDEN,
                Json(protocol_error(
                    client_protocol,
                    "model_not_allowed",
                    &format!("Model '{}' is not allowed for this API key", model),
                )),
            )
                .into_response();
        }
    }

    // 余额预检:余额 <= 0 直接 402 拒绝。放在 key 限额检查之前:余额不足
    // 应稳定返回 402,而不是先消耗 RPM 窗口与并发槽位、刷满限额后漂移成
    // 429。只做入口预检、不做预留金——并发在途请求仍可能把单请求扣成
    // 负数(用量先发生费用后结算),这是有意的取舍,扣费侧注释见
    // insert_request_log。
    if !has_positive_balance(&state, &claims.sub).await {
        let msg = "Insufficient balance: please top up before making more requests".to_string();
        spawn_request_log(
            &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(), None,
            model.clone(), client_protocol.as_str(), 0, 0, 0, 0, 0, 402, false,
            Some(msg.clone()),
        );
        return (
            StatusCode::PAYMENT_REQUIRED,
            Json(protocol_error(client_protocol, "insufficient_quota", &msg)),
        )
            .into_response();
    }

    // API key 的每分钟限流、每日 token 额度与并发上限(仅 /v1 的 key 调用;
    // JWT 会话无此概念)。并发槽位 guard 须随请求存活到响应完全结束。
    let mut concurrency_guard = None;
    if let Some(key_id) = &auth_ctx.api_key_id {
        match enforce_key_limits(&state, key_id).await {
            Err(msg) => {
                spawn_request_log(
                    &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(), None,
                    model.clone(), client_protocol.as_str(), 0, 0, 0, 0, 0, 429, false,
                    Some(msg.clone()),
                );
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(protocol_error(client_protocol, "rate_limit_error", &msg)),
                )
                    .into_response();
            }
            Ok(guard) => concurrency_guard = guard,
        }
    }

    // 热路径每请求日志用 debug 级:生产默认 info 级下保持安静,
    // 高并发时不至于让 stdout 日志 I/O 成为瓶颈。
    debug!(
        "Proxy completion: protocol={} model={} user={}{}",
        client_protocol, model, claims.username,
        if auth_ctx.api_key_id.is_some() { " (api-key)" } else { "" }
    );

    let is_stream = body["stream"].as_bool().unwrap_or(false);
    let max_attempts = state.config.max_retries.max(1);

    // 响应缓存:准入(白名单 / 余额 / 限额)全部通过后才查缓存,命中时
    // 不触达任何上游,但照常记日志(按 AIKUN_CACHE_HIT_BILLING 计费)。
    let cache_key = state.cache.key_for(client_protocol, &claims.sub, &body, headers);
    if let Some(key) = &cache_key {
        if let Some(hit) = state.cache.get(key).await {
            debug!("Cache hit: protocol={} model={} user={}", client_protocol, model, claims.username);
            crate::billing::ledger::record_cache_hit(
                &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(), model.clone(),
                client_protocol.as_str(), hit.usage,
                state.cache.hit_billing == crate::config::CacheHitBilling::Full,
            )
            .await;
            drop(concurrency_guard);
            return (StatusCode::OK, [(CACHE_HEADER, "HIT")], Json(hit.body)).into_response();
        }
    }

    // 单次尝试的超时取全局与渠道级 timeout_secs 的较大者:渠道允许配到
    // 600s,整体 deadline 若只按全局值算会把它静默截断。查询失败回退全局值。
    let max_channel_timeout = {
        let pool = state.pool.clone();
        tokio::task::spawn_blocking(move || {
            pool.read().lock().ok().and_then(|conn| {
                conn.query_row(
                    "SELECT MAX(timeout_secs) FROM providers WHERE is_active = 1",
                    [],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .ok()
                .flatten()
            })
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(0)
    };
    let per_attempt_secs = state
        .config
        .request_timeout_secs
        .max(max_channel_timeout.max(0) as u64);

    // 重试循环外包整体超时：避免 max_attempts × 单渠道超时让一次请求挂数分钟，
    // 超时按 504 返回并写失败日志。下限 300s;若单次超时 × 尝试次数
    // 超过下限则随之放大,保证全局与渠道级超时配置都实际生效。
    let overall_timeout = std::time::Duration::from_secs(300).max(
        std::time::Duration::from_secs(max_attempts as u64 * per_attempt_secs + 15),
    );

    // 与 attempt_loop 共享的已尝试渠道(按选择顺序):整体 deadline 取消
    // 在途尝试时,对最后在试渠道补记一次失败——被取消的尝试不会走任何
    // record_failure,不补记的话"假死"渠道在指标里完全隐形。
    let tried_order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let req_start = Instant::now();
    match tokio::time::timeout(
        overall_timeout,
        dispatch::attempt_loop(
            state.clone(), client_protocol, model.clone(), body, is_stream,
            claims.sub.clone(), auth_ctx.api_key_id.clone(), tried_order.clone(),
            concurrency_guard, cache_key,
        ),
    )
    .await
    {
        Ok(resp) => resp,
        Err(_) => {
            warn!(
                "Overall retry deadline ({}s) exceeded: model={} attempts={}",
                overall_timeout.as_secs(), model, max_attempts
            );
            let last_provider = tried_order.lock().ok().and_then(|t| t.last().cloned());
            if let Some(pid) = last_provider.clone() {
                spawn_record_failure(
                    state.pool.clone(), pid,
                    req_start.elapsed().as_millis() as f64, 0,
                    state.config.auto_disable_threshold, true,
                );
            }
            spawn_request_log(
                &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(), last_provider,
                model.clone(), client_protocol.as_str(), 0, 0, 0, 0, 0, 504, false,
                Some(format!("overall timeout after {}s", overall_timeout.as_secs())),
            );
            (
                StatusCode::GATEWAY_TIMEOUT,
                Json(protocol_error(
                    client_protocol,
                    "timeout_error",
                    "Request timed out: upstream attempts exceeded the overall deadline",
                )),
            )
                .into_response()
        }
    }
}

/// Rewrite the upstream body's model field using the channel's model mapping.
pub(crate) fn apply_model_mapping(provider: &Provider, upstream_body: &mut Value) {
    if provider.model_mapping.is_empty() {
        return;
    }
    let mapping: HashMap<String, String> =
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

/// 上游响应体大小上限（8MB）：异常上游的超大 body 不再造成内存 DoS。
const MAX_RESP_BODY: usize = 8 * 1024 * 1024;

/// 读取上游响应 body 并解析为 JSON；读取失败、超过上限或解析失败都返回
/// Err，由调用方按失败路径处理。
pub(crate) async fn read_json_limited(resp: reqwest::Response) -> Result<Value, String> {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("failed to read upstream body: {}", e))?;
        if buf.len() + chunk.len() > MAX_RESP_BODY {
            return Err(format!(
                "upstream response body exceeded {} bytes",
                MAX_RESP_BODY
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&buf).map_err(|e| format!("failed to parse upstream response: {}", e))
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
        let p = provider_with_mapping(r#"{"gpt-": "short", "gpt-4o": "six-chars", "gpt-4": "five-chars"}"#);
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
}
