//! [`GatewayError`](thiserror):管线统一的错误形状与可重试判定(阶段 2)。
//!
//! 收敛此前散落在 dispatch / stream / passthrough 里的字符串、`(Value, u16)`
//! 元组与裸 StatusCode:
//! * [`GatewayError::failure_kind`] 是失败分类的单一真相,供候选迭代侧
//!   决定"换渠道 / 快速失败 / 凭证死亡后换渠道"(与 [`crate::pipeline::classify`]
//!   的硬边界判定配合,9.4);
//! * [`GatewayError::into_response`] 生成客户端可见的错误体——标准模式按
//!   客户端协议形状转换,透传模式原样回传上游错误体(字节级语义);
//!   两者都先过 [`crate::channel::transport::redact_json_secrets`] 的
//!   凭证脱敏(9.8)。

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::Value;

use crate::channel::transport::{redact_json_secrets, redact_secrets};
use crate::pipeline::classify::FailureKind;
use aikun_protocol::{Protocol, convert_error, protocol_error};

/// 管线错误:准入拒绝、上游失败、全部渠道耗尽。
///
/// `Upstream`/`Transport`/`BadBody` 携带渠道维度的脱敏材料(secrets 在
/// 构造时传入并就地脱敏,后续任何出口——日志、响应体、span——都不可能
/// 再出现明文)。
#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum GatewayError {
    /// 请求体缺 model 字段(400)。
    #[error("Model field is required in request body")]
    MissingModel,
    /// 模型不在 API key 白名单内(403)。
    #[error("Model '{0}' is not allowed for this API key")]
    ModelNotAllowed(String),
    /// 余额不足(402)。
    #[error("Insufficient balance: please top up before making more requests")]
    InsufficientBalance,
    /// API key 限额(RPM / 日额度 / 并发,429);消息为限额说明。
    #[error("{0}")]
    KeyLimit(String),
    /// 整体重试 deadline 超时(504)。
    #[error("overall timeout after {0}s")]
    OverallTimeout(u64),
    /// 没有任何渠道能服务该模型(503)。
    #[error("No available provider for model '{0}'. Check provider configuration.")]
    NoProvider(String),
    /// 上游返回 HTTP 错误。`body`/`status` 为上游原文(状态码供分类),
    /// `client_body` 为脱敏后的客户端错误体。
    #[error("upstream returned {status}")]
    Upstream {
        status: u16,
        /// 服务端日志文案(401/403 为通用文案防凭证回显,其余为脱敏后的
        /// 上游 error.message)。
        log_message: String,
        /// 上游错误体(服务端日志用,已脱敏)。
        body: Value,
        /// 客户端错误体(已按客户端协议整形并脱敏)。
        client_body: Value,
    },
    /// 传输失败(连接失败 / TTFB 超时 / 整体超时):合成 502。
    #[error("upstream transport error")]
    Transport { client_body: Value },
    /// 上游 200 但响应体不可用(读失败 / 超 8MB / 形状非法 / 非流式请求
    /// 回了 SSE):按 502 处理。
    #[error("upstream returned a response body that does not match its protocol")]
    BadBody {
        /// 服务端日志详情(截断的上游体,已脱敏)。
        detail: String,
        client_body: Value,
    },
}

impl GatewayError {
    /// 从上游 HTTP 错误构造:按模式整形客户端体并做全出口脱敏(9.8)。
    /// `passthrough` = 字节级透传(上游错误体原样回传,仅脱敏)。
    pub(crate) fn upstream(
        err_body: Value,
        status: u16,
        client_protocol: Protocol,
        passthrough: bool,
        secrets: &[String],
    ) -> Self {
        let secret_refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
        // 先脱敏上游原文(服务端日志面),再从干净的材料构造客户端体。
        let body = redact_json_secrets(&err_body, &secret_refs);
        let client_body = if passthrough {
            body.clone()
        } else {
            convert_error(&body, client_protocol)
        };
        // 401/403 的上游消息可能回显渠道凭证(one-api 系常见);request_logs
        // 对终端用户可见(/api/logs 按 user_id 过滤),日志只存通用文案。
        // 其余状态保留脱敏后的上游消息,排查价值不打折。
        let log_message = if status == 401 || status == 403 {
            format!("upstream authentication failed ({})", status)
        } else {
            body["error"]["message"]
                .as_str()
                .unwrap_or("upstream error")
                .to_string()
        };
        Self::Upstream {
            status,
            log_message,
            body,
            client_body,
        }
    }

    /// 从传输失败构造(合成 502 错误体,与旧实现逐字一致)。
    pub(crate) fn transport() -> Self {
        Self::Transport {
            client_body: serde_json::json!({
                "error": {"message": "Upstream request failed", "type": "upstream_error"}
            }),
        }
    }

