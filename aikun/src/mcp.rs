//! MCP 远程工具透传(`/v1/mcp` 家族)——ZCode 渠道专属。
//!
//! 形态炼化自 ZCode 官方插件体系(2026-11-18 实测本地插件缓存):远程
//! MCP 服务挂在 `{base}/api/v1/mcp/server/{server_key}`,MCP Streamable
//! HTTP(JSON-RPC 2.0 over POST),鉴权 = 渠道同一 JWT(插件声明
//! `auth: zcode_official / jwt_token`),消耗套餐权限(`requiresPaidPlan`,
//! 如天眼查 finance_tianyancha、图片搜索 image_search)。
//!
//! 网关做**字节级透传**:
//! * 选路:活跃 zcode 渠道按管理序(priority DESC, name ASC)逐个尝试,
//!   传输错误 / 5xx / 429 换下一条——与聊天管线同故障转移哲学,但不走
//!   模型选路(MCP 请求不带 model,也无模型映射可言);
//! * 头:渠道侧注入 auth + 伪装头(Anthropic 形态);客户端侧
//!   `Mcp-Session-Id` / `Mcp-Protocol-Version` / `Accept` 白名单转发
//!   (initialize 建的会话归客户端持有,必须原样送达上游);
//! * 响应:状态码 + `mcp-session-id` + body **流式**原样回传(上游可能
//!   回 application/json 单响应,也可能 text/event-stream——由请求方法
//!   与上游决定,网关不猜);
//! * 记账:0 元(上游消耗套餐额度,MCP 响应无 usage 可提),请求日志
//!   照落(model 列 = server_key,request_type = "mcp")。
//!
//! 已知裁剪(v1):GET(SSE 服务端推送)与 DELETE(会话终止)两个
//! Streamable HTTP 方法不做——initialize / tools/list / tools/call 均走
//! POST,单响应形态覆盖绝大多数客户端;长时 SSE 受渠道整体超时约束。

use std::time::Instant;

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::Response,
};
use serde_json::Value;
use tracing::warn;

use aikun_channels::adapters::zcode::{KIND, MCP_FORWARDED_HEADERS, MCP_SERVER_PATH_PREFIX};
use aikun_channels::{adapter_for, base_url_for};
use aikun_protocol::Protocol;

use crate::AppState;
use crate::billing::ledger::{RequestLogParams, spawn_request_log};
use crate::channel::selector::{spawn_record_failure, spawn_record_result};
use crate::channel::transport::cached_client;
use crate::models::Provider;
use crate::relay::{RelayCtx, vendor_error};

/// 单次 MCP 调用最多尝试的 zcode 渠道数(防大渠道池下逐条全试)。
const MAX_MCP_ATTEMPTS: usize = 3;

/// server_key 合法性(与 spec `ServerKey` pattern 一致;生成层已按 pattern
/// 拦截,此处双保险——透传目标拼进 URL,不容任意字符)。
fn valid_server_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 活跃 zcode 渠道,按管理序(priority DESC, name ASC)截断。
async fn zcode_candidates(state: &AppState) -> Vec<Provider> {
    let active = state
        .store
        .channels
        .active_channels()
        .await
        .unwrap_or_default();
    let mut providers: Vec<Provider> = active
        .into_iter()
        .filter(|p| p.channel_kind.trim() == KIND)
        .collect();
    providers.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.name.cmp(&b.name))
    });
    providers.truncate(MAX_MCP_ATTEMPTS);
    providers
}

