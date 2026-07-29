use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::{
    extract::State,
    http::StatusCode,
    response::{
        sse::{Event, Sse},
        IntoResponse, Response,
    },
    Json,
};
use futures_util::StreamExt;
use rusqlite::params;
use serde_json::{json, Value};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::auth::{AuthContext, Claims};
use crate::db::DbPool;
use crate::models::Provider;
use crate::proxy::client::{extract_model, send_request};
use crate::proxy::convert::{
    convert_request, convert_response, extract_usage_any, parse_sse_events,
    parse_sse_remaining, valid_response_shape, StreamConverter, SseOut, PROTOCOL_OPENAI,
};
use crate::router::selector::{
    list_available_models_async, select_provider_async, spawn_record_failure, spawn_record_result,
};
use crate::AppState;

/// Unified chat completions endpoint (OpenAI-compatible).
pub async fn chat_completion(
    State(state): State<AppState>,
    claims: Claims,
    auth_ctx: AuthContext,
    Json(body): Json<Value>,
) -> Response {
    proxy_completion(state, claims, auth_ctx, PROTOCOL_OPENAI, body).await
}

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
    client_protocol: &str,
    body: Value,
) -> Response {
    let model = extract_model(&body);
    if model == "unknown" {
        // 缺 model 的 400 也写一条失败日志，保证审计完整
        spawn_request_log(
            &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(), None,
            model.clone(), client_protocol, 0, 0, 0, 0, 400, false,
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
    if let Some(allowed) = &auth_ctx.allowed_models {
        let model_allowed = allowed.iter().any(|m| match m.strip_suffix('*') {
            Some(prefix) => model.starts_with(prefix),
            None => m == &model,
        });
        if !model_allowed {
            spawn_request_log(
                &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(), None,
                model.clone(), client_protocol, 0, 0, 0, 0, 403, false,
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

    // 热路径每请求日志用 debug 级:生产默认 info 级下保持安静,
    // 高并发时不至于让 stdout 日志 I/O 成为瓶颈。
    debug!(
        "Proxy completion: protocol={} model={} user={}{}",
        client_protocol, model, claims.username,
        if auth_ctx.api_key_id.is_some() { " (api-key)" } else { "" }
    );

    let is_stream = body["stream"].as_bool().unwrap_or(false);
    let max_attempts = state.config.max_retries.max(1);

    // 重试循环外包整体超时：避免 max_attempts × 单渠道超时让一次请求挂数分钟，
    // 超时按 504 返回并写失败日志。下限 300s;若配置的单次超时 × 尝试次数
    // 超过下限则随之放大,保证 REQUEST_TIMEOUT_SECS 配置实际生效。
    let overall_timeout = std::time::Duration::from_secs(300).max(
        std::time::Duration::from_secs(
            max_attempts as u64 * state.config.request_timeout_secs + 15,
        ),
    );
    match tokio::time::timeout(
        overall_timeout,
        attempt_loop(
            state.clone(), client_protocol, model.clone(), body, is_stream,
            claims.sub.clone(), auth_ctx.api_key_id.clone(),
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
            spawn_request_log(
                &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(), None,
                model.clone(), client_protocol, 0, 0, 0, 0, 504, false,
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

/// 渠道重试/故障转移循环：依次尝试可用渠道，全部失败时返回最后一次上游
/// 错误（或 503）。流式请求在拿到 200 头后直接移交 stream_response。
#[allow(clippy::too_many_arguments)]
async fn attempt_loop(
    state: AppState,
    client_protocol: &str,
    model: String,
    body: Value,
    is_stream: bool,
    user_id: String,
    api_key_id: Option<String>,
) -> Response {
    let max_attempts = state.config.max_retries.max(1);
    let threshold = state.config.auto_disable_threshold;

    let mut tried: HashSet<String> = HashSet::new();
    let mut last_error: Option<(Value, u16)> = None;

    for _ in 0..max_attempts {
        let provider = match select_provider_async(state.pool.clone(), model.clone(), tried.clone()).await {
            Some(p) => p,
            None => break,
        };
        tried.insert(provider.id.clone());

        let provider_protocol = crate::models::channel_protocol(&provider);
        debug!(
            "Routing '{}' ({} client) → provider '{}' ({} upstream, health={} latency={}ms)",
            model, client_protocol, provider.name, provider_protocol,
            provider.health_status, provider.latency_ms as i64
        );

        // Convert the request into the provider's native protocol, then apply
        // the channel's model mapping (requested model → upstream model).
        let mut upstream_body = convert_request(&body, client_protocol, provider_protocol);
        apply_model_mapping(&provider, &mut upstream_body);

        let (resp, latency) =
            match send_request(&state.clients, &provider, provider_protocol, &upstream_body, is_stream, state.config.request_timeout_secs).await {
                Ok(v) => v,
                Err((err_body, status, latency)) => {
                    warn!(
                        "Transport error from provider {}: status={} — failing over",
                        provider.name, status
                    );
                    spawn_record_failure(state.pool.clone(), provider.id.clone(), latency, 0, threshold);
                    spawn_request_log(
                        &state.pool, user_id.clone(), api_key_id.clone(),
                        Some(provider.id.clone()), model.clone(), client_protocol,
                        0, 0, 0, latency as i32, status as i32, false,
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
                match read_json_limited(resp).await {
                    Ok(resp_body) if valid_response_shape(&resp_body, provider_protocol) => {
                        spawn_record_result(state.pool.clone(), provider.id.clone(), latency, true);
                        let (p, c, t) = extract_usage_any(&resp_body);
                        spawn_request_log(
                            &state.pool, user_id.clone(), api_key_id.clone(),
                            Some(provider.id.clone()), model.clone(), client_protocol,
                            p, c, t, latency as i32, 200, true, None,
                        );
                        debug!(
                            "Completed: model={} provider={} latency={}ms tokens={}",
                            model, provider.name, latency as i64, t
                        );
                        let client_body =
                            convert_response(&resp_body, provider_protocol, client_protocol, &model);
                        return (StatusCode::OK, Json(client_body)).into_response();
                    }
                    result => {
                        let msg = match result {
                            Err(msg) => msg,
                            Ok(_) => "upstream returned a response body that does not match its protocol"
                                .to_string(),
                        };
                        warn!(
                            "Upstream 200 from provider {} but body unusable: {} — failing over",
                            provider.name, msg
                        );
                        spawn_record_failure(state.pool.clone(), provider.id.clone(), latency, 0, threshold);
                        spawn_request_log(
                            &state.pool, user_id.clone(), api_key_id.clone(),
                            Some(provider.id.clone()), model.clone(), client_protocol,
                            0, 0, 0, latency as i32, 502, false, Some(msg.clone()),
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
                let msg = match read_json_limited(resp).await {
                    Ok(_) => "upstream answered a stream request with a non-SSE body".to_string(),
                    Err(m) => m,
                };
                warn!(
                    "Upstream 200 from provider {} but not an SSE stream: {} — failing over",
                    provider.name, msg
                );
                spawn_record_failure(state.pool.clone(), provider.id.clone(), latency, 0, threshold);
                spawn_request_log(
                    &state.pool, user_id.clone(), api_key_id.clone(),
                    Some(provider.id.clone()), model.clone(), client_protocol,
                    0, 0, 0, latency as i32, 502, false, Some(msg.clone()),
                );
                last_error = Some((
                    json!({"error": {"message": msg, "type": "upstream_error"}}),
                    502,
                ));
                continue;
            }
            // 流式不在此记录成功：流结束时统一记一次 record_request_result /
            // record_failure（中途失败），避免指标重复或自相矛盾。
            return stream_response(
                state, resp, client_protocol, provider_protocol,
                provider.id.clone(), provider.timeout_secs, model, user_id,
                api_key_id,
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
        spawn_record_failure(state.pool.clone(), provider.id.clone(), latency, status, threshold);
        spawn_request_log(
            &state.pool, user_id.clone(), api_key_id.clone(),
            Some(provider.id.clone()), model.clone(), client_protocol,
            0, 0, 0, latency as i32, status as i32, false, Some(msg),
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
                &state.pool, user_id.clone(), api_key_id.clone(), None,
                model.clone(), client_protocol, 0, 0, 0, 0, 503, false,
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

/// Rewrite the upstream body's model field using the channel's model mapping.
fn apply_model_mapping(provider: &Provider, upstream_body: &mut Value) {
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

/// Guard that accounts for streams aborted before completion (typically the
/// client disconnecting mid-stream). It lives inside the stream generator:
/// if the generator is dropped early, `Drop` writes a failure log with the
/// usage captured so far. Provider health metrics are intentionally not
/// touched — a client disconnect says nothing about the provider.
struct StreamAbortGuard {
    pool: Arc<crate::db::DbPool>,
    user_id: String,
    api_key_id: Option<String>,
    provider_id: String,
    model: String,
    request_type: String,
    start: Instant,
    usage: Arc<Mutex<(i32, i32, i32)>>,
    finished: bool,
}

impl StreamAbortGuard {
    fn finish(&mut self) {
        self.finished = true;
    }
}

impl Drop for StreamAbortGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let latency = self.start.elapsed().as_millis() as i32;
        let (p, c, t) = self.usage.lock().map(|u| *u).unwrap_or((0, 0, 0));
        // The DB write takes a blocking mutex; dispatch it to the blocking
        // pool so a Drop on the async executor never stalls the runtime.
        let pool = self.pool.clone();
        let user_id = self.user_id.clone();
        let api_key_id = self.api_key_id.clone();
        let provider_id = self.provider_id.clone();
        let model = self.model.clone();
        let request_type = self.request_type.clone();
        let write = move || {
            insert_request_log(
                &pool, &user_id, api_key_id.as_deref(), Some(&provider_id),
                &model, &request_type, p, c, t, latency, 499, false,
                Some("client disconnected".to_string()),
            );
        };
        // Drop 可能发生在无运行时上下文（如关停后）：此时 spawn 会 panic 并
        // abort 进程，故无运行时直接同步写。
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(write);
        } else {
            write();
        }
    }
}

/// Build the SSE streaming response from a successful upstream response.
///
/// `idle_timeout_secs` bounds the gap between upstream chunks: a stalled
/// stream (no bytes for that long) is aborted and counted as a provider
/// failure instead of hanging the connection forever.
#[allow(clippy::too_many_arguments)]
fn stream_response(
    state: AppState,
    resp: reqwest::Response,
    client_protocol: &str,
    provider_protocol: &str,
    provider_id: String,
    idle_timeout_secs: i32,
    model: String,
    user_id: String,
    api_key_id: Option<String>,
) -> Response {
    let byte_stream = resp.bytes_stream();
    let pool = state.pool.clone();
    let threshold = state.config.auto_disable_threshold;
    let model_log = model.clone();
    let start = Instant::now();
    let mut converter = StreamConverter::new(client_protocol, provider_protocol, &model);
    // idle_timeout_secs<=0 的遗留渠道先回退全局 request_timeout_secs 再
    // clamp，与 client.rs 的非流式超时语义一致（避免退化为 1 秒误 abort）。
    let idle_secs = if idle_timeout_secs > 0 {
        idle_timeout_secs as u64
    } else {
        state.config.request_timeout_secs
    };
    let idle_timeout = std::time::Duration::from_secs(idle_secs.clamp(1, 600));
    let client_protocol = client_protocol.to_string();

    // Abort the stream if the upstream sends more than this without a
    // complete SSE event boundary — an unbounded buffer is a memory risk.
    const MAX_SSE_BUF: usize = 1024 * 1024;

    let sse_stream = async_stream::stream! {
        let usage_cell = Arc::new(Mutex::new((0, 0, 0)));
        let mut guard = StreamAbortGuard {
            pool: pool.clone(),
            user_id: user_id.clone(),
            api_key_id: api_key_id.clone(),
            provider_id: provider_id.clone(),
            model: model_log.clone(),
            request_type: client_protocol.clone(),
            start,
            usage: usage_cell.clone(),
            finished: false,
        };
        let mut buf: Vec<u8> = Vec::new();
        // Set when the upstream stream becomes unusable mid-flight (stall,
        // transport error, or unbounded buffer); recorded as a failure below.
        let mut stream_error: Option<String> = None;
        let mut upstream = std::pin::pin!(byte_stream);
        loop {
            let item = match tokio::time::timeout(idle_timeout, upstream.next()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    warn!(
                        "Upstream stream from {} stalled for {:?} — aborting",
                        provider_id, idle_timeout
                    );
                    stream_error = Some(format!(
                        "upstream stream stalled for {}s",
                        idle_timeout.as_secs()
                    ));
                    break;
                }
            };
            match item {
                Ok(bytes) => {
                    buf.extend_from_slice(&bytes);
                    for ev in parse_sse_events(&mut buf) {
                        for out in converter.push(&ev) {
                            yield Ok::<Event, Infallible>(to_axum_event(out));
                        }
                    }
                    if let Ok(mut u) = usage_cell.lock() {
                        *u = converter.usage();
                    }
                    if buf.len() > MAX_SSE_BUF {
                        warn!(
                            "SSE buffer from provider {} exceeded {} bytes without an event boundary — aborting stream",
                            provider_id, MAX_SSE_BUF
                        );
                        stream_error = Some("upstream stream buffer overflow".to_string());
                        break;
                    }
                }
                Err(e) => {
                    warn!("Upstream stream error from {}: {}", provider_id, e);
                    stream_error = Some(format!("upstream stream error: {}", e));
                    break;
                }
            }
        }

        if let Some(err_msg) = stream_error {
            // The upstream stream was unusable — count it as a provider failure.
            let latency = start.elapsed().as_millis() as f64;
            spawn_record_failure(pool.clone(), provider_id.clone(), latency, 0, threshold);
            let (p, c, t) = converter.usage();
            spawn_request_log(
                &pool, user_id.clone(), api_key_id.clone(), Some(provider_id.clone()),
                model_log.clone(), &client_protocol,
                p, c, t, latency as i32, 502, false, Some(err_msg.clone()),
            );
            // 通知客户端流异常终止：协议对应的错误事件 + converter 收尾事件。
            // finish() 对转换型 converter 已含终止帧([DONE]/message_stop),
            // 仅在其缺失时补一个,避免客户端收到双重终止。
            yield Ok(to_axum_event(stream_error_event(&client_protocol, &err_msg)));
            let finish_outs = converter.finish();
            let has_terminal = finish_outs
                .iter()
                .any(|o| o.data == "[DONE]" || o.data.contains("message_stop"));
            for out in finish_outs {
                yield Ok(to_axum_event(out));
            }
            if !has_terminal {
                yield Ok(to_axum_event(stream_terminal_event(&client_protocol)));
            }
            guard.finish();
            return;
        }

        if let Some(ev) = parse_sse_remaining(&mut buf) {
            for out in converter.push(&ev) {
                yield Ok(to_axum_event(out));
            }
        }
        for out in converter.finish() {
            yield Ok(to_axum_event(out));
        }

        // Stream finished — record metrics and request log.
        let latency = start.elapsed().as_millis() as f64;
        spawn_record_result(pool.clone(), provider_id.clone(), latency, true);
        let (p, c, t) = converter.usage();
        spawn_request_log(
            &pool, user_id.clone(), api_key_id.clone(), Some(provider_id.clone()),
            model_log.clone(), &client_protocol,
            p, c, t, latency as i32, 200, true, None,
        );
        debug!(
            "Stream completed: model={} provider={} latency={}ms tokens={}",
            model_log, provider_id, latency as i64, t
        );
        guard.finish();
    };

    Sse::new(sse_stream).into_response()
}

fn to_axum_event(out: SseOut) -> Event {
    let ev = Event::default().data(out.data);
    match out.event {
        Some(name) => ev.event(name),
        None => ev,
    }
}

/// Map an OpenAI-style error type to the closest Anthropic-style one, so the
/// Anthropic error shape still carries the original error category.
fn anthropic_error_type(err_type: &str) -> &'static str {
    match err_type {
        "invalid_request_error" => "invalid_request_error",
        "authentication_error" => "authentication_error",
        "permission_error" | "model_not_allowed" => "permission_error",
        "not_found_error" => "not_found_error",
        "rate_limit_error" => "rate_limit_error",
        "overloaded_error" => "overloaded_error",
        _ => "api_error",
    }
}

/// Build an error body in the client-facing protocol shape.
fn protocol_error(client_protocol: &str, err_type: &str, message: &str) -> Value {
    if client_protocol == crate::proxy::convert::PROTOCOL_ANTHROPIC {
        json!({
            "type": "error",
            "error": {"type": anthropic_error_type(err_type), "message": message}
        })
    } else {
        json!({
            "error": {"message": message, "type": err_type}
        })
    }
}

/// Convert an upstream error body to the client-facing protocol shape,
/// preserving the original error message and type.
fn convert_error(err_body: &Value, client_protocol: &str) -> Value {
    let message = err_body["error"]["message"]
        .as_str()
        .unwrap_or("upstream error");
    let err_type = err_body["error"]["type"]
        .as_str()
        .unwrap_or("upstream_error");
    if client_protocol == crate::proxy::convert::PROTOCOL_ANTHROPIC {
        json!({
            "type": "error",
            "error": {"type": anthropic_error_type(err_type), "message": message}
        })
    } else {
        json!({
            "error": {"message": message, "type": err_type}
        })
    }
}

/// 上游响应体大小上限（8MB）：异常上游的超大 body 不再造成内存 DoS。
const MAX_RESP_BODY: usize = 8 * 1024 * 1024;

/// 读取上游响应 body 并解析为 JSON；读取失败、超过上限或解析失败都返回
/// Err，由调用方按失败路径处理。
async fn read_json_limited(resp: reqwest::Response) -> Result<Value, String> {
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

/// 流中途失败时发给客户端的协议对应错误事件。
fn stream_error_event(client_protocol: &str, message: &str) -> SseOut {
    if client_protocol == crate::proxy::convert::PROTOCOL_ANTHROPIC {
        SseOut {
            event: Some("error".to_string()),
            data: json!({
                "type": "error",
                "error": {"type": "api_error", "message": message}
            })
            .to_string(),
        }
    } else {
        SseOut {
            event: None,
            data: json!({
                "error": {"message": message, "type": "upstream_error"}
            })
            .to_string(),
        }
    }
}

/// 协议对应的流终止帧：OpenAI 为 `[DONE]`，Anthropic 为 `message_stop`。
fn stream_terminal_event(client_protocol: &str) -> SseOut {
    if client_protocol == crate::proxy::convert::PROTOCOL_ANTHROPIC {
        SseOut {
            event: Some("message_stop".to_string()),
            data: json!({"type": "message_stop"}).to_string(),
        }
    } else {
        SseOut {
            event: None,
            data: "[DONE]".to_string(),
        }
    }
}

/// Fire-and-forget 版本：异步上下文写请求日志时投递到 blocking 线程池，
/// 避免持 DB mutex 的同步写阻塞 async executor。
#[allow(clippy::too_many_arguments)]
fn spawn_request_log(
    pool: &Arc<DbPool>,
    user_id: String,
    api_key_id: Option<String>,
    provider_id: Option<String>,
    model: String,
    request_type: &str,
    prompt_tokens: i32,
    completion_tokens: i32,
    total_tokens: i32,
    latency_ms: i32,
    status_code: i32,
    success: bool,
    error_message: Option<String>,
) {
    let pool = pool.clone();
    let request_type = request_type.to_string();
    tokio::task::spawn_blocking(move || {
        insert_request_log(
            &pool, &user_id, api_key_id.as_deref(), provider_id.as_deref(),
            &model, &request_type, prompt_tokens, completion_tokens, total_tokens,
            latency_ms, status_code, success, error_message,
        );
    });
}

#[allow(clippy::too_many_arguments)]
fn insert_request_log(
    pool: &DbPool,
    user_id: &str,
    api_key_id: Option<&str>,
    provider_id: Option<&str>,
    model: &str,
    request_type: &str,
    prompt_tokens: i32,
    completion_tokens: i32,
    total_tokens: i32,
    latency_ms: i32,
    status_code: i32,
    success: bool,
    error_message: Option<String>,
) {
    if let Ok(conn) = pool.conn.lock() {
        // 日志插入失败必须可见:此前静默吞错曾导致迁移中间态下日志全丢。
        if let Err(e) = conn.execute(
            "INSERT INTO request_logs (id, user_id, api_key_id, provider_id, model, request_type,
             prompt_tokens, completion_tokens, total_tokens, latency_ms, status_code, success, error_message, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                Uuid::new_v4().to_string(),
                Some(user_id),
                api_key_id,
                provider_id,
                model,
                request_type,
                prompt_tokens,
                completion_tokens,
                total_tokens,
                latency_ms,
                status_code,
                success as i32,
                error_message,
                chrono::Utc::now().to_rfc3339(),
            ],
        ) {
            warn!("Failed to insert request log: {}", e);
        }
    }
}

/// List available models aggregated from all healthy providers.
/// Returns OpenAI-compatible model list format.
pub async fn list_models(
    State(state): State<AppState>,
    _claims: Claims,
) -> impl IntoResponse {
    let models = list_available_models_async(state.pool.clone()).await;

    let data: Vec<Value> = models
        .into_iter()
        .map(|id| json!({
            "id": id,
            "object": "model",
            "created": chrono::Utc::now().timestamp(),
            "owned_by": "aikun"
        }))
        .collect();

    (StatusCode::OK, Json(json!({
        "object": "list",
        "data": data
    })))
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
