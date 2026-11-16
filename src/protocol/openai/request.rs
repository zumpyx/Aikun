//! OpenAI 请求 → 其他协议(OpenAI → Anthropic)。


use serde_json::{json, Map, Value};

#[allow(unused_imports)]
use crate::protocol::*;

pub(crate) fn openai_content_to_anthropic_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(s) => {
            if s.is_empty() {
                vec![]
            } else {
                vec![json!({"type": "text", "text": s})]
            }
        }
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p["type"].as_str() {
                Some("text") => Some(with_cache_control(
                    json!({"type": "text", "text": p["text"].as_str().unwrap_or("")}),
                    p,
                )),
                Some("image_url") => {
                    let url = p["image_url"]["url"]
                        .as_str()
                        .or_else(|| p["image_url"].as_str())
                        .unwrap_or("");
                    Some(with_cache_control(openai_image_to_anthropic(url), p))
                }
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

pub(crate) fn openai_image_to_anthropic(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(',') {
            let media_type = meta.trim_end_matches(";base64");
            return json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": data}
            });
        }
    }
    json!({"type": "image", "source": {"type": "url", "url": url}})
}

pub(crate) fn openai_req_to_anthropic(body: &Value) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), body["model"].clone());

    let max_tokens = body["max_tokens"]
        .as_i64()
        .or_else(|| body["max_completion_tokens"].as_i64())
        .unwrap_or(4096);
    out.insert("max_tokens".into(), json!(max_tokens));

    if body["stream"].as_bool().unwrap_or(false) {
        out.insert("stream".into(), json!(true));
    }
    for key in ["temperature", "top_p"] {
        if !body[key].is_null() {
            out.insert(key.into(), body[key].clone());
        }
    }
    // 请求体里已经带了 thinking 对象就原样转发。不从 reasoning_effort
    // 去编 budget_tokens:两边的思考预算不是同一个字段。
    if body["thinking"].is_object() {
        out.insert("thinking".into(), body["thinking"].clone());
    }

    // stop → stop_sequences
    match &body["stop"] {
        Value::String(s) => {
            out.insert("stop_sequences".into(), json!([s]));
        }
        Value::Array(a) => {
            out.insert("stop_sequences".into(), json!(a));
        }
        _ => {}
    }

    // tools
    if let Some(tools) = body["tools"].as_array() {
        let converted: Vec<Value> = tools
            .iter()
            .map(|t| {
                let f = &t["function"];
                let schema = if f["parameters"].is_null() {
                    json!({"type": "object", "properties": {}})
                } else {
                    f["parameters"].clone()
                };
                json!({
                    "name": f["name"],
                    "description": f["description"],
                    "input_schema": schema,
                })
            })
            .collect();
        out.insert("tools".into(), json!(converted));
    }

    // tool_choice
    match &body["tool_choice"] {
        Value::String(s) => {
            let mapped = match s.as_str() {
                "auto" => Some(json!({"type": "auto"})),
                "none" => Some(json!({"type": "none"})),
                "required" => Some(json!({"type": "any"})),
                _ => None,
            };
            if let Some(m) = mapped {
                out.insert("tool_choice".into(), m);
            }
        }
        Value::Object(_) => {
            if body["tool_choice"]["type"].as_str() == Some("function") {
                out.insert(
                    "tool_choice".into(),
                    json!({"type": "tool", "name": body["tool_choice"]["function"]["name"]}),
                );
            }
        }
        _ => {}
    }

    // messages
    // 没有 cache_control 时 system 仍拼成一个字符串(与旧行为一致)。
    // 任一块带了 cache_control 才改成 Anthropic 的 text block 数组,否则
    // 标记会在拼接时丢掉。
    enum SystemPiece {
        Text(String),
        Blocks(Vec<Value>),
    }
    let mut system_pieces: Vec<SystemPiece> = Vec::new();
    let mut system_has_cache = false;
    let mut messages: Vec<Value> = Vec::new();

    let mut push_blocks = |role: &str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        // Anthropic requires strictly alternating roles: merge consecutive
        // same-role messages by concatenating content blocks.
        if let Some(last) = messages.last_mut() {
            if last["role"].as_str() == Some(role) {
                if let Some(arr) = last["content"].as_array_mut() {
                    arr.extend(blocks);
                    return;
                }
            }
        }
        messages.push(json!({"role": role, "content": blocks}));
    };

    if let Some(msgs) = body["messages"].as_array() {
        for msg in msgs {
            match msg["role"].as_str().unwrap_or("") {
                "system" | "developer" => {
                    if let Some(parts) = msg["content"].as_array() {
                        if parts.iter().any(|p| cache_control_of(p).is_some()) {
                            system_has_cache = true;
                            let blocks: Vec<Value> = parts
                                .iter()
                                .filter(|p| p["type"].as_str() == Some("text"))
                                .filter_map(|p| {
                                    let text = p["text"].as_str().unwrap_or("");
                                    if text.is_empty() && cache_control_of(p).is_none() {
                                        return None;
                                    }
                                    Some(with_cache_control(
                                        json!({"type": "text", "text": text}),
                                        p,
                                    ))
                                })
                                .collect();
                            if !blocks.is_empty() {
                                system_pieces.push(SystemPiece::Blocks(blocks));
                            }
                        } else {
                            let text = openai_content_text(&msg["content"]);
                            if !text.is_empty() {
                                system_pieces.push(SystemPiece::Text(text));
                            }
                        }
                    } else {
                        let text = openai_content_text(&msg["content"]);
                        if !text.is_empty() {
                            system_pieces.push(SystemPiece::Text(text));
                        }
                    }
                }
                "user" => {
                    let blocks = openai_content_to_anthropic_blocks(&msg["content"]);
                    push_blocks("user", blocks);
                }
                "assistant" => {
                    let mut blocks = Vec::new();
                    if let Some(reasoning) = msg["reasoning_content"].as_str() {
                        if !reasoning.is_empty() {
                            let mut thinking = json!({"type": "thinking", "thinking": reasoning});
                            if let Some(sig) = msg["reasoning_signature"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                            {
                                thinking["signature"] = json!(sig);
                            }
                            blocks.push(thinking);
                        }
                    }
                    blocks.extend(openai_content_to_anthropic_blocks(&msg["content"]));
                    if let Some(tcs) = msg["tool_calls"].as_array() {
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
                    push_blocks("assistant", blocks);
                }
                "tool" => {
                    let text = openai_content_text(&msg["content"]);
                    push_blocks(
                        "user",
                        vec![json!({
                            "type": "tool_result",
                            "tool_use_id": msg["tool_call_id"],
                            "content": text,
                        })],
                    );
                }
                _ => {}
            }
        }
    }

    if system_has_cache {
        let mut blocks = Vec::new();
        for piece in system_pieces {
            match piece {
                SystemPiece::Text(text) => {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                SystemPiece::Blocks(more) => blocks.extend(more),
            }
        }
        if !blocks.is_empty() {
            out.insert("system".into(), json!(blocks));
        }
    } else {
        let texts: Vec<String> = system_pieces
            .into_iter()
            .filter_map(|piece| match piece {
                SystemPiece::Text(text) => Some(text),
                SystemPiece::Blocks(_) => None,
            })
            .collect();
        if !texts.is_empty() {
            out.insert("system".into(), json!(texts.join("\n")));
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
    use crate::protocol::test_util::{out_types, sse, sse_typed};
    #[test]
    fn request_openai_to_anthropic_basic() {
        let body = json!({
            "model": "claude-3",
            "messages": [
                {"role": "system", "content": "be nice"},
                {"role": "user", "content": "hello"}
            ]
        });
        let out = convert_request(&body, Protocol::OpenAi, Protocol::Anthropic);
        assert_eq!(out["model"], "claude-3");
        assert_eq!(out["max_tokens"], 4096); // default
        assert_eq!(out["system"], "be nice");
        assert_eq!(out["messages"], json!([{"role": "user", "content": [{"type": "text", "text": "hello"}]}]));
        assert!(!out.as_object().unwrap().contains_key("stream"));
    }

    #[test]
    fn request_openai_to_anthropic_merges_consecutive_roles() {
        let body = json!({
            "model": "m",
            "max_completion_tokens": 100,
            "stream": true,
            "messages": [
                {"role": "user", "content": "a"},
                {"role": "user", "content": "b"},
                {"role": "assistant", "content": "c"}
            ]
        });
        let out = convert_request(&body, Protocol::OpenAi, Protocol::Anthropic);
        assert_eq!(out["max_tokens"], 100); // max_completion_tokens honored
        assert_eq!(out["stream"], true);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["content"].as_array().unwrap().len(), 2); // merged user blocks
    }

    #[test]
    fn request_openai_to_anthropic_tools_and_stop() {
        let body = json!({
            "model": "m",
            "stop": "END",
            "messages": [{"role": "user", "content": "x"}],
            "tools": [{"type": "function", "function": {"name": "f", "description": "d", "parameters": {"type": "object"}}}],
            "tool_choice": "required"
        });
        let out = convert_request(&body, Protocol::OpenAi, Protocol::Anthropic);
        assert_eq!(out["stop_sequences"], json!(["END"]));
        assert_eq!(out["tools"][0]["name"], "f");
        assert_eq!(out["tools"][0]["input_schema"], json!({"type": "object"}));
        assert_eq!(out["tool_choice"], json!({"type": "any"}));
    }

    #[test]
    fn request_openai_to_anthropic_keeps_reasoning_and_cache_control() {
        let body = json!({
            "model": "m",
            "thinking": {"type": "enabled"},
            "messages": [
                {"role": "system", "content": [
                    {"type": "text", "text": "rules", "cache_control": {"type": "ephemeral"}}
                ]},
                {"role": "user", "content": [
                    {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
                ]},
                {"role": "assistant", "content": "answer", "reasoning_content": "because", "reasoning_signature": "sig"}
            ]
        });
        let out = convert_request(&body, Protocol::OpenAi, Protocol::Anthropic);
        assert_eq!(out["thinking"], json!({"type": "enabled"}));
        assert_eq!(out["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(out["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(out["messages"][1]["content"][0]["type"], "thinking");
        assert_eq!(out["messages"][1]["content"][0]["thinking"], "because");
        assert_eq!(out["messages"][1]["content"][0]["signature"], "sig");
        assert_eq!(out["messages"][1]["content"][1]["text"], "answer");
        // 不从 reasoning_effort 编造 budget_tokens
        assert!(out.get("budget_tokens").is_none());
    }
}
