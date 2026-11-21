//! 单次尝试(阶段 2 拆分出的第二个可单测单元):对一条已选定的渠道执行
//! 协议转换 → 发送 → 响应分流(非流式 / 流式移交 / 失败)。
//!
//! 失败出口统一为 [`GatewayError`](已记账、已计渠道健康),是否换渠道
//! 由候选迭代(`dispatch::attempt_loop`)依据 [`crate::pipeline::classify`]
//! 的硬边界判定——本单元不做任何重试决策。

use std::sync::Arc;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use tracing::{debug, warn};

use crate::AppState;
use crate::billing::ledger::{RequestLogParams, request_log_sync_billable, spawn_request_log};
use crate::channel::selector::{spawn_pin_channel, spawn_record_failure, spawn_record_result};
use crate::channel::transport::{provider_secrets, send_raw, send_request};
use crate::models::Provider;
use crate::pipeline::GatewayError;
use crate::pipeline::context::RequestCtx;
use crate::pipeline::dispatch::AttemptState;
use aikun_protocol::{
    Protocol, convert_request, convert_response, extract_usage_any, valid_response_shape,
};

/// 单次尝试的出口。
pub(crate) enum AttemptOutcome {
    /// 最终客户端响应(非流式成功 / 致命错误已整形),候选迭代直接返回。
    Done(Response),
    /// 流式移交:上游 200 + SSE。调用方(候选迭代)把整个 ctx move 进
    /// SSE 生成器——移交后**不存在**再尝试的分支(9.4)。
    Stream {
        resp: reqwest::Response,
        adapter: Arc<dyn crate::channel::ChannelAdapter>,
        provider_protocol: Protocol,
        provider_id: String,
        idle_timeout_secs: i32,
    },
    /// 本渠道失败(失败日志与渠道健康已记),交候选迭代分类。
    Failed(GatewayError),
}

