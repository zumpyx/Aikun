//! Anthropic 请求 → OpenAI(枢纽)。

use serde_json::{Map, Value, json};

#[allow(unused_imports)]
use crate::*;

pub fn anthropic_content_to_openai_parts(blocks: &[Value]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|b| match b["type"].as_str() {
            Some("text") => Some(with_cache_control(
                json!({"type": "text", "text": b["text"].as_str().unwrap_or("")}),
                b,
            )),
            Some("image") => {
                let src = &b["source"];
                let url = match src["type"].as_str() {
                    Some("base64") => format!(
                        "data:{};base64,{}",
                        src["media_type"].as_str().unwrap_or("image/png"),
                        src["data"].as_str().unwrap_or("")
                    ),
                    _ => src["url"].as_str().unwrap_or("").to_string(),
                };
                Some(with_cache_control(
                    json!({"type": "image_url", "image_url": {"url": url}}),
                    b,
                ))
            }
            _ => None,
        })
        .collect()
}

/// 单个纯文本、且没有 cache_control 时折成字符串;有标记就必须留在 part 上。
pub fn collapse_openai_content(parts: Vec<Value>) -> Value {
    if parts.len() == 1
        && parts[0]["type"].as_str() == Some("text")
        && cache_control_of(&parts[0]).is_none()
    {
        parts[0]["text"].clone()
    } else {
        json!(parts)
    }
}