    /// 从"200 但体不可用"构造;`client_msg` 进客户端错误体、`detail`
    /// 进服务端日志,两者都先过凭证脱敏(9.8)。
    pub(crate) fn bad_body(client_msg: String, detail: String, secrets: &[String]) -> Self {
        let secret_refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
        let detail = redact_secrets(&detail, &secret_refs);
        let client_msg = redact_secrets(&client_msg, &secret_refs);
        Self::BadBody {
            detail,
            client_body: serde_json::json!({
                "error": {"message": client_msg, "type": "upstream_error"}
            }),
        }
    }

    /// 失败分类(单一真相;边界判定见 [`crate::pipeline::classify`])。
    pub(crate) fn failure_kind(&self) -> FailureKind {
        match self {
            GatewayError::Upstream { status, .. } => FailureKind::from_status(*status),
            GatewayError::Transport { .. } | GatewayError::BadBody { .. } => FailureKind::Retryable,
            // 准入拒绝与超时/无渠道是终态,不参与故障转移。
            GatewayError::MissingModel
            | GatewayError::ModelNotAllowed(_)
            | GatewayError::InsufficientBalance
            | GatewayError::KeyLimit(_)
            | GatewayError::OverallTimeout(_)
            | GatewayError::NoProvider(_) => FailureKind::Fatal,
        }
    }

    /// 客户端可见的 HTTP 状态码。
    pub(crate) fn status_code(&self) -> StatusCode {
        match self {
            GatewayError::MissingModel => StatusCode::BAD_REQUEST,
            GatewayError::ModelNotAllowed(_) => StatusCode::FORBIDDEN,
            GatewayError::InsufficientBalance => StatusCode::PAYMENT_REQUIRED,
            GatewayError::KeyLimit(_) => StatusCode::TOO_MANY_REQUESTS,
            GatewayError::OverallTimeout(_) => StatusCode::GATEWAY_TIMEOUT,
            GatewayError::NoProvider(_) => StatusCode::SERVICE_UNAVAILABLE,
            GatewayError::Upstream { status, .. } => {
                // 上游 401/403 是渠道凭证失败,不是客户端请求的错:
                // 穷尽后对客户端一律掩码为 502(见 into_response 注释)。
                if *status == 401 || *status == 403 {
                    StatusCode::BAD_GATEWAY
                } else {
                    StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
                }
            }
            GatewayError::Transport { .. } | GatewayError::BadBody { .. } => {
                StatusCode::BAD_GATEWAY
            }
        }
    }

    /// 日志 error_message(request_logs 对终端用户可见,/api/logs 按
    /// user_id 过滤——401/403 只存通用文案,防上游回显渠道凭证)。
    pub(crate) fn log_message(&self) -> Option<String> {
        match self {
            GatewayError::MissingModel => Some("missing model in request body".to_string()),
            GatewayError::ModelNotAllowed(m) => {
                Some(format!("model '{}' not allowed for this API key", m))
            }
            GatewayError::InsufficientBalance => Some(self.to_string()),
            GatewayError::KeyLimit(_) => Some(self.to_string()),
            GatewayError::OverallTimeout(_) => Some(self.to_string()),
            GatewayError::NoProvider(m) => Some(format!("no available provider for model '{m}'")),
            GatewayError::Upstream { log_message, .. } => Some(log_message.clone()),
            GatewayError::Transport { .. } => Some("upstream transport error".to_string()),
            GatewayError::BadBody { detail, .. } => Some(detail.clone()),
        }
    }

    /// 日志 status_code:与响应状态同值(401/403 落库沿用上游原状态,
    /// 与旧实现一致——掩码只发生在客户端响应面)。
    pub(crate) fn log_status(&self) -> i32 {
        match self {
            GatewayError::Upstream { status, .. } => i32::from(*status),
            _ => self.status_code().as_u16() as i32,
        }
    }

