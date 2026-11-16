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
pub(crate) use openai::*;
pub(crate) use openai::request::*;
pub(crate) use openai::response::*;
pub(crate) use openai::stream::*;
pub(crate) use responses::request::*;
pub(crate) use sse::*;
pub(crate) use stream::*;
pub(crate) use usage::*;

#[allow(unused_imports)]
use std::collections::HashMap;

use serde_json::{json, Value};

pub const PROTOCOL_OPENAI: &str = "openai";

pub const PROTOCOL_ANTHROPIC: &str = "anthropic";

pub const PROTOCOL_RESPONSES: &str = "responses";

/// Map a provider_type to the wire protocol it speaks.
pub fn provider_protocol(provider_type: &str) -> &'static str {
    match provider_type {
        "anthropic" => PROTOCOL_ANTHROPIC,
        _ => PROTOCOL_OPENAI, // openai, azure, custom all use OpenAI format
    }
}

/// Convert a request body from one protocol to another. Identity if same.
pub fn convert_request(body: &Value, from: &str, to: &str) -> Value {
    if from == to {
        // OpenAI→OpenAI pass-through: ask the upstream to report token usage
        // in the final SSE chunk so the gateway can log it. include_usage is
        // forced on even when the client explicitly disabled it — otherwise a
        // client could stream for free. Other stream_options keys are kept.
        if from == PROTOCOL_OPENAI && body["stream"].as_bool().unwrap_or(false) {
            let mut b = body.clone();
            if let Some(opts) = b["stream_options"].as_object_mut() {
                opts.insert("include_usage".into(), json!(true));
            } else {
                b["stream_options"] = json!({"include_usage": true});
            }
            return b;
        }
        return body.clone();
    }
    match (from, to) {
        (PROTOCOL_OPENAI, PROTOCOL_ANTHROPIC) => openai_req_to_anthropic(body),
        (PROTOCOL_ANTHROPIC, PROTOCOL_OPENAI) => anthropic_req_to_openai(body),
        (PROTOCOL_RESPONSES, PROTOCOL_OPENAI) => responses_req_to_openai(body),
        // responses→anthropic 无直译,经 OpenAI chat 形状中转组合。
        (PROTOCOL_RESPONSES, PROTOCOL_ANTHROPIC) => {
            openai_req_to_anthropic(&responses_req_to_openai(body))
        }
        _ => body.clone(),
    }
}

