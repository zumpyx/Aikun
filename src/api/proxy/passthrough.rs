//! 通用端点字节级透传:chat/messages/responses 之外的 OpenAI 兼容端点
//! (白名单内)。不做协议转换——按 model 选路、应用模型映射、换渠道凭证
//! 原样转发,响应 JSON 原样回传。仅转发给声明 openai 协议的渠道
//! (字节级转发无法跨协议)。
//! 计费:响应 JSON 里有 usage 就按价格表折算(embeddings 有),没有的
//! (images/moderations)按 0 元记账,日志照常落库。

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use tracing::{debug, warn};

use super::chat::{
    apply_model_mapping, enforce_key_limits, has_positive_balance, protocol_error,
    read_json_limited, request_log_sync, spawn_request_log,
};
use crate::auth::{AuthContext, Claims};
use crate::proxy::client::{build_api_url, cached_client};
use crate::proxy::convert::{extract_usage_any, PROTOCOL_OPENAI};
use crate::router::selector::{select_provider_async, spawn_record_failure, spawn_record_result};
use crate::AppState;

/// 允许透传的路径白名单(/v1/ 之后):都是 body 带 model 的 JSON 端点。
/// 不放行 files/batches 等无 model 的端点——无法按模型选路,且会把渠道
/// 凭证暴露给上游的账号级接口。multipart 端点(audio/*)同样不在此列。
const ALLOWED_PATHS: &[&str] = &[
    "embeddings",
    "moderations",
    "images/generations",
    "rerank",
];