pub fn anthropic_req_to_openai(body: &Value) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), body["model"].clone());

    if !body["max_tokens"].is_null() {
        out.insert("max_tokens".into(), body["max_tokens"].clone());
    }
    let stream = body["stream"].as_bool().unwrap_or(false);
    if stream {
        out.insert("stream".into(), json!(true));
        // Ask the OpenAI upstream to report token usage in the final chunk.
        out.insert("stream_options".into(), json!({"include_usage": true}));
    }
    for key in ["temperature", "top_p"] {
        if !body[key].is_null() {
            out.insert(key.into(), body[key].clone());
        }
    }
    if let Some(stop) = body["stop_sequences"].as_array() {
        out.insert("stop".into(), json!(stop));
    }
    // 已有的 thinking 对象原样带上(DeepSeek 等接受 {"type":"enabled"})。
    // 不把 budget_tokens 翻译成 reasoning_effort。
    if body["thinking"].is_object() {
        out.insert("thinking".into(), body["thinking"].clone());
    }

    // tools
    if let Some(tools) = body["tools"].as_array() {
        let converted: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t["name"],
                        "description": t["description"],
                        "parameters": if t["input_schema"].is_null() { json!({"type": "object", "properties": {}}) } else { t["input_schema"].clone() },
                    }
                })
            })
            .collect();
        out.insert("tools".into(), json!(converted));
    }

    // tool_choice
    match body["tool_choice"]["type"].as_str() {
        Some("auto") => {
            out.insert("tool_choice".into(), json!("auto"));
        }
        Some("none") => {
            out.insert("tool_choice".into(), json!("none"));
        }
        Some("any") => {
            out.insert("tool_choice".into(), json!("required"));
        }
        Some("tool") => {
            out.insert(
                "tool_choice".into(),
                json!({"type": "function", "function": {"name": body["tool_choice"]["name"]}}),
            );
        }
        _ => {}
    }

    let mut messages: Vec<Value> = Vec::new();

    // system → system message
    match &body["system"] {
        Value::String(s) if !s.is_empty() => {
            messages.push(json!({"role": "system", "content": s}));
        }
        Value::Array(blocks) => {
            if blocks.iter().any(|b| cache_control_of(b).is_some()) {
                let parts = anthropic_content_to_openai_parts(blocks);
                if !parts.is_empty() {
                    messages.push(json!({"role": "system", "content": parts}));
                }
            } else {
                let text: String = blocks
                    .iter()
                    .filter(|b| b["type"].as_str() == Some("text"))
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    messages.push(json!({"role": "system", "content": text}));
                }
            }
        }
        _ => {}
    }

    if let Some(msgs) = body["messages"].as_array() {
        for msg in msgs {
            let role = msg["role"].as_str().unwrap_or("user");
            let blocks: Vec<Value> = match &msg["content"] {
                Value::String(s) => vec![json!({"type": "text", "text": s})],
                Value::Array(a) => a.clone(),
                _ => vec![],
            };

            match role {
                "user" => {
                    // Split tool_result blocks into separate tool messages.
                    let (tool_results, regular): (Vec<&Value>, Vec<&Value>) = blocks
                        .iter()
                        .partition(|b| b["type"].as_str() == Some("tool_result"));

                    for tr in tool_results {
                        let content = match &tr["content"] {
                            Value::String(s) => s.clone(),
                            Value::Array(parts) => parts
                                .iter()
                                .filter(|p| p["type"].as_str() == Some("text"))
                                .filter_map(|p| p["text"].as_str())
                                .collect::<Vec<_>>()
                                .join(""),
                            other => other.to_string(),
                        };
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": tr["tool_use_id"],
                            "content": content,
                        }));
                    }

                    let parts = anthropic_content_to_openai_parts(
                        &regular.into_iter().cloned().collect::<Vec<_>>(),
                    );
                    if !parts.is_empty() {
                        messages.push(json!({
                            "role": "user",
                            "content": collapse_openai_content(parts),
                        }));
                    }
                }
                "assistant" => {
                    let text: String = blocks
                        .iter()
                        .filter(|b| b["type"].as_str() == Some("text"))
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("");
                    let reasoning_parts: Vec<&str> = blocks
                        .iter()
                        .filter(|b| b["type"].as_str() == Some("thinking"))
                        .filter_map(|b| b["thinking"].as_str())
                        .filter(|s| !s.is_empty())
                        .collect();
                    let signatures: Vec<&str> = blocks
                        .iter()
                        .filter(|b| b["type"].as_str() == Some("thinking"))
                        .filter_map(|b| b["signature"].as_str())
                        .filter(|s| !s.is_empty())
                        .collect();
                    let tool_calls: Vec<Value> = blocks
                        .iter()
                        .filter(|b| b["type"].as_str() == Some("tool_use"))
                        .map(|b| {
                            json!({
                                "id": b["id"],
                                "type": "function",
                                "function": {
                                    "name": b["name"],
                                    "arguments": serde_json::to_string(&b["input"]).unwrap_or_else(|_| "{}".into()),
                                }
                            })
                        })
                        .collect();
                    let mut m = Map::new();
                    m.insert("role".into(), json!("assistant"));
                    m.insert("content".into(), json!(text));
                    if !reasoning_parts.is_empty() {
                        m.insert("reasoning_content".into(), json!(reasoning_parts.join("")));
                    }
                    // 多块思考各带签名时无法合成一个,宁可不带也不要贴错。
                    if signatures.len() == 1 {
                        m.insert("reasoning_signature".into(), json!(signatures[0]));
                    }
                    if !tool_calls.is_empty() {
                        m.insert("tool_calls".into(), json!(tool_calls));
                    }
                    messages.push(Value::Object(m));
                }
                _ => {}
            }
        }
    }

    out.insert("messages".into(), json!(messages));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::test_util::{out_types, sse, sse_typed};
    #[test]
    fn request_anthropic_to_openai_basic() {
        let body = json!({
            "model": "gpt-4",
            "max_tokens": 50,
            "stream": true,
            "system": "sys",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = convert_request(&body, Protocol::Anthropic, Protocol::OpenAi);
        assert_eq!(out["max_tokens"], 50);
        assert_eq!(out["stream"], true);
        assert_eq!(out["stream_options"], json!({"include_usage": true}));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0], json!({"role": "system", "content": "sys"}));
        assert_eq!(msgs[1], json!({"role": "user", "content": "hi"})); // single text collapses to string
    }

    #[test]
    fn request_anthropic_to_openai_tool_result_split() {
        let body = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "result-text"},
                    {"type": "text", "text": "follow-up"}
                ]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "tool_use", "id": "t2", "name": "f", "input": {"a": 1}}
                ]}
            ]
        });
        let out = convert_request(&body, Protocol::Anthropic, Protocol::OpenAi);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(
            msgs[0],
            json!({"role": "tool", "tool_call_id": "t1", "content": "result-text"})
        );
        assert_eq!(msgs[1], json!({"role": "user", "content": "follow-up"}));
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(
            msgs[2]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":1}"
        );
    }

    #[test]
    fn request_anthropic_to_openai_keeps_thinking_and_cache_control() {
        let body = json!({
            "model": "m",
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "system": [
                {"type": "text", "text": "rules", "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "because", "signature": "sig"},
                    {"type": "text", "text": "answer"}
                ]}
            ]
        });
        let out = convert_request(&body, Protocol::Anthropic, Protocol::OpenAi);
        assert_eq!(out["thinking"]["budget_tokens"], 1024);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(msgs[1]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(msgs[2]["reasoning_content"], "because");
        assert_eq!(msgs[2]["reasoning_signature"], "sig");
        assert_eq!(msgs[2]["content"], "answer");
    }
}
