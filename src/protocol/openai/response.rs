//! OpenAI 响应 → 其他协议(OpenAI → Anthropic / Responses)。


use serde_json::{json, Value};

#[allow(unused_imports)]
use crate::protocol::*;

pub(crate) fn openai_finish_to_anthropic(finish: &str) -> &'static str {
    match finish {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        _ => "end_turn", // stop, content_filter, ...
    }
}

/// OpenAI finish_reason → Responses incomplete_details.reason;
/// None 表示正常完成(completed)。
pub(crate) fn openai_finish_to_incomplete_reason(finish: &str) -> Option<&'static str> {
    match finish {
        "length" => Some("max_output_tokens"),
        "content_filter" => Some("content_filter"),
        _ => None,
    }
}

pub(crate) fn openai_resp_to_anthropic(resp: &Value) -> Value {
    // 上游未返回有效 choices（如错误体）时按上游错误处理，不伪造成功响应
    let choice = match resp["choices"].as_array().and_then(|c| c.first()) {
        Some(c) if c["message"].is_object() => c,
        _ => {
            return json!({
                "type": "error",
                "error": {"type": "upstream_error", "message": "Upstream returned an invalid response"}
            })
        }
    };
    let message = &choice["message"];

    let mut blocks: Vec<Value> = Vec::new();
    if let Some(reasoning) = message["reasoning_content"].as_str() {
        if !reasoning.is_empty() {
            let mut thinking = json!({"type": "thinking", "thinking": reasoning});
            if let Some(sig) = message["reasoning_signature"]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                thinking["signature"] = json!(sig);
            }
            blocks.push(thinking);
        }
    }
    if let Some(text) = message["content"].as_str() {
        if !text.is_empty() {
            blocks.push(json!({"type": "text", "text": text}));
        }
    }
    if let Some(tcs) = message["tool_calls"].as_array() {
        for tc in tcs {
            let input = tc["function"]["arguments"]
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| json!({}));
            blocks.push(json!({
                "type": "tool_use",
                "id": tc["id"],
                "name": tc["function"]["name"],
                "input": input,
            }));
        }
    }

    let stop_reason = openai_finish_to_anthropic(choice["finish_reason"].as_str().unwrap_or("stop"));
    // OpenAI 的 prompt_tokens 含缓存命中部分;Anthropic 的 input_tokens 不含,
    // 缓存命中单列 cache_read_input_tokens。与 anthropic_resp_to_openai 的
    // 合并口径互为镜像(0.4 曾把含缓存的 prompt 整个塞进 input_tokens 并丢掉
    // 缓存维度,见 CHANGELOG 0.4.5 修复 ①)。
    let prompt = resp["usage"]["prompt_tokens"].as_i64().unwrap_or(0);
    let cached = resp["usage"]["prompt_tokens_details"]["cached_tokens"]
        .as_i64()
        .unwrap_or(0)
        .clamp(0, prompt.max(0));
    let completion = resp["usage"]["completion_tokens"].as_i64().unwrap_or(0);
    let mut usage = json!({
        "input_tokens": prompt - cached,
        "output_tokens": completion,
    });
    if cached > 0 {
        usage["cache_read_input_tokens"] = json!(cached);
    }

    json!({
        "id": resp["id"].as_str().unwrap_or("msg_unknown"),
        "type": "message",
        "role": "assistant",
        "model": resp["model"],
        "content": blocks,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage,
    })
}

