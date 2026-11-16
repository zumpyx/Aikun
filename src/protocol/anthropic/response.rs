//! Anthropic 响应 → OpenAI(枢纽),以及 Anthropic 错误类型映射。


use serde_json::{json, Map, Value};

#[allow(unused_imports)]
use crate::protocol::*;

pub(crate) fn anthropic_stop_to_openai(stop_reason: &str) -> &'static str {
    match stop_reason {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        _ => "stop", // end_turn, stop_sequence, ...
    }
}

/// 上游错误类型 → Anthropic 标准错误类型;白名单外的类型统一映射为
/// api_error,避免把 OpenAI 的 server_error 等非标准类型透传给客户端。
pub(crate) fn anthropic_error_type(err_type: &str) -> &'static str {
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

pub(crate) fn anthropic_resp_to_openai(resp: &Value, model: &str) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    if let Some(blocks) = resp["content"].as_array() {
        for b in blocks {
            match b["type"].as_str() {
                Some("text") => text.push_str(b["text"].as_str().unwrap_or("")),
                Some("thinking") => reasoning.push_str(b["thinking"].as_str().unwrap_or("")),
                Some("tool_use") => tool_calls.push(json!({
                    "id": b["id"],
                    "type": "function",
                    "function": {
                        "name": b["name"],
                        "arguments": serde_json::to_string(&b["input"]).unwrap_or_else(|_| "{}".into()),
                    }
                })),
                _ => {}
            }
        }
    }

    let finish = anthropic_stop_to_openai(resp["stop_reason"].as_str().unwrap_or("end_turn"));
    // Anthropic 的 input_tokens 不含缓存;转成 OpenAI 口径时 prompt_tokens
    // 要含 cache_read/cache_creation(total 同步),cache_read 同时写入
    // prompt_tokens_details.cached_tokens 供下游拆分缓存计费。
    let input = resp["usage"]["input_tokens"].as_i64().unwrap_or(0);
    let cache_read = resp["usage"]["cache_read_input_tokens"].as_i64().unwrap_or(0);
    let cache_creation = resp["usage"]["cache_creation_input_tokens"]
        .as_i64()
        .unwrap_or(0);
    let prompt = input + cache_read + cache_creation;
    let completion = resp["usage"]["output_tokens"].as_i64().unwrap_or(0);

    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message.insert("content".into(), json!(text));
    if !reasoning.is_empty() {
        message.insert("reasoning_content".into(), json!(reasoning));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), json!(tool_calls));
    }

    let resp_model = model; // 回写客户端请求的模型名,不透传上游真实模型名
    json!({
        "id": resp["id"].as_str().unwrap_or("chatcmpl-unknown"),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": resp_model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish,
        }],
        "usage": {
            "prompt_tokens": prompt,
            "prompt_tokens_details": {"cached_tokens": cache_read},
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
        }
    })
}


#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::protocol::test_util::{out_types, sse, sse_typed};
    #[test]
    fn response_anthropic_to_openai_with_tool_use() {
        let resp = json!({
            "id": "msg_1",
            "model": "claude-3",
            "content": [
                {"type": "text", "text": "calling"},
                {"type": "tool_use", "id": "tu1", "name": "f", "input": {"x": 2}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let out = convert_response(&resp, "anthropic", "openai", "claude-3");
        assert_eq!(out["object"], "chat.completion");
        let choice = &out["choices"][0];
        assert_eq!(choice["message"]["content"], "calling");
        assert_eq!(choice["message"]["tool_calls"][0]["function"]["name"], "f");
        assert_eq!(choice["finish_reason"], "tool_calls"); // tool_use → tool_calls
        assert_eq!(out["usage"]["total_tokens"], 15);
    }

    #[test]
    fn response_anthropic_to_responses_composition() {
        let resp = json!({
            "id": "msg_1",
            "model": "claude-4",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 2}
        });
        let out = convert_response(&resp, "anthropic", "responses", "claude-4");
        assert_eq!(out["object"], "response");
        assert_eq!(out["output"][0]["content"][0]["text"], "hello");
        assert_eq!(out["usage"]["input_tokens"], 3);
    }

    #[test]
    fn anthropic_error_type_whitelist() {
        // 标准类型透传,非标准类型(如 OpenAI 的 server_error)映射为 api_error
        assert_eq!(anthropic_error_type("rate_limit_error"), "rate_limit_error");
        assert_eq!(anthropic_error_type("overloaded_error"), "overloaded_error");
        assert_eq!(anthropic_error_type("server_error"), "api_error");
        assert_eq!(anthropic_error_type("insufficient_quota"), "api_error");
    }

    #[test]
    fn response_anthropic_to_openai_maps_cache_tokens() {
        // 非流式:cache_read → prompt_tokens_details.cached_tokens,
        // prompt_tokens/total 含 cache_read/cache_creation(OpenAI 口径)
        let resp = json!({
            "id": "msg_1",
            "model": "claude-4",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 9,
                "output_tokens": 3,
                "cache_creation_input_tokens": 2,
                "cache_read_input_tokens": 5,
            }
        });
        let out = convert_response(&resp, "anthropic", "openai", "claude-4");
        assert_eq!(out["usage"]["prompt_tokens"], 16);
        assert_eq!(out["usage"]["prompt_tokens_details"]["cached_tokens"], 5);
        assert_eq!(out["usage"]["total_tokens"], 19);
        // 组合到 responses 后缓存拆分仍然正确
        let out = convert_response(&resp, "anthropic", "responses", "claude-4");
        assert_eq!(out["usage"]["input_tokens"], 16);
        assert_eq!(out["usage"]["input_tokens_details"]["cached_tokens"], 5);
    }
}
