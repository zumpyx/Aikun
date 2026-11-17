//! 候选渠道迭代、单次尝试与失败分类(原 attempt_loop)。
//! roadmap 阶段 2 将继续拆为 候选迭代 / 单次 attempt / 失败分类 三个单元。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use tracing::{debug, warn};

use super::*;
use crate::AppState;
use crate::billing::ledger::{request_log_sync, spawn_request_log};
use crate::channel::selector::{
    select_provider_async, spawn_pin_channel, spawn_record_failure, spawn_record_result,
};
use crate::channel::transport::send_request;
use crate::pipeline::admission::ConcurrencyGuard;
use aikun_protocol::{
    Protocol, convert_error, convert_request, convert_response, extract_usage_any, protocol_error,
    valid_response_shape,
};

/// 渠道重试/故障转移循环：依次尝试可用渠道，全部失败时返回最后一次上游
/// 错误（或 503）。流式请求在拿到 200 头后直接移交 stream_response。
#[allow(clippy::too_many_arguments)]
pub(super) async fn attempt_loop(
    state: AppState,
    client_protocol: Protocol,
    model: String,
    body: Value,
    is_stream: bool,
    user_id: String,
    api_key_id: Option<String>,
    tried_order: Arc<std::sync::Mutex<Vec<String>>>,
    concurrency_guard: Option<ConcurrencyGuard>,
    cache_key: Option<String>,
) -> Response {
    let max_attempts = state.config.max_retries.max(1);
    let threshold = state.config.auto_disable_threshold;
    // 并发槽位在本函数作用域持有:失败/非流式成功路径随函数返回释放;
    // 流式路径在移交 stream_response 时被 move 进流生成器。
    let mut concurrency_guard = concurrency_guard;

    let mut tried: HashSet<String> = HashSet::new();
    // 每个渠道在本请求内的尝试次数:受渠道级 max_retries(重试次数,0 = 不重试)
    // 约束——同一渠道最多尝试 max_retries + 1 次,达上限后本请求内不再选它。
    let mut per_provider_attempts: HashMap<String, u32> = HashMap::new();
    // 本请求已对哪些渠道累计过 consecutive_failure:selector 在候选耗尽时
    // 会重置排除集,同一渠道可能被反复选回;重试造成的重复失败不重复累加,
    // 防止失败计数被重试放大(见 record_failure 的 count_consecutive)。
    let mut failure_counted: HashSet<String> = HashSet::new();
    let mut last_error: Option<(Value, u16)> = None;

    for _ in 0..max_attempts {
        let provider = match select_provider_async(
            state.store.clone(),
            model.clone(),
            tried.clone(),
            api_key_id.clone(),
        )
        .await
        {
            Some(p) => p,
            None => break,
        };
        tried.insert(provider.id.clone());
        if let Ok(mut t) = tried_order.lock() {
            t.push(provider.id.clone());
        }

        // 渠道级重试上限:该渠道已尝试 max_retries + 1 次后跳过,换下一个。
        let attempts = per_provider_attempts
            .entry(provider.id.clone())
            .or_insert(0);
        *attempts += 1;
        if *attempts > provider.max_retries.max(0) as u32 + 1 {
            debug!(
                "Provider {} reached its per-channel retry limit ({}) — skipping",
                provider.name, provider.max_retries
            );
            continue;
        }

        // 密钥解密失败(AIKUN_ENCRYPTION_KEY/AIKUN_JWT_SECRET 被换)的渠道
        // 直接跳过,记一次普通失败:绝不能把空凭证发给上游——上游 401 会
        // 触发渠道立即禁用,换掉 JWT secret 就会连锁打挂全部渠道。
        if provider.key_decrypt_failed {
            tracing::error!(
                "Provider '{}' 密钥解密失败,跳过该渠道——检查 AIKUN_ENCRYPTION_KEY/AIKUN_JWT_SECRET 是否变更",
                provider.name
            );
            spawn_record_failure(
                state.store.clone(),
                provider.id.clone(),
                0.0,
                0,
                threshold,
                failure_counted.insert(provider.id.clone()),
            );
            continue;
        }

        let provider_protocol = crate::models::upstream_protocol_for(&provider, client_protocol);
        debug!(
            "Routing '{}' ({} client) → provider '{}' ({} upstream, health={} latency={}ms)",
            model,
            client_protocol,
            provider.name,
            provider_protocol,
            provider.health_status,
            provider.latency_ms as i64
        );

        // Convert the request into the provider's native protocol, then apply
        // the channel's model mapping (requested model → upstream model).
        let mut upstream_body = convert_request(&body, client_protocol, provider_protocol);
        apply_model_mapping(&provider, &mut upstream_body);

        let adapter = aikun_channels::adapter_for(&provider);
        let (resp, latency) = match send_request(
            &state.clients,
            &provider,
            provider_protocol,
            upstream_body,
            is_stream,
            state.config.request_timeout_secs,
        )
        .await
        {
            Ok(v) => v,
            Err((err_body, status, latency)) => {
                warn!(
                    "Transport error from provider {}: status={} — failing over",
                    provider.name, status
                );
                spawn_record_failure(
                    state.store.clone(),
                    provider.id.clone(),
                    latency,
                    0,
                    threshold,
                    failure_counted.insert(provider.id.clone()),
                );
                spawn_request_log(
                    &state,
                    user_id.clone(),
                    api_key_id.clone(),
                    Some(provider.id.clone()),
                    model.clone(),
                    client_protocol.as_str(),
                    0,
                    0,
                    0,
                    0,
                    latency as i32,
                    status as i32,
                    false,
                    Some("upstream transport error".to_string()),
                );
                last_error = Some((err_body, status));
                continue;
            }
        };

        let status = resp.status().as_u16();

        if status == 200 {
            if !is_stream {
                // 读取并解析响应体（带 8MB 上限）；上游 200 但 body 不可用
                // 时按失败处理并故障转移，绝不把错误形状 JSON 当 200 返回。
                let read = read_json_limited(resp).await.map(|mut b| {
                    adapter.normalize_response(provider_protocol, &mut b);
                    b
                });
                match read {
                    Ok(resp_body) if valid_response_shape(&resp_body, provider_protocol) => {
                        spawn_record_result(
                            state.store.clone(),
                            provider.id.clone(),
                            latency,
                            true,
                        );
                        spawn_pin_channel(
                            state.store.clone(),
                            api_key_id.clone(),
                            model.clone(),
                            provider.id.clone(),
                        );
                        let (p, c, t, k) = extract_usage_any(&resp_body);
                        // 计费级:响应返回前日志与扣费已落库(await)。
                        request_log_sync(
                            &state,
                            user_id.clone(),
                            api_key_id.clone(),
                            Some(provider.id.clone()),
                            model.clone(),
                            client_protocol.as_str(),
                            p,
                            c,
                            t,
                            k,
                            latency as i32,
                            200,
                            true,
                            None,
                        )
                        .await;
                        debug!(
                            "Completed: model={} provider={} latency={}ms tokens={}",
                            model, provider.name, latency as i64, t
                        );
                        let client_body = convert_response(
                            &resp_body,
                            provider_protocol,
                            client_protocol,
                            &model,
                        );
                        if let Some(key) = cache_key {
                            // 同步写入(内存后端为纳秒级),
                            // 保证响应返回后紧接着的同一请求必然命中。
                            let entry = crate::cache::CacheEntry {
                                body: client_body.clone(),
                                usage: (p, c, t, k),
                            };
                            state.cache.put(&key, entry).await;
                            return (StatusCode::OK, [(CACHE_HEADER, "MISS")], Json(client_body))
                                .into_response();
                        }
                        return (StatusCode::OK, Json(client_body)).into_response();
                    }
                    result => {
                        // 审计/服务端日志带截断的上游响应体(便于排查
                        // MiniMax 这类 200+错误信封的上游),客户端只收通用文案。
                        let (msg, detail) = match &result {
                            Err(msg) => (msg.clone(), msg.clone()),
                            Ok(resp_body) => (
                                "upstream returned a response body that does not match its protocol"
                                    .to_string(),
                                format!(
                                    "upstream returned a response body that does not match its protocol: {}",
                                    truncate_json(resp_body)
                                ),
                            ),
                        };
                        warn!(
                            "Upstream 200 from provider {} but body unusable: {} — failing over",
                            provider.name, detail
                        );
                        spawn_record_failure(
                            state.store.clone(),
                            provider.id.clone(),
                            latency,
                            0,
                            threshold,
                            failure_counted.insert(provider.id.clone()),
                        );
                        spawn_request_log(
                            &state,
                            user_id.clone(),
                            api_key_id.clone(),
                            Some(provider.id.clone()),
                            model.clone(),
                            client_protocol.as_str(),
                            0,
                            0,
                            0,
                            0,
                            latency as i32,
                            502,
                            false,
                            Some(detail),
                        );
                        last_error = Some((
                            json!({"error": {"message": msg, "type": "upstream_error"}}),
                            502,
                        ));
                        continue;
                    }
                }
            }
            // 流式:上游必须以 SSE 回应。部分网关对流式请求返回 200 +
            // JSON 错误体(限流/鉴权失败),若直接移交 stream_response
            // 会把错误 JSON 当数据帧转发并按成功记账。
            let is_sse = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_ascii_lowercase().contains("text/event-stream"))
                .unwrap_or(false);
            if !is_sse {
                // 同上:日志带截断的上游响应体,客户端只收通用文案。
                let (msg, detail) = match read_json_limited(resp).await {
                    Ok(resp_body) => (
                        "upstream answered a stream request with a non-SSE body".to_string(),
                        format!(
                            "upstream answered a stream request with a non-SSE body: {}",
                            truncate_json(&resp_body)
                        ),
                    ),
                    Err(m) => (m.clone(), m),
                };
                warn!(
                    "Upstream 200 from provider {} but not an SSE stream: {} — failing over",
                    provider.name, detail
                );
                spawn_record_failure(
                    state.store.clone(),
                    provider.id.clone(),
                    latency,
                    0,
                    threshold,
                    failure_counted.insert(provider.id.clone()),
                );
                spawn_request_log(
                    &state,
                    user_id.clone(),
                    api_key_id.clone(),
                    Some(provider.id.clone()),
                    model.clone(),
                    client_protocol.as_str(),
                    0,
                    0,
                    0,
                    0,
                    latency as i32,
                    502,
                    false,
                    Some(detail),
                );
                last_error = Some((
                    json!({"error": {"message": msg, "type": "upstream_error"}}),
                    502,
                ));
                continue;
            }
            // 流式不在此记录成功：流结束时统一记一次 record_request_result /
            // record_failure（中途失败），避免指标重复或自相矛盾。
            // 渠道亲和在交出 SSE 时就钉上:上游已经接受了这条对话。流中途
            // 失败也不会改钉,下一次仍先试这个渠道,再失败才换。
            // 客户端是否自己请求了 include_usage:未请求时转发前剥掉
            // 网关注入的 usage chunk。
            let client_wants_usage = body["stream_options"]["include_usage"]
                .as_bool()
                .unwrap_or(false);
            spawn_pin_channel(
                state.store.clone(),
                api_key_id.clone(),
                model.clone(),
                provider.id.clone(),
            );
            return super::stream::stream_response(
                state,
                resp,
                adapter,
                client_protocol,
                provider_protocol,
                provider.id.clone(),
                provider.timeout_secs,
                model,
                user_id,
                api_key_id,
                client_wants_usage,
                concurrency_guard.take(),
            );
        }

        // --- Upstream returned an HTTP error ---
        let err_body: Value = read_json_limited(resp)
            .await
            .unwrap_or_else(|_| json!({"error": {"message": "upstream error"}}));
        let msg = err_body["error"]["message"]
            .as_str()
            .unwrap_or("upstream error")
            .to_string();
        warn!(
            "Upstream error: model={} provider={} status={} msg={}",
            model, provider.name, status, msg
        );
        spawn_record_failure(
            state.store.clone(),
            provider.id.clone(),
            latency,
            status,
            threshold,
            failure_counted.insert(provider.id.clone()),
        );
        // 401/403 的上游消息可能回显渠道凭证(one-api 系常见);request_logs
        // 对终端用户可见(/api/logs 按 user_id 过滤),日志只存通用文案,
        // 完整消息仅留在服务端 warn 里。
        let log_msg = if status == 401 || status == 403 {
            format!("upstream authentication failed ({})", status)
        } else {
            msg.clone()
        };
        spawn_request_log(
            &state,
            user_id.clone(),
            api_key_id.clone(),
            Some(provider.id.clone()),
            model.clone(),
            client_protocol.as_str(),
            0,
            0,
            0,
            0,
            latency as i32,
            status as i32,
            false,
            Some(log_msg),
        );

        // Retryable: rate limits, server errors, timeouts, and dead channels
        // (401/403 — the channel's key is bad, not the client's request).
        if status == 401 || status == 403 || status == 408 || status == 429 || status >= 500 {
            last_error = Some((err_body, status));
            continue;
        }

        // Other client errors (400, 404, ...): fail fast, return to caller.
        let client_body = convert_error(&err_body, client_protocol);
        let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
        return (code, Json(client_body)).into_response();
    }

    // All attempts exhausted (or no provider supports the model).
    match last_error {
        Some((err_body, status)) => {
            // Never leak upstream 401/403 to end users — it's the channel's
            // credential that failed, not the client's key.
            if status == 401 || status == 403 {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(protocol_error(
                        client_protocol,
                        "upstream_error",
                        "All upstream providers failed",
                    )),
                )
                    .into_response();
            }
            let client_body = convert_error(&err_body, client_protocol);
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            (code, Json(client_body)).into_response()
        }
        None => {
            // No provider could serve this model at all — record it too, so
            // the audit log reflects every failed request, not just upstream
            // ones.
            spawn_request_log(
                &state,
                user_id.clone(),
                api_key_id.clone(),
                None,
                model.clone(),
                client_protocol.as_str(),
                0,
                0,
                0,
                0,
                0,
                503,
                false,
                Some(format!("no available provider for model '{}'", model)),
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(protocol_error(
                    client_protocol,
                    "provider_unavailable",
                    &format!(
                        "No available provider for model '{}'. Check provider configuration.",
                        model
                    ),
                )),
            )
                .into_response()
        }
    }
}