/// Convert a chat/completions response to a Responses API response object.
pub(crate) fn openai_resp_to_responses(resp: &Value) -> Value {
    let message = &resp["choices"][0]["message"];
    let mut output: Vec<Value> = Vec::new();

    if let Some(text) = message["content"].as_str() {
        if !text.is_empty() {
            output.push(json!({
                "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text, "annotations": []}],
            }));
        }
    }
    if let Some(tcs) = message["tool_calls"].as_array() {
        for tc in tcs {
            output.push(json!({
                "id": format!("fc_{}", uuid::Uuid::new_v4().simple()),
                "type": "function_call",
                "status": "completed",
                "call_id": tc["id"].as_str().unwrap_or(""),
                "name": tc["function"]["name"].as_str().unwrap_or(""),
                "arguments": tc["function"]["arguments"].as_str().unwrap_or(""),
            }));
        }
    }

    // 截断/审核命中的响应不能标 completed,否则客户端当成完整结果使用。
    let finish = resp["choices"][0]["finish_reason"].as_str().unwrap_or("stop");
    let incomplete = openai_finish_to_incomplete_reason(finish);
    let (status, incomplete_details) = match incomplete {
        Some(reason) => ("incomplete", json!({"reason": reason})),
        None => ("completed", Value::Null),
    };

    let u = &resp["usage"];
    let prompt = u["prompt_tokens"].as_i64().unwrap_or(0);
    let cached = u["prompt_tokens_details"]["cached_tokens"].as_i64().unwrap_or(0);
    let completion = u["completion_tokens"].as_i64().unwrap_or(0);
    let total = u["total_tokens"].as_i64().unwrap_or(prompt + completion);

    json!({
        "id": format!("resp_{}", uuid::Uuid::new_v4().simple()),
        "object": "response",
        "created_at": resp["created"].as_i64()
            .unwrap_or_else(|| chrono::Utc::now().timestamp()),
        "status": status,
        "incomplete_details": incomplete_details,
        "model": resp["model"],
        "output": output,
        "usage": {
            "input_tokens": prompt,
            "input_tokens_details": {"cached_tokens": cached},
            "output_tokens": completion,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": total,
        },
    })
}