/// POST /v1/{*path} 的兜底 handler(显式路由优先于通配,chat/messages/
/// responses/models 不走这里)。客户端协议按 OpenAI 处理。
pub async fn passthrough(
    State(state): State<AppState>,
    claims: Claims,
    auth_ctx: AuthContext,
    Path(path): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if !ALLOWED_PATHS.contains(&path.as_str()) {
        return (
            StatusCode::NOT_FOUND,
            Json(protocol_error(
                PROTOCOL_OPENAI,
                "not_found_error",
                &format!("Endpoint /v1/{} is not supported by this gateway", path),
            )),
        )
            .into_response();
    }

    let model = body["model"].as_str().unwrap_or("").to_string();
    if model.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(protocol_error(
                PROTOCOL_OPENAI,
                "invalid_request_error",
                "Model field is required in request body",
            )),
        )
            .into_response();
    }

    // API key 的模型白名单(与 chat 同口径:精确或前缀 '*' 匹配)。
    if let Some(allowed) = &auth_ctx.allowed_models {
        let model_allowed = allowed.iter().any(|m| match m.strip_suffix('*') {
            Some(prefix) => model.starts_with(prefix),
            None => m == &model,
        });
        if !model_allowed {
            spawn_request_log(
                &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(), None,
                model.clone(), &path, 0, 0, 0, 0, 0, 403, false,
                Some(format!("model '{}' not allowed for this API key", model)),
            );
            return (
                StatusCode::FORBIDDEN,
                Json(protocol_error(
                    PROTOCOL_OPENAI,
                    "model_not_allowed",
                    &format!("Model '{}' is not allowed for this API key", model),
                )),
            )
                .into_response();
        }
    }

    // 余额预检在 key 限额之前(与 chat 同口径:402 语义稳定)。
    if !has_positive_balance(&state, &claims.sub).await {
        let msg = "Insufficient balance: please top up before making more requests".to_string();
        spawn_request_log(
            &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(), None,
            model.clone(), &path, 0, 0, 0, 0, 0, 402, false, Some(msg.clone()),
        );
        return (
            StatusCode::PAYMENT_REQUIRED,
            Json(protocol_error(PROTOCOL_OPENAI, "insufficient_quota", &msg)),
        )
            .into_response();
    }

    // 并发槽位 guard 持有到函数返回(全部为非流式路径,响应体读完才返回)。
    let mut concurrency_guard = None;
    if let Some(key_id) = &auth_ctx.api_key_id {
        match enforce_key_limits(&state, key_id).await {
            Err(msg) => {
                spawn_request_log(
                    &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(), None,
                    model.clone(), &path, 0, 0, 0, 0, 0, 429, false, Some(msg.clone()),
                );
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(protocol_error(PROTOCOL_OPENAI, "rate_limit_error", &msg)),
                )
                    .into_response();
            }
            Ok(guard) => concurrency_guard = guard,
        }
    }
    let _guard = concurrency_guard.take();

    let max_attempts = state.config.max_retries.max(1);
    let threshold = state.config.auto_disable_threshold;
    let mut tried: HashSet<String> = HashSet::new();
    // 与 chat 同口径:同一渠道最多尝试 max_retries + 1 次;失败计数防重试放大。
    let mut per_provider_attempts: HashMap<String, u32> = HashMap::new();
    let mut failure_counted: HashSet<String> = HashSet::new();
    let mut last_error: Option<(Value, u16)> = None;

    for _ in 0..max_attempts {
        let provider = match select_provider_async(state.pool.clone(), model.clone(), tried.clone()).await {
            Some(p) => p,
            None => break,
        };
        tried.insert(provider.id.clone());

        let attempts = per_provider_attempts.entry(provider.id.clone()).or_insert(0);
        *attempts += 1;
        if *attempts > provider.max_retries.max(0) as u32 + 1 {
            continue;
        }

        // 密钥解密失败的渠道直接跳过(绝不能把空凭证发给上游,见 chat)。
        if provider.key_decrypt_failed {
            tracing::error!(
                "Provider '{}' 密钥解密失败,跳过该渠道——检查 AIKUN_ENCRYPTION_KEY/AIKUN_JWT_SECRET 是否变更",
                provider.name
            );
            spawn_record_failure(
                state.pool.clone(), provider.id.clone(), 0.0, 0, threshold,
                failure_counted.insert(provider.id.clone()),
            );
            continue;
        }

        // 字节级透传无协议转换,只能走声明 openai 协议的渠道;
        // protocols 为空的旧行按 provider_type 兜底。
        let speaks_openai = serde_json::from_str::<Vec<String>>(&provider.protocols)
            .map(|ps| ps.iter().any(|p| p == "openai"))
            .unwrap_or(false)
            || (provider.protocols.trim().is_empty() && provider.provider_type == "openai");
        if !speaks_openai {
            continue;
        }

        // 应用渠道的模型映射(请求模型 → 上游模型),其余字节不动。
        let mut upstream_body = body.clone();
        apply_model_mapping(&provider, &mut upstream_body);
        let url = build_api_url(
            crate::models::base_url_for(&provider, PROTOCOL_OPENAI),
            &format!("/{}", path),
        );
        let timeout = if provider.timeout_secs > 0 {
            provider.timeout_secs as u64
        } else {
            state.config.request_timeout_secs
        };
        let client = cached_client(&state.clients, &provider.proxy_url, &provider.name, timeout);
        let req = client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&upstream_body);
        // 免密钥本地上游不携带 Authorization(空 Bearer 会让部分上游 401)。
        let req = if provider.api_key.is_empty() {
            req
        } else {
            req.header("Authorization", format!("Bearer {}", provider.api_key))
        };

        debug!(
            "Passthrough /v1/{} model={} → provider '{}' (health={})",
            path, model, provider.name, provider.health_status
        );
        let start = Instant::now();
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let latency = start.elapsed().as_millis() as f64;
                warn!(
                    "Transport error from provider {} (/v1/{}): {} — failing over",
                    provider.name, path, e
                );
                spawn_record_failure(
                    state.pool.clone(), provider.id.clone(), latency, 0, threshold,
                    failure_counted.insert(provider.id.clone()),
                );
                spawn_request_log(
                    &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(),
                    Some(provider.id.clone()), model.clone(), &path,
                    0, 0, 0, 0, latency as i32, 502, false,
                    Some("upstream transport error".to_string()),
                );
                last_error = Some((
                    json!({"error": {"message": "upstream transport error", "type": "upstream_error"}}),
                    502,
                ));
                continue;
            }
        };
        let latency = start.elapsed().as_millis() as f64;
        let status = resp.status().as_u16();

        if status == 200 {
            match read_json_limited(resp).await {
                Ok(resp_body) => {
                    spawn_record_result(state.pool.clone(), provider.id.clone(), latency, true);
                    // 有 usage 计费,无 usage(images/moderations)0 元记账;
                    // 计费级:响应返回前日志与扣费已落库(await)。
                    let (p, c, t, k) = extract_usage_any(&resp_body);
                    request_log_sync(
                        &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(),
                        Some(provider.id.clone()), model.clone(), &path,
                        p, c, t, k, latency as i32, 200, true, None,
                    )
                    .await;
                    debug!(
                        "Passthrough completed: /v1/{} model={} provider={} latency={}ms tokens={}",
                        path, model, provider.name, latency as i64, t
                    );
                    return (StatusCode::OK, Json(resp_body)).into_response();
                }
                Err(msg) => {
                    // 200 但 body 不可读/超 8MB:按失败转移(口径同 chat)。
                    warn!(
                        "Upstream 200 from provider {} but body unusable: {} — failing over",
                        provider.name, msg
                    );
                    spawn_record_failure(
                        state.pool.clone(), provider.id.clone(), latency, 0, threshold,
                        failure_counted.insert(provider.id.clone()),
                    );
                    spawn_request_log(
                        &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(),
                        Some(provider.id.clone()), model.clone(), &path,
                        0, 0, 0, 0, latency as i32, 502, false, Some(msg.clone()),
                    );
                    last_error = Some((
                        json!({"error": {"message": msg, "type": "upstream_error"}}),
                        502,
                    ));
                    continue;
                }
            }
        }

        // --- 上游返回 HTTP 错误 ---
        let err_body: Value = read_json_limited(resp)
            .await
            .unwrap_or_else(|_| json!({"error": {"message": "upstream error"}}));
        let msg = err_body["error"]["message"]
            .as_str()
            .unwrap_or("upstream error")
            .to_string();
        warn!(
            "Upstream error: /v1/{} model={} provider={} status={} msg={}",
            path, model, provider.name, status, msg
        );
        spawn_record_failure(
            state.pool.clone(), provider.id.clone(), latency, status, threshold,
            failure_counted.insert(provider.id.clone()),
        );
        // 401/403 的上游消息可能回显渠道凭证,日志只存通用文案(口径同 chat)。
        let log_msg = if status == 401 || status == 403 {
            format!("upstream authentication failed ({})", status)
        } else {
            msg.clone()
        };
        spawn_request_log(
            &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(),
            Some(provider.id.clone()), model.clone(), &path,
            0, 0, 0, 0, latency as i32, status as i32, false, Some(log_msg),
        );

        // 可重试:限流、服务端错误、超时、渠道凭证失效(401/403)。
        if status == 401 || status == 403 || status == 408 || status == 429 || status >= 500 {
            last_error = Some((err_body, status));
            continue;
        }
        // 其余客户端错误(400/404):fail fast,原样回传。
        let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
        return (code, Json(err_body)).into_response();
    }

    // 全部尝试耗尽(或无渠道支持该模型)。
    match last_error {
        Some((err_body, status)) => {
            // 绝不向终端用户泄露上游 401/403(是渠道凭证失败,见 chat)。
            if status == 401 || status == 403 {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(protocol_error(PROTOCOL_OPENAI, "upstream_error", "All upstream providers failed")),
                )
                    .into_response()
            } else {
                let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
                (code, Json(err_body)).into_response()
            }
        }
        None => {
            spawn_request_log(
                &state.pool, claims.sub.clone(), auth_ctx.api_key_id.clone(), None,
                model.clone(), &path, 0, 0, 0, 0, 0, 503, false,
                Some(format!("no available provider for model '{}'", model)),
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(protocol_error(
                    PROTOCOL_OPENAI,
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