    /// 生成客户端错误响应。准入类错误按客户端协议出协议形状;上游类
    /// 错误回 `client_body`(构造时已整形 + 脱敏)。
    pub(crate) fn into_response(self, client_protocol: Protocol) -> Response {
        match &self {
            GatewayError::MissingModel
            | GatewayError::ModelNotAllowed(_)
            | GatewayError::InsufficientBalance
            | GatewayError::KeyLimit(_)
            | GatewayError::OverallTimeout(_)
            | GatewayError::NoProvider(_) => {
                let err_type = match &self {
                    GatewayError::MissingModel => "invalid_request_error",
                    GatewayError::ModelNotAllowed(_) => "model_not_allowed",
                    GatewayError::InsufficientBalance => "insufficient_quota",
                    GatewayError::KeyLimit(_) => "rate_limit_error",
                    GatewayError::OverallTimeout(_) => "timeout_error",
                    _ => "provider_unavailable",
                };
                let message = match &self {
                    GatewayError::NoProvider(m) => {
                        format!(
                            "No available provider for model '{m}'. Check provider configuration."
                        )
                    }
                    GatewayError::OverallTimeout(_) => {
                        "Request timed out: upstream attempts exceeded the overall deadline"
                            .to_string()
                    }
                    other => other.to_string(),
                };
                (
                    self.status_code(),
                    Json(protocol_error(client_protocol, err_type, &message)),
                )
                    .into_response()
            }
            GatewayError::Upstream {
                status,
                client_body,
                ..
            } => {
                // 绝不向终端用户泄露上游 401/403(渠道凭证失败):客户端收
                // 通用 502 upstream_error(与旧实现逐字一致)。
                if *status == 401 || *status == 403 {
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
                (self.status_code(), Json(client_body.clone())).into_response()
            }
            GatewayError::Transport { client_body } | GatewayError::BadBody { client_body, .. } => {
                (self.status_code(), Json(client_body.clone())).into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const KEY: &str = "sk-live-secret-0123456789";

    fn secrets() -> Vec<String> {
        vec![KEY.to_string()]
    }

    /// 9.4:失败分类口径。
    #[test]
    fn failure_kind_matches_status() {
        assert_eq!(
            GatewayError::upstream(json!({}), 500, Protocol::OpenAi, false, &[]).failure_kind(),
            FailureKind::Retryable
        );
        assert_eq!(
            GatewayError::upstream(json!({}), 429, Protocol::OpenAi, false, &[]).failure_kind(),
            FailureKind::Retryable
        );
        assert_eq!(
            GatewayError::upstream(json!({}), 400, Protocol::OpenAi, false, &[]).failure_kind(),
            FailureKind::Fatal
        );
        assert_eq!(
            GatewayError::upstream(json!({}), 401, Protocol::OpenAi, false, &[]).failure_kind(),
            FailureKind::CredentialDead
        );
        assert_eq!(
            GatewayError::upstream(json!({}), 403, Protocol::OpenAi, false, &[]).failure_kind(),
            FailureKind::CredentialDead
        );
        assert_eq!(
            GatewayError::transport().failure_kind(),
            FailureKind::Retryable
        );
        assert_eq!(
            GatewayError::KeyLimit("rpm".into()).failure_kind(),
            FailureKind::Fatal
        );
    }

    /// 9.8:上游 401 消息回显渠道 key 时,客户端响应与日志文案都无明文。
    #[tokio::test]
    async fn upstream_error_never_leaks_key() {
        let body = json!({"error": {"message": format!("bad key {KEY}"), "type": "auth"}});
        let e = GatewayError::upstream(body, 401, Protocol::OpenAi, false, &secrets());
        // 401 穷尽后对客户端掩码为 502 通用文案
        let resp = e.clone().into_response(Protocol::OpenAi);
        assert_eq!(resp.status(), 502);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(KEY), "leaked in client body: {text}");
        // 日志文案同样无明文
        assert!(!e.log_message().unwrap().contains(KEY));
    }

    /// 9.8:非 401/403 的上游错误消息(500 等)转发给客户端前也脱敏。
    #[tokio::test]
    async fn upstream_500_body_redacted_for_client() {
        let body =
            json!({"error": {"message": format!("upstream saw key {KEY}"), "type": "server"}});
        let e = GatewayError::upstream(body, 500, Protocol::OpenAi, false, &secrets());
        let resp = e.into_response(Protocol::OpenAi);
        assert_eq!(resp.status(), 500);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(KEY), "leaked: {text}");
        assert!(text.contains("***"));
    }

    /// 9.8:透传模式原样回传上游错误体,但仍过脱敏。
    #[tokio::test]
    async fn passthrough_error_body_redacted_but_raw() {
        let body =
            json!({"error": {"message": format!("bad key {KEY}"), "type": "auth", "extra": KEY}});
        let e = GatewayError::upstream(body, 400, Protocol::OpenAi, true, &secrets());
        let resp = e.into_response(Protocol::OpenAi);
        assert_eq!(resp.status(), 400);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(KEY), "leaked: {text}");
        assert!(text.contains("extra"), "透传保留上游错误体的额外字段");
    }

    /// 透传的 400 错误体保持原样形状(不转协议信封)。
    #[tokio::test]
    async fn passthrough_fatal_error_shape_unchanged() {
        let body = json!({"error": {"message": "input too long", "type": "invalid_request_error", "code": 400}});
        let e = GatewayError::upstream(body, 400, Protocol::OpenAi, true, &[]);
        let resp = e.into_response(Protocol::OpenAi);
        assert_eq!(resp.status(), 400);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], json!(400), "透传保留上游错误体原字段");
    }

    /// 日志状态:上游错误落库沿用上游状态码;准入类按响应状态。
    #[test]
    fn log_status_matches_upstream_or_response() {
        assert_eq!(
            GatewayError::upstream(json!({}), 404, Protocol::OpenAi, false, &[]).log_status(),
            404
        );
        assert_eq!(
            GatewayError::upstream(json!({}), 401, Protocol::OpenAi, false, &[]).log_status(),
            401
        );
        assert_eq!(GatewayError::InsufficientBalance.log_status(), 402);
        assert_eq!(GatewayError::transport().log_status(), 502);
    }

    #[tokio::test]
    async fn admission_errors_use_protocol_shapes() {
        let resp = GatewayError::MissingModel.into_response(Protocol::Anthropic);
        assert_eq!(resp.status(), 400);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], json!("error"), "anthropic 形状");
    }
}