/// 对 `provider` 执行一次上游尝试。`state` 是循环共享的记账去重集
/// (`failure_counted`,防止重试放大 consecutive_failures)。
pub(crate) async fn single_attempt(
    state: &AppState,
    ctx: &RequestCtx,
    provider: &Provider,
    attempts: &mut AttemptState,
) -> AttemptOutcome {
    let threshold = state.config.auto_disable_threshold;
    let secrets = provider_secrets(provider);

    // ---- 协议选择与上游体构造 ----
    // 透传模式:无转换,恒按 OpenAI(字节级转发无法跨协议);标准模式按
    // 渠道声明选协议。
    let (provider_protocol, upstream_body) = match &ctx.conversion {
        crate::pipeline::context::Conversion::Passthrough { .. } => {
            let mut upstream_body = ctx.body.clone();
            super::apply_model_mapping(provider, &mut upstream_body);
            (Protocol::OpenAi, upstream_body)
        }
        crate::pipeline::context::Conversion::Standard => {
            let provider_protocol =
                crate::models::upstream_protocol_for(provider, ctx.client_protocol);
            let mut upstream_body =
                convert_request(&ctx.body, ctx.client_protocol, provider_protocol);
            super::apply_model_mapping(provider, &mut upstream_body);
            (provider_protocol, upstream_body)
        }
    };
    let adapter = aikun_channels::adapter_for(provider);

    debug!(
        request_id = %ctx.request_id,
        "Routing '{}' ({} client) → provider '{}' ({} upstream, health={} latency={}ms)",
        ctx.model,
        ctx.client_protocol,
        provider.name,
        provider_protocol,
        provider.health_status,
        provider.latency_ms as i64
    );

    // ---- 发送 ----
    let send = match &ctx.conversion {
        crate::pipeline::context::Conversion::Passthrough { path } => {
            send_raw(
                &state.clients,
                provider,
                path,
                &upstream_body,
                state.config.request_timeout_secs,
                state.config.connect_timeout_secs,
                state.config.ttfb_timeout_secs,
            )
            .await
        }
        crate::pipeline::context::Conversion::Standard => {
            let bm_token = bm_token_of(&state.store, provider).await;
            send_request(
                &state.clients,
                provider,
                provider_protocol,
                upstream_body,
                ctx.is_stream,
                state.config.request_timeout_secs,
                state.config.connect_timeout_secs,
                state.config.ttfb_timeout_secs,
                true, // 真实转发允许 off-peak 取号(每日限额的正当消费者)
                bm_token.as_deref(),
            )
            .await
        }
    };
    let (resp, latency) = match send {
        Ok(v) => v,
        Err((err_body, status, latency)) => {
            let _ = err_body; // 合成 502 体由 GatewayError::transport 复刻
            warn!(
                request_id = %ctx.request_id,
                "Transport error from provider {}: status={} — failing over",
                provider.name, status
            );
            spawn_record_failure(
                state.store.clone(),
                provider.id.clone(),
                latency,
                0,
                threshold,
                attempts.failure_counted.insert(provider.id.clone()),
            );
            spawn_request_log(
                state,
                RequestLogParams::failure(
                    &ctx.request_id,
                    ctx.user_id.clone(),
                    ctx.api_key_id.clone(),
                    Some(provider.id.clone()),
                    &ctx.model,
                    &ctx.request_type(),
                    latency as i32,
                    status as i32,
                    Some("upstream transport error".to_string()),
                ),
            );
            return AttemptOutcome::Failed(GatewayError::transport());
        }
    };

    let status = resp.status().as_u16();

    // ---- 200:成功路径(非流式)或流式移交 ----
    if status == 200 {
        if !ctx.is_stream {
            return non_stream_success(
                state,
                ctx,
                provider,
                provider_protocol,
                &adapter,
                resp,
                latency,
                attempts,
            )
            .await;
        }
        // 流式:上游必须以 SSE 回应。部分网关对流式请求返回 200 + JSON
        // 错误体(限流/鉴权失败),直接移交会把错误 JSON 当数据帧转发并按
        // 成功记账。
        let is_sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_ascii_lowercase().contains("text/event-stream"))
            .unwrap_or(false);
        if !is_sse {
            let (client_msg, detail) = match super::read_json_limited(resp).await {
                Ok(resp_body) => (
                    "upstream answered a stream request with a non-SSE body".to_string(),
                    format!(
                        "upstream answered a stream request with a non-SSE body: {}",
                        super::truncate_json(&resp_body)
                    ),
                ),
                Err(m) => (m.clone(), m),
            };
            // 9.8:上游体可能回显渠道凭证,进日志前先脱敏。
            let detail = crate::channel::transport::redact_secrets(
                &detail,
                &secrets.iter().map(String::as_str).collect::<Vec<_>>(),
            );
            warn!(
                request_id = %ctx.request_id,
                "Upstream 200 from provider {} but not an SSE stream: {} — failing over",
                provider.name, detail
            );
            return bad_body_failure(
                state, ctx, provider, latency, attempts, client_msg, detail, &secrets,
            );
        }
        // 流式不在此记录成功:流结束时统一记一次(见 stream 模块),避免
        // 指标重复或自相矛盾。渠道亲和在交出 SSE 时就钉上:上游已经接受
        // 了这条对话;流中途失败也不改钉,下一次仍先试这个渠道。
        spawn_pin_channel(
            state.store.clone(),
            ctx.api_key_id.clone(),
            ctx.model.clone(),
            provider.id.clone(),
        );
        ctx.span.record("channel", provider.name.as_str());
        // 移交:整个 ctx 将被候选迭代 move 进 SSE 生成器。
        return AttemptOutcome::Stream {
            resp,
            adapter,
            provider_protocol,
            provider_id: provider.id.clone(),
            idle_timeout_secs: provider.timeout_secs,
        };
    }

    // ---- 上游返回 HTTP 错误 ----
    let err_body: serde_json::Value = super::read_json_limited(resp)
        .await
        .unwrap_or_else(|_| serde_json::json!({"error": {"message": "upstream error"}}));
    let error = GatewayError::upstream(
        err_body,
        status,
        ctx.client_protocol,
        ctx.is_passthrough(),
        &secrets,
    );
    warn!(
        request_id = %ctx.request_id,
        "Upstream error: model={} provider={} status={} msg={}",
        ctx.model, provider.name, status, error.log_message().unwrap_or_default()
    );
    spawn_record_failure(
        state.store.clone(),
        provider.id.clone(),
        latency,
        status,
        threshold,
        attempts.failure_counted.insert(provider.id.clone()),
    );
    spawn_request_log(
        state,
        RequestLogParams::failure(
            &ctx.request_id,
            ctx.user_id.clone(),
            ctx.api_key_id.clone(),
            Some(provider.id.clone()),
            &ctx.model,
            &ctx.request_type(),
            latency as i32,
            status as i32,
            error.log_message(),
        ),
    );
    AttemptOutcome::Failed(error)
}