/// MCP JSON-RPC 调用的透传主流程(见模块文档)。
pub(crate) async fn call_mcp(
    state: &AppState,
    ctx: &RelayCtx,
    server_key: &str,
    body: Value,
) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    if !valid_server_key(server_key) {
        return vendor_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            Some((
                "invalid_server_key",
                "server_key must match [a-zA-Z0-9_-]{1,64}",
            )),
        );
    }
    let candidates = zcode_candidates(state).await;
    if candidates.is_empty() {
        log_mcp_request(
            state,
            ctx,
            &request_id,
            None,
            server_key,
            0.0,
            503,
            false,
            Some("no active zcode channel for MCP".to_string()),
        );
        return vendor_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "provider_unavailable",
            Some(("provider_unavailable", "no active zcode channel for MCP")),
        );
    }

    let threshold = state.config.auto_disable_threshold;
    let start = Instant::now();
    for provider in &candidates {
        // 库内密文 → 明文 JWT(与选路 decrypt_provider 同口径;空 key 的
        // 免鉴权本地上游同样放行——auth_headers 由非空判定驱动)。
        let jwt = crate::crypto::decrypt_or_plain(&state.store.cipher, &provider.api_key);
        let adapter = adapter_for(provider);
        let url = format!(
            "{}{MCP_SERVER_PATH_PREFIX}{server_key}",
            base_url_for(provider, Protocol::Anthropic).trim_end_matches('/')
        );
        let timeout = if provider.timeout_secs > 0 {
            provider.timeout_secs as u64
        } else {
            state.config.request_timeout_secs
        };
        let client = cached_client(
            &state.clients,
            &provider.proxy_url,
            &provider.name,
            timeout,
            state.config.connect_timeout_secs,
        );
        let mut req = client.post(&url).json(&body);
        if !jwt.is_empty() {
            for (k, v) in adapter.auth_headers(&jwt, Protocol::Anthropic) {
                req = req.header(k, v);
            }
            for (k, v) in adapter.extra_headers(Protocol::Anthropic) {
                req = req.header(k, v);
            }
        }
        // 客户端侧 MCP 协议头白名单转发(HeaderMap 取值大小写不敏感)。
        for name in MCP_FORWARDED_HEADERS {
            if let Some(v) = ctx.headers.get(*name) {
                req = req.header(*name, v);
            }
        }
        let latency = start.elapsed().as_millis() as f64;
        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                // 已送达但渠道侧不可用(5xx/429):记失败换下一条;其余
                // 状态(含 4xx 协议错误——未知会话/未知工具/权限不足)是
                // 上游对这条请求的正式回答,原样透传,不转移。
                if status >= 500 || status == 429 {
                    spawn_record_failure(
                        state.store.clone(),
                        provider.id.clone(),
                        latency,
                        status,
                        threshold,
                        true,
                    );
                    warn!(
                        request_id = %request_id,
                        "MCP upstream error from provider {}: status={} — trying next",
                        provider.name, status
                    );
                    continue;
                }
                spawn_record_result(state.store.clone(), provider.id.clone(), latency, true);
                log_mcp_request(
                    state,
                    ctx,
                    &request_id,
                    Some(provider.id.clone()),
                    server_key,
                    latency,
                    status as i32,
                    status < 400,
                    None,
                );
                return passthrough_response(resp);
            }
            Err(e) => {
                spawn_record_failure(
                    state.store.clone(),
                    provider.id.clone(),
                    latency,
                    0,
                    threshold,
                    true,
                );
                warn!(
                    request_id = %request_id,
                    "MCP transport error from provider {}: {} — trying next",
                    provider.name,
                    e.without_url()
                );
                continue;
            }
        }
    }
    let latency = start.elapsed().as_millis() as f64;
    log_mcp_request(
        state,
        ctx,
        &request_id,
        None,
        server_key,
        latency,
        502,
        false,
        Some("mcp upstream failed on all zcode channels".to_string()),
    );
    vendor_error(
        StatusCode::BAD_GATEWAY,
        "upstream_error",
        Some(("upstream_error", "MCP request failed on all zcode channels")),
    )
}

/// 上游响应 → 客户端响应:状态码 + content-type + `mcp-session-id`
/// (initialize 的会话凭证,客户端后续请求要带)+ body 流式透传。
fn passthrough_response(resp: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = axum::http::Response::builder().status(status);
    for header_name in [
        header::CONTENT_TYPE,
        header::HeaderName::from_static("mcp-session-id"),
    ] {
        if let Some(v) = resp.headers().get(&header_name) {
            builder = builder.header(header_name, v.clone());
        }
    }
    match builder.body(Body::from_stream(resp.bytes_stream())) {
        Ok(r) => r,
        Err(_) => vendor_error(StatusCode::BAD_GATEWAY, "upstream_error", None),
    }
}

/// MCP 请求落日志(0 token、0 元;model 列承载 server_key 便于按工具统计)。
#[allow(clippy::too_many_arguments)]
fn log_mcp_request(
    state: &AppState,
    ctx: &RelayCtx,
    request_id: &str,
    provider_id: Option<String>,
    server_key: &str,
    latency_ms: f64,
    status_code: i32,
    success: bool,
    error_message: Option<String>,
) {
    spawn_request_log(
        state,
        RequestLogParams {
            request_id: request_id.to_string(),
            user_id: ctx.claims.sub.clone(),
            api_key_id: ctx.auth_ctx.api_key_id.clone(),
            provider_id,
            model: server_key.to_string(),
            request_type: "mcp".to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            cached_tokens: 0,
            latency_ms: latency_ms as i32,
            status_code,
            success,
            error_message,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_key_validation() {
        assert!(valid_server_key("finance_tianyancha"));
        assert!(valid_server_key("image_search"));
        assert!(valid_server_key("a"));
        assert!(!valid_server_key(""), "空");
        assert!(!valid_server_key("a/b"), "路径分隔符");
        assert!(!valid_server_key("a b"), "空格");
        assert!(!valid_server_key("键"), "非 ASCII");
        assert!(!valid_server_key(&"k".repeat(65)), "超长");
    }
}
