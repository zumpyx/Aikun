//! 协议层:OpenAI Chat Completions / OpenAI Responses / Anthropic Messages
//! 三种线协议之间的请求、响应与 SSE 流式事件双向转换。
//!
//! 星型枢纽:以 OpenAI chat 形态为中转,跨协议靠组合(如 anthropic→responses
//! = OarStreamState ∘ AnStreamState)。本层只做纯数据变换,不碰网络、数据库与计费。
//!
//! 目录(roadmap-v0.5 阶段 1):`sse` 字节解析、`usage` 用量口径、`stream`
//! 流式调度,`openai/` `anthropic/` `responses/` 按**源协议**存放
//! request / response / stream 转换。

pub mod anthropic;
pub mod error;
pub mod openai;
pub mod responses;
pub mod sse;
pub mod stream;
#[cfg(test)]
pub(crate) mod test_util;
pub mod usage;

pub(crate) use anthropic::request::*;
pub(crate) use anthropic::response::*;
pub(crate) use anthropic::stream::*;
pub(crate) use error::*;
pub(crate) use openai::request::*;
pub(crate) use openai::response::*;
pub(crate) use openai::stream::*;
pub(crate) use openai::*;
pub(crate) use responses::request::*;
pub(crate) use sse::*;
pub(crate) use stream::*;
pub(crate) use usage::*;

use serde_json::{Value, json};

/// 网关支持的三种线协议。所有协议分支都用穷举 `match`,新增协议时编译器
/// 会指出每一处需要补齐的转换(roadmap-v0.5 决策 2)。
///
/// 字符串形态(`"openai"` / `"anthropic"` / `"responses"`)只出现在边界:
/// 数据库列、日志 request_type、管理端 JSON,经 [`Protocol::parse`] /
/// [`Protocol::as_str`] 互转。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// OpenAI Chat Completions(星型枢纽形态)。
    OpenAi,
    /// Anthropic Messages。
    Anthropic,
    /// OpenAI Responses API。
    Responses,
}

impl Protocol {
    /// 全部协议,按管理端展示顺序。
    pub const ALL: [Protocol; 3] = [Protocol::OpenAi, Protocol::Anthropic, Protocol::Responses];

    /// 边界字符串形态(数据库 / 日志 / JSON)。
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::OpenAi => "openai",
            Protocol::Anthropic => "anthropic",
            Protocol::Responses => "responses",
        }
    }

    /// 解析边界字符串;未知值返回 None,由调用方决定如何处理(不静默兜底)。
    pub fn parse(s: &str) -> Option<Protocol> {
        match s {
            "openai" => Some(Protocol::OpenAi),
            "anthropic" => Some(Protocol::Anthropic),
            "responses" => Some(Protocol::Responses),
            _ => None,
        }
    }

    /// provider_type → 上游线协议。provider_type 是渠道类型而非协议:
    /// openai / azure / custom 等都讲 OpenAI 格式,只有 anthropic 例外。
    pub fn for_provider_type(provider_type: &str) -> Protocol {
        if provider_type == "anthropic" {
            Protocol::Anthropic
        } else {
            Protocol::OpenAi
        }
    }
}