/// 非流式 200 的成功/坏体分流(标准与透传共用;透传跳过协议归一化与
/// 形状校验——字节级语义)。
#[allow(clippy::too_many_arguments)]
async fn non_stream_success(
    state: &AppState,
    ctx: &RequestCtx,
    provider: &Provider,
    provider_protocol: Protocol,
    adapter: &Arc<dyn crate::channel::ChannelAdapter>,
    resp: reqwest::Response,
    latency: f64,
    attempts: &mut AttemptState,
) -> AttemptOutcome {
    let secrets = provider_secrets(provider);
    // 读取并解析响应体(带 8MB 上限);上游 200 但 body 不可用时按失败
    // 处理并故障转移,绝不把错误形状 JSON 当 200 返回。
    let read = super::read_json_limited(resp).await.map(|mut b| {
        if !ctx.is_passthrough() {
            adapter.normalize_response(provider_protocol, &mut b);
        }
        b
    });
    let resp_body = match read {
        Ok(b) if ctx.is_passthrough() || valid_response_shape(&b, provider_protocol) => b,
        result => {
            // 审计/服务端日志带截断的上游响应体(便于排查 MiniMax 这类
            // 200+错误信封的上游),客户端只收通用文案(标准模式)。
            let (client_msg, detail) = match &result {
                Err(msg) => (msg.clone(), msg.clone()),
                Ok(resp_body) => (
                    "upstream returned a response body that does not match its protocol"
                        .to_string(),
                    format!(
                        "upstream returned a response body that does not match its protocol: {}",
                        super::truncate_json(resp_body)
                    ),
                ),
            };
            warn!(
                request_id = %ctx.request_id,
                "Upstream 200 from provider {} but body unusable: {} — failing over",
                provider.name, detail
            );
            return bad_body_failure(
                state, ctx, provider, latency, attempts, client_msg, detail, &secrets,
            );
        }
    };

    // ---- 成功:记账(计费级,await)→ 响应 ----
    spawn_record_result(state.store.clone(), provider.id.clone(), latency, true);
    spawn_pin_channel(
        state.store.clone(),
        ctx.api_key_id.clone(),
        ctx.model.clone(),
        provider.id.clone(),
    );
    let (p, c, t, k) = extract_usage_any(&resp_body);
    // ZCode 闲时免费(23:00-09:00 Flash 上游零消耗):按请求模型名判定,
    // 渠道模型映射不参与(映射到别名则不享受,保守方向)。
    let free_upstream = provider.channel_kind.trim() == aikun_channels::adapters::zcode::KIND
        && aikun_channels::adapters::zcode::upstream_free(
            &ctx.model,
            chrono::Utc::now().timestamp(),
        );
    request_log_sync_billable(
        state,
        RequestLogParams {
            request_id: ctx.request_id.clone(),
            user_id: ctx.user_id.clone(),
            api_key_id: ctx.api_key_id.clone(),
            provider_id: Some(provider.id.clone()),
            model: ctx.model.clone(),
            request_type: ctx.request_type(),
            prompt_tokens: p,
            completion_tokens: c,
            total_tokens: t,
            cached_tokens: k,
            latency_ms: latency as i32,
            status_code: 200,
            success: true,
            error_message: None,
        },
        !free_upstream,
    )
    .await;
    debug!(
        request_id = %ctx.request_id,
        "Completed: model={} provider={} latency={}ms tokens={}",
        ctx.model, provider.name, latency as i64, t
    );
    ctx.span.record("channel", provider.name.as_str());
    ctx.span.record("ttft_ms", latency as u64);
    let client_body = if ctx.is_passthrough() {
        // 字节级透传:响应体原样回传(usage 照常提取计费)。
        resp_body
    } else {
        convert_response(
            &resp_body,
            provider_protocol,
            ctx.client_protocol,
            &ctx.model,
        )
    };
    if let Some(key) = &ctx.cache_key {
        // 同步写入(内存后端为纳秒级),保证响应返回后紧接着的同一请求
        // 必然命中。
        let entry = crate::cache::CacheEntry {
            body: client_body.clone(),
            usage: (p, c, t, k),
        };
        state.cache.put(key, entry).await;
        return AttemptOutcome::Done(
            (
                StatusCode::OK,
                [(super::CACHE_HEADER, "MISS")],
                Json(client_body),
            )
                .into_response(),
        );
    }
    AttemptOutcome::Done((StatusCode::OK, Json(client_body)).into_response())
}

/// "200 但体不可用"失败:渠道健康 + 失败日志 + GatewayError::BadBody。
#[allow(clippy::too_many_arguments)]
fn bad_body_failure(
    state: &AppState,
    ctx: &RequestCtx,
    provider: &Provider,
    latency: f64,
    attempts: &mut AttemptState,
    client_msg: String,
    detail: String,
    secrets: &[String],
) -> AttemptOutcome {
    let error = GatewayError::bad_body(client_msg, detail, secrets);
    spawn_record_failure(
        state.store.clone(),
        provider.id.clone(),
        latency,
        0,
        state.config.auto_disable_threshold,
        attempts.failure_counted.insert(provider.id.clone()),
    );
    spawn_request_log(
        state,
        RequestLogParams::failure(
            &ctx.request_id,
            ctx.user_id.clone(),
            ctx.api_key_id.clone(),
            Some(provider.id.clone()),
            &ctx.model,
            &ctx.request_type(),
            latency as i32,
            502,
            error.log_message(),
        ),
    );
    AttemptOutcome::Failed(error)
}

/// 渠道的 BigModel 业务 token(zcode_oauth 解密;无/失败 None——
/// off-peak PAT 签发的输入;缺省时 offpeak 走 JWT 兜底)。
async fn bm_token_of(store: &crate::store::AppStore, provider: &Provider) -> Option<String> {
    if provider.channel_kind.trim() != aikun_channels::adapters::zcode::KIND {
        return None;
    }
    let row = store.channels.find_zcode_oauth(&provider.id).await.ok()??;
    let plain = crate::crypto::decrypt_or_plain(&store.cipher, &row.business_token);
    (!plain.trim().is_empty()).then_some(plain)
}