/// Convert a non-streaming response body from one protocol to another.
pub fn convert_response(resp: &Value, from: &str, to: &str, model: &str) -> Value {
    let mut out = if from == to {
        resp.clone()
    } else {
        match (from, to) {
            (PROTOCOL_OPENAI, PROTOCOL_ANTHROPIC) => openai_resp_to_anthropic(resp),
            (PROTOCOL_ANTHROPIC, PROTOCOL_OPENAI) => anthropic_resp_to_openai(resp, model),
            (PROTOCOL_OPENAI, PROTOCOL_RESPONSES) => openai_resp_to_responses(resp),
            // anthropic→responses 经 OpenAI chat 形状中转组合。
            (PROTOCOL_ANTHROPIC, PROTOCOL_RESPONSES) => {
                openai_resp_to_responses(&anthropic_resp_to_openai(resp, model))
            }
            _ => resp.clone(),
        }
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
pub fn valid_response_shape(resp: &Value, protocol: &str) -> bool {
    match protocol {
        PROTOCOL_OPENAI => resp["choices"]
            .as_array()
            .and_then(|c| c.first())
            .is_some_and(|c| c["message"].is_object()),
        PROTOCOL_ANTHROPIC => resp["content"].is_array(),
        // 第三方中转可能省略 status/object 之一,output 数组 + 任一标识即可。
        PROTOCOL_RESPONSES => {
            resp["output"].is_array()
                && (resp["status"].is_string() || resp["object"].as_str() == Some("response"))
        }
        _ => true,
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
        assert!(valid_response_shape(&openai_ok, PROTOCOL_OPENAI));
        assert!(valid_response_shape(&anthropic_ok, PROTOCOL_ANTHROPIC));
        assert!(!valid_response_shape(&error_body, PROTOCOL_OPENAI));
        assert!(!valid_response_shape(&error_body, PROTOCOL_ANTHROPIC));
        // Cross-shaped bodies are rejected too (misconfigured upstream).
        assert!(!valid_response_shape(&anthropic_ok, PROTOCOL_OPENAI));
        assert!(!valid_response_shape(&openai_ok, PROTOCOL_ANTHROPIC));
        assert!(valid_response_shape(&error_body, "unknown-protocol"));
    }

    // ---- provider_protocol ----

    #[test]
    fn protocol_mapping() {
        assert_eq!(provider_protocol("anthropic"), PROTOCOL_ANTHROPIC);
        assert_eq!(provider_protocol("openai"), PROTOCOL_OPENAI);
        assert_eq!(provider_protocol("azure"), PROTOCOL_OPENAI);
        assert_eq!(provider_protocol("custom"), PROTOCOL_OPENAI);
        assert_eq!(provider_protocol("whatever"), PROTOCOL_OPENAI);
    }

    // ---- convert_request ----

    #[test]
    fn request_same_protocol_passthrough() {
        let body = json!({"model": "gpt-4", "messages": []});
        assert_eq!(convert_request(&body, "openai", "openai"), body);
        assert_eq!(convert_request(&body, "anthropic", "anthropic"), body);
    }

    #[test]
    fn request_openai_stream_injects_usage_option() {
        let body = json!({"model": "gpt-4", "stream": true, "messages": []});
        let out = convert_request(&body, "openai", "openai");
        assert_eq!(out["stream_options"], json!({"include_usage": true}));
        // Non-streaming requests stay untouched.
        let plain = json!({"model": "gpt-4", "messages": []});
        assert!(convert_request(&plain, "openai", "openai")["stream_options"].is_null());
        // Other stream_options keys are kept, but include_usage is always
        // forced on — an explicit client-side `false` must not disable
        // usage reporting (that would let streams go unbilled).
        let preset = json!({"model": "gpt-4", "stream": true, "stream_options": {"x": 1}});
        assert_eq!(
            convert_request(&preset, "openai", "openai")["stream_options"],
            json!({"x": 1, "include_usage": true})
        );
        let disabled = json!({"model": "gpt-4", "stream": true, "stream_options": {"include_usage": false}});
        assert_eq!(
            convert_request(&disabled, "openai", "openai")["stream_options"],
            json!({"include_usage": true})
        );
    }

    #[test]
    fn response_same_protocol_passthrough() {
        let resp = json!({"anything": true});
        assert_eq!(convert_response(&resp, "openai", "openai", "m"), resp);
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
        let out = convert_response(&upstream, "openai", "openai", "gpt-4");
        assert_eq!(out["model"], "gpt-4");
        // OpenAI → Anthropic
        let out = convert_response(&upstream, "openai", "anthropic", "gpt-4");
        assert_eq!(out["model"], "gpt-4");
        // Anthropic → OpenAI
        let upstream_an = json!({
            "id": "msg_1", "model": "claude-real",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });
        let out = convert_response(&upstream_an, "anthropic", "openai", "claude-alias");
        assert_eq!(out["model"], "claude-alias");
    }

    // ---- Responses API ----

    #[test]
    fn request_responses_passthrough_untouched() {
        let body = json!({"model": "gpt-5", "stream": true, "input": "hi", "store": false});
        assert_eq!(convert_request(&body, "responses", "responses"), body);
    }

    #[test]
    fn valid_response_shape_responses() {
        let ok = json!({"object": "response", "status": "completed", "output": []});
        let ok_no_status = json!({"object": "response", "output": []});
        let err = json!({"error": {"message": "boom"}});
        assert!(valid_response_shape(&ok, PROTOCOL_RESPONSES));
        assert!(valid_response_shape(&ok_no_status, PROTOCOL_RESPONSES));
        assert!(!valid_response_shape(&err, PROTOCOL_RESPONSES));
        // openai chat 形状不算 responses 形状
        let chat = json!({"choices": [{"message": {"role": "assistant"}}]});
        assert!(!valid_response_shape(&chat, PROTOCOL_RESPONSES));
    }
}