impl serde::Serialize for Protocol {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Convert a request body from one protocol to another. Identity if same.
pub fn convert_request(body: &Value, from: Protocol, to: Protocol) -> Value {
    use Protocol::*;
    match (from, to) {
        // OpenAI→OpenAI pass-through: ask the upstream to report token usage
        // in the final SSE chunk so the gateway can log it. include_usage is
        // forced on even when the client explicitly disabled it — otherwise a
        // client could stream for free. Other stream_options keys are kept.
        (OpenAi, OpenAi) => {
            if body["stream"].as_bool().unwrap_or(false) {
                let mut b = body.clone();
                if let Some(opts) = b["stream_options"].as_object_mut() {
                    opts.insert("include_usage".into(), json!(true));
                } else {
                    b["stream_options"] = json!({"include_usage": true});
                }
                return b;
            }
            body.clone()
        }
        (Anthropic, Anthropic) | (Responses, Responses) => body.clone(),
        (OpenAi, Anthropic) => openai_req_to_anthropic(body),
        (Anthropic, OpenAi) => anthropic_req_to_openai(body),
        (Responses, OpenAi) => responses_req_to_openai(body),
        // responses→anthropic 无直译,经 OpenAI chat 形状中转组合。
        (Responses, Anthropic) => openai_req_to_anthropic(&responses_req_to_openai(body)),
        // 上游讲 Responses 仅当客户端也是 Responses(见 models::upstream_protocol_for),
        // 这两种组合不会被路由选中;显式列出以保持穷举,行为与 0.4 相同(原样转发)。
        (OpenAi, Responses) | (Anthropic, Responses) => body.clone(),
    }
}

/// Convert a non-streaming response body from one protocol to another.
pub fn convert_response(resp: &Value, from: Protocol, to: Protocol, model: &str) -> Value {
    use Protocol::*;
    let mut out = match (from, to) {
        (OpenAi, OpenAi) | (Anthropic, Anthropic) | (Responses, Responses) => resp.clone(),
        (OpenAi, Anthropic) => openai_resp_to_anthropic(resp),
        (Anthropic, OpenAi) => anthropic_resp_to_openai(resp, model),
        (OpenAi, Responses) => openai_resp_to_responses(resp),
        // anthropic→responses 经 OpenAI chat 形状中转组合。
        (Anthropic, Responses) => openai_resp_to_responses(&anthropic_resp_to_openai(resp, model)),
        // Responses 上游只服务 Responses 客户端(同上),不会出现;保持穷举。
        (Responses, OpenAi) | (Responses, Anthropic) => resp.clone(),
    };
    // 统一回写客户端请求的模型名(含直通路径):配置了 model_mapping 时
    // 上游返回的是映射后的真实模型名,透传会泄漏渠道拓扑;与流式口径一致。
    if out["model"].is_string() {
        out["model"] = json!(model);
    }
    out
}

/// Check that a 200 response body actually matches the expected protocol
/// shape. Some upstreams return HTTP 200 with an error-shaped or otherwise
/// foreign body; treating those as success would fabricate empty completions.
pub fn valid_response_shape(resp: &Value, protocol: Protocol) -> bool {
    match protocol {
        Protocol::OpenAi => resp["choices"]
            .as_array()
            .and_then(|c| c.first())
            .is_some_and(|c| c["message"].is_object()),
        Protocol::Anthropic => resp["content"].is_array(),
        // 第三方中转可能省略 status/object 之一,output 数组 + 任一标识即可。
        Protocol::Responses => {
            resp["output"].is_array()
                && (resp["status"].is_string() || resp["object"].as_str() == Some("response"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::protocol::test_util::{out_types, sse, sse_typed};

    #[test]
    fn valid_response_shape_checks_protocol_shape() {
        let openai_ok = json!({"choices": [{"message": {"role": "assistant", "content": "hi"}}]});
        let anthropic_ok = json!({"type": "message", "content": [{"type": "text", "text": "hi"}]});
        let error_body = json!({"error": {"message": "boom"}});
        assert!(valid_response_shape(&openai_ok, Protocol::OpenAi));
        assert!(valid_response_shape(&anthropic_ok, Protocol::Anthropic));
        assert!(!valid_response_shape(&error_body, Protocol::OpenAi));
        assert!(!valid_response_shape(&error_body, Protocol::Anthropic));
        // Cross-shaped bodies are rejected too (misconfigured upstream).
        assert!(!valid_response_shape(&anthropic_ok, Protocol::OpenAi));
        assert!(!valid_response_shape(&openai_ok, Protocol::Anthropic));
        // 0.4 的未知协议字符串会静默放行;enum 化后未知协议在边界被拒绝。
        assert_eq!(Protocol::parse("unknown-protocol"), None);
    }

    #[test]
    fn protocol_round_trips_through_boundary_strings() {
        for p in Protocol::ALL {
            assert_eq!(Protocol::parse(p.as_str()), Some(p));
            assert_eq!(p.to_string(), p.as_str());
            assert_eq!(serde_json::to_value(p).unwrap(), json!(p.as_str()));
        }
    }

    // ---- provider_protocol ----

    #[test]
    fn protocol_mapping() {
        assert_eq!(
            Protocol::for_provider_type("anthropic"),
            Protocol::Anthropic
        );
        assert_eq!(Protocol::for_provider_type("openai"), Protocol::OpenAi);
        assert_eq!(Protocol::for_provider_type("azure"), Protocol::OpenAi);
        assert_eq!(Protocol::for_provider_type("custom"), Protocol::OpenAi);
        assert_eq!(Protocol::for_provider_type("whatever"), Protocol::OpenAi);
    }

    // ---- convert_request ----

    #[test]
    fn request_same_protocol_passthrough() {
        let body = json!({"model": "gpt-4", "messages": []});
        assert_eq!(
            convert_request(&body, Protocol::OpenAi, Protocol::OpenAi),
            body
        );
        assert_eq!(
            convert_request(&body, Protocol::Anthropic, Protocol::Anthropic),
            body
        );
    }

    #[test]
    fn request_openai_stream_injects_usage_option() {
        let body = json!({"model": "gpt-4", "stream": true, "messages": []});
        let out = convert_request(&body, Protocol::OpenAi, Protocol::OpenAi);
        assert_eq!(out["stream_options"], json!({"include_usage": true}));
        // Non-streaming requests stay untouched.
        let plain = json!({"model": "gpt-4", "messages": []});
        assert!(
            convert_request(&plain, Protocol::OpenAi, Protocol::OpenAi)["stream_options"].is_null()
        );
        // Other stream_options keys are kept, but include_usage is always
        // forced on — an explicit client-side `false` must not disable
        // usage reporting (that would let streams go unbilled).
        let preset = json!({"model": "gpt-4", "stream": true, "stream_options": {"x": 1}});
        assert_eq!(
            convert_request(&preset, Protocol::OpenAi, Protocol::OpenAi)["stream_options"],
            json!({"x": 1, "include_usage": true})
        );
        let disabled =
            json!({"model": "gpt-4", "stream": true, "stream_options": {"include_usage": false}});
        assert_eq!(
            convert_request(&disabled, Protocol::OpenAi, Protocol::OpenAi)["stream_options"],
            json!({"include_usage": true})
        );
    }

    #[test]
    fn response_same_protocol_passthrough() {
        let resp = json!({"anything": true});
        assert_eq!(
            convert_response(&resp, Protocol::OpenAi, Protocol::OpenAi, "m"),
            resp
        );
    }

    #[test]
    fn response_model_rewritten_to_requested_model() {
        // 配置了 model_mapping 的场景:上游返回映射后的真实模型名,
        // 各协议分支(含直通)统一回写为客户端请求的模型名。
        let upstream = json!({
            "id": "chatcmpl-1",
            "model": "real-gpt-4-2024",
            "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
        });
        // 直通
        let out = convert_response(&upstream, Protocol::OpenAi, Protocol::OpenAi, "gpt-4");
        assert_eq!(out["model"], "gpt-4");
        // OpenAI → Anthropic
        let out = convert_response(&upstream, Protocol::OpenAi, Protocol::Anthropic, "gpt-4");
        assert_eq!(out["model"], "gpt-4");
        // Anthropic → OpenAI
        let upstream_an = json!({
            "id": "msg_1", "model": "claude-real",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });
        let out = convert_response(
            &upstream_an,
            Protocol::Anthropic,
            Protocol::OpenAi,
            "claude-alias",
        );
        assert_eq!(out["model"], "claude-alias");
    }

    // ---- Responses API ----

    #[test]
    fn request_responses_passthrough_untouched() {
        let body = json!({"model": "gpt-5", "stream": true, "input": "hi", "store": false});
        assert_eq!(
            convert_request(&body, Protocol::Responses, Protocol::Responses),
            body
        );
    }

    #[test]
    fn valid_response_shape_responses() {
        let ok = json!({"object": "response", "status": "completed", "output": []});
        let ok_no_status = json!({"object": "response", "output": []});
        let err = json!({"error": {"message": "boom"}});
        assert!(valid_response_shape(&ok, Protocol::Responses));
        assert!(valid_response_shape(&ok_no_status, Protocol::Responses));
        assert!(!valid_response_shape(&err, Protocol::Responses));
        // openai chat 形状不算 responses 形状
        let chat = json!({"choices": [{"message": {"role": "assistant"}}]});
        assert!(!valid_response_shape(&chat, Protocol::Responses));
    }
}
