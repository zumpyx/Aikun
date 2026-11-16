//! 面向客户端的错误形状:按客户端协议生成错误 JSON / 流内错误事件 / 流终止帧。
//! roadmap-v0.5 阶段 1:从 pipeline 归位到协议层(纯数据变换,穷举 match)。

use serde_json::{Value, json};

use crate::protocol::{Protocol, SseOut, anthropic_error_type};

/// Build an error body in the client-facing protocol shape.
pub fn protocol_error(client_protocol: Protocol, err_type: &str, message: &str) -> Value {
    match client_protocol {
        Protocol::Anthropic => json!({
            "type": "error",
            "error": {"type": anthropic_error_type(err_type), "message": message}
        }),
        // OpenAI 形状带 code 字段(与 type 同值),部分 SDK 按 error.code 分支。
        // Responses API 的非流式错误沿用 OpenAI 信封(与 0.4 行为一致)。
        Protocol::OpenAi | Protocol::Responses => json!({
            "error": {"message": message, "type": err_type, "param": null, "code": err_type}
        }),
    }
}

/// Convert an upstream error body to the client-facing protocol shape,
/// preserving the original error message and type.
pub fn convert_error(err_body: &Value, client_protocol: Protocol) -> Value {
    let message = err_body["error"]["message"]
        .as_str()
        .unwrap_or("upstream error");
    let err_type = err_body["error"]["type"]
        .as_str()
        .unwrap_or("upstream_error");
    match client_protocol {
        Protocol::Anthropic => json!({
            "type": "error",
            "error": {"type": anthropic_error_type(err_type), "message": message}
        }),
        Protocol::OpenAi | Protocol::Responses => json!({
            "error": {"message": message, "type": err_type}
        }),
    }
}

/// 流中途失败时发给客户端的协议对应错误事件。
pub fn stream_error_event(client_protocol: Protocol, message: &str) -> SseOut {
    match client_protocol {
        Protocol::Anthropic => SseOut {
            event: Some("error".to_string()),
            data: json!({
                "type": "error",
                "error": {"type": "api_error", "message": message}
            })
            .to_string(),
        },
        // Responses API 的错误事件本身就是终止帧(其后流直接结束)。
        Protocol::Responses => SseOut {
            event: Some("error".to_string()),
            data: json!({
                "type": "error",
                "code": "upstream_error",
                "message": message
            })
            .to_string(),
        },
        Protocol::OpenAi => SseOut {
            event: None,
            data: json!({
                "error": {"message": message, "type": "upstream_error"}
            })
            .to_string(),
        },
    }
}

/// 协议对应的流终止帧:OpenAI 为 `[DONE]`,Anthropic 为 `message_stop`。
/// Responses API 无终止帧(response.completed 或 error 事件后流直接结束),
/// 返回 None。
pub fn stream_terminal_event(client_protocol: Protocol) -> Option<SseOut> {
    match client_protocol {
        Protocol::Anthropic => Some(SseOut {
            event: Some("message_stop".to_string()),
            data: json!({"type": "message_stop"}).to_string(),
        }),
        Protocol::OpenAi => Some(SseOut {
            event: None,
            data: "[DONE]".to_string(),
        }),
        Protocol::Responses => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_shapes_per_protocol() {
        let an = protocol_error(Protocol::Anthropic, "rate_limit_error", "slow");
        assert_eq!(an["type"], "error");
        assert_eq!(an["error"]["type"], "rate_limit_error");
        let oa = protocol_error(Protocol::OpenAi, "insufficient_quota", "pay");
        assert_eq!(oa["error"]["code"], "insufficient_quota");
        assert_eq!(
            protocol_error(Protocol::Responses, "x", "y"),
            protocol_error(Protocol::OpenAi, "x", "y")
        );
    }

    #[test]
    fn terminal_events_per_protocol() {
        assert_eq!(
            stream_terminal_event(Protocol::OpenAi).unwrap().data,
            "[DONE]"
        );
        assert_eq!(
            stream_terminal_event(Protocol::Anthropic)
                .unwrap()
                .event
                .as_deref(),
            Some("message_stop")
        );
        assert!(stream_terminal_event(Protocol::Responses).is_none());
        assert_eq!(
            stream_error_event(Protocol::Responses, "boom")
                .event
                .as_deref(),
            Some("error")
        );
        assert!(stream_error_event(Protocol::OpenAi, "boom").event.is_none());
    }
}