#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::protocol::test_util::{out_types, sse, sse_typed};
    // ---- convert_response ----

    #[test]
    fn response_openai_to_anthropic() {
        let resp = json!({
            "id": "chatcmpl-1",
            "model": "gpt-4",
            "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "length"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
        });
        let out = convert_response(&resp, Protocol::OpenAi, Protocol::Anthropic, "gpt-4");
        assert_eq!(out["type"], "message");
        assert_eq!(out["content"], json!([{"type": "text", "text": "hi"}]));
        assert_eq!(out["stop_reason"], "max_tokens"); // length → max_tokens
        assert_eq!(out["usage"], json!({"input_tokens": 3, "output_tokens": 4}));
    }

    #[test]
    fn response_openai_to_anthropic_splits_cached_tokens() {
        // 回归(bug ①):prompt_tokens 含缓存命中,Anthropic 客户端应看到
        // input_tokens = 未缓存部分,cache_read_input_tokens = 缓存命中。
        let resp = json!({
            "id": "chatcmpl-1", "model": "gpt-4",
            "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14,
                      "prompt_tokens_details": {"cached_tokens": 6}}
        });
        let out = convert_response(&resp, Protocol::OpenAi, Protocol::Anthropic, "gpt-4");
        assert_eq!(out["usage"]["input_tokens"], 4);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 6);
        assert_eq!(out["usage"]["output_tokens"], 4);
        // 经 Anthropic 形状再提取用量,与直接从 OpenAI 原文提取一致(计费口径不漂移)。
        assert_eq!(extract_usage_any(&out), extract_usage_any(&resp));
        // 上游数据不可信:cached > prompt 时饱和,不出现负数。
        let bad = json!({
            "choices": [{"message": {"role": "assistant", "content": ""}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "prompt_tokens_details": {"cached_tokens": 9}}
        });
        let out = convert_response(&bad, Protocol::OpenAi, Protocol::Anthropic, "m");
        assert_eq!(out["usage"]["input_tokens"], 0);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 3);
    }

    #[test]
    fn response_reasoning_round_trip() {
        let oa = json!({
            "id": "chatcmpl-1",
            "model": "m",
            "choices": [{"message": {
                "role": "assistant",
                "content": "ans",
                "reasoning_content": "think",
                "reasoning_signature": "sig"
            }, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        });
        let an = convert_response(&oa, Protocol::OpenAi, Protocol::Anthropic, "m");
        assert_eq!(an["content"][0]["type"], "thinking");
        assert_eq!(an["content"][0]["thinking"], "think");
        assert_eq!(an["content"][0]["signature"], "sig");
        assert_eq!(an["content"][1]["text"], "ans");

        let back = convert_response(&an, Protocol::Anthropic, Protocol::OpenAi, "m");
        assert_eq!(back["choices"][0]["message"]["reasoning_content"], "think");
        assert_eq!(back["choices"][0]["message"]["content"], "ans");
    }

    #[test]
    fn response_openai_to_anthropic_invalid_choices() {
        // choices 为空或缺 message（如上游错误体）时按上游错误处理
        for bad in [
            json!({"choices": []}),
            json!({"choices": [{}]}),
            json!({"error": {"message": "boom"}}),
        ] {
            let out = convert_response(&bad, Protocol::OpenAi, Protocol::Anthropic, "m");
            assert_eq!(out["type"], "error");
            assert_eq!(out["error"]["type"], "upstream_error");
        }
    }

    #[test]
    fn response_openai_to_responses_with_tool_call() {
        let resp = json!({
            "id": "chatcmpl-1",
            "model": "gpt-5",
            "created": 1_700_000_000,
            "choices": [{"message": {
                "role": "assistant",
                "content": "hi",
                "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{\"a\":1}"}}]
            }, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14,
                      "prompt_tokens_details": {"cached_tokens": 6}}
        });
        let out = convert_response(&resp, Protocol::OpenAi, Protocol::Responses, "gpt-5");
        assert_eq!(out["object"], "response");
        assert_eq!(out["status"], "completed");
        assert_eq!(out["model"], "gpt-5");
        assert_eq!(out["created_at"], 1_700_000_000); // 取 chat 响应的 created
        assert!(out["id"].as_str().unwrap().starts_with("resp_"));
        let output = out["output"].as_array().unwrap();
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0], json!({"type": "output_text", "text": "hi", "annotations": []}));
        assert_eq!(output[1]["type"], "function_call");
        assert_eq!(output[1]["call_id"], "c1");
        assert_eq!(output[1]["arguments"], "{\"a\":1}");
        assert_eq!(out["usage"]["input_tokens"], 10);
        assert_eq!(out["usage"]["input_tokens_details"]["cached_tokens"], 6);
        assert_eq!(out["usage"]["output_tokens"], 4);
    }

    #[test]
    fn response_openai_to_responses_incomplete_on_length() {
        let resp = json!({
            "id": "chatcmpl-1",
            "model": "gpt-5",
            "created": 1_700_000_000,
            "choices": [{"message": {"role": "assistant", "content": "partial"}, "finish_reason": "length"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14}
        });
        let out = convert_response(&resp, Protocol::OpenAi, Protocol::Responses, "gpt-5");
        assert_eq!(out["status"], "incomplete");
        assert_eq!(out["incomplete_details"]["reason"], "max_output_tokens");

        // content_filter 同理;正常 stop 仍是 completed 且 incomplete_details 为 null
        let mut resp_cf = resp.clone();
        resp_cf["choices"][0]["finish_reason"] = json!("content_filter");
        let out = convert_response(&resp_cf, Protocol::OpenAi, Protocol::Responses, "gpt-5");
        assert_eq!(out["status"], "incomplete");
        assert_eq!(out["incomplete_details"]["reason"], "content_filter");

        let mut resp_ok = resp.clone();
        resp_ok["choices"][0]["finish_reason"] = json!("stop");
        let out = convert_response(&resp_ok, Protocol::OpenAi, Protocol::Responses, "gpt-5");
        assert_eq!(out["status"], "completed");
        assert!(out["incomplete_details"].is_null());
    }
}
