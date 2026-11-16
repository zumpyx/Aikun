//! OpenAI Responses 请求 → OpenAI Chat(枢纽)。


use serde_json::{json, Map, Value};

#[allow(unused_imports)]
use crate::protocol::*;

/// Convert an OpenAI Responses API request body to a chat/completions request.
///
/// 映射不上的 responses 专有字段(store/include/reasoning/previous_response_id
/// /background/truncation 与 web_search 等内置工具)在此丢弃;需要这些特性的
/// 客户端应走声明了 responses 协议的渠道透传。
pub(crate) fn responses_req_to_openai(body: &Value) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), body["model"].clone());

    let stream = body["stream"].as_bool().unwrap_or(false);
    if stream {
        out.insert("stream".into(), json!(true));
        // 与 openai 直通同口径:强制上游在结尾 chunk 上报 usage,防免费白嫖。
        out.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if let Some(mt) = body["max_output_tokens"].as_i64() {
        out.insert("max_completion_tokens".into(), json!(mt));
    }
    for key in ["temperature", "top_p", "parallel_tool_calls"] {
        if !body[key].is_null() {
            out.insert(key.into(), body[key].clone());
        }
    }

    // tools:Responses 的 function 工具是扁平定义,包回 chat 的嵌套形状;
    // 内置工具(web_search 等)无 chat 对应物,丢弃。
    if let Some(tools) = body["tools"].as_array() {
        let converted: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                if t["type"].as_str() != Some("function") {
                    return None;
                }
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": t["name"],
                        "description": t["description"],
                        "parameters": if t["parameters"].is_null() {
                            json!({"type": "object", "properties": {}})
                        } else {
                            t["parameters"].clone()
                        },
                    }
                }))
            })
            .collect();
        if !converted.is_empty() {
            out.insert("tools".into(), json!(converted));
        }
    }

    // tool_choice:字符串取值(auto/none/required)两协议一致;对象形状包回嵌套。
    match &body["tool_choice"] {
        Value::String(s) => {
            out.insert("tool_choice".into(), json!(s));
        }
        Value::Object(o) if o["type"].as_str() == Some("function") => {
            out.insert(
                "tool_choice".into(),
                json!({"type": "function", "function": {"name": o["name"]}}),
            );
        }
        _ => {}
    }

    let mut messages: Vec<Value> = Vec::new();
    if let Some(instr) = body["instructions"].as_str().filter(|i| !i.is_empty()) {
        messages.push(json!({"role": "system", "content": instr}));
    }
    match &body["input"] {
        Value::String(s) => {
            if !s.is_empty() {
                messages.push(json!({"role": "user", "content": s}));
            }
        }
        Value::Array(items) => {
            for item in items {
                responses_input_item_to_openai(item, &mut messages);
            }
        }
        _ => {}
    }
    out.insert("messages".into(), json!(messages));

    Value::Object(out)
}

/// Append one Responses input item to the chat messages list.
pub(crate) fn responses_input_item_to_openai(item: &Value, messages: &mut Vec<Value>) {
    // 省略 type 的 {role, content} 是 Responses 的简易消息写法,按 message 处理。
    let item_type = item["type"].as_str().unwrap_or("message");
    match item_type {
        "message" => {
            let role = item["role"].as_str().unwrap_or("user");
            let parts = responses_content_to_openai_parts(&item["content"]);
            // 单个文本块折叠为纯字符串,与 anthropic_req_to_openai 同一风格。
            if parts.len() == 1 && parts[0]["type"].as_str() == Some("text") {
                messages.push(json!({"role": role, "content": parts[0]["text"].clone()}));
            } else if !parts.is_empty() {
                messages.push(json!({"role": role, "content": parts}));
            }
        }
        "function_call" => {
            // call_id 是 Responses 的工具调用关联 id,映射为 chat 的 tool_call id。
            let id = item["call_id"]
                .as_str()
                .or_else(|| item["id"].as_str())
                .unwrap_or("");
            messages.push(json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": item["name"].as_str().unwrap_or(""),
                        "arguments": item["arguments"].as_str().unwrap_or(""),
                    }
                }]
            }));
        }
        "function_call_output" => {
            let content = match &item["output"] {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| p["text"].as_str().or_else(|| p["output_text"].as_str()))
                    .collect::<Vec<_>>()
                    .join(""),
                other => other.to_string(),
            };
            messages.push(json!({
                "role": "tool",
                "tool_call_id": item["call_id"].as_str().unwrap_or(""),
                "content": content,
            }));
        }
        // reasoning / item_reference 等无 chat 对应物,丢弃。
        _ => {}
    }
}

/// Convert Responses message content (string or input/output parts) to chat
/// content parts.
pub(crate) fn responses_content_to_openai_parts(content: &Value) -> Vec<Value> {
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
                Some("input_text") | Some("output_text") => {
                    Some(json!({"type": "text", "text": p["text"].as_str().unwrap_or("")}))
                }
                Some("input_image") => {
                    let url = p["image_url"].as_str().unwrap_or("");
                    Some(json!({"type": "image_url", "image_url": {"url": url}}))
                }
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}


#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::protocol::test_util::{out_types, sse, sse_typed};
    #[test]
    fn request_responses_to_openai_full() {
        let body = json!({
            "model": "gpt-5",
            "stream": true,
            "instructions": "be terse",
            "max_output_tokens": 128,
            "temperature": 0.5,
            "store": false,
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "look"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AA=="}
                ]},
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{\"a\":1}"},
                {"type": "function_call_output", "call_id": "c1", "output": "done"},
                {"type": "reasoning", "summary": []},
                {"role": "user", "content": "plain easy message"}
            ],
            "tools": [
                {"type": "function", "name": "f", "description": "d", "parameters": {"type": "object"}},
                {"type": "web_search"}
            ],
            "tool_choice": "auto"
        });
        let out = convert_request(&body, "responses", "openai");
        assert_eq!(out["model"], "gpt-5");
        assert_eq!(out["stream"], true);
        assert_eq!(out["stream_options"], json!({"include_usage": true}));
        assert_eq!(out["max_completion_tokens"], 128);
        assert_eq!(out["temperature"], 0.5);
        assert!(out["store"].is_null()); // responses 专有字段不带过去

        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0], json!({"role": "system", "content": "be terse"}));
        // 多模态 message → parts 数组
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"][1]["type"], "image_url");
        // function_call → assistant tool_calls,call_id 成为关联 id
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "c1");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "f");
        // function_call_output → tool 消息
        assert_eq!(msgs[3], json!({"role": "tool", "tool_call_id": "c1", "content": "done"}));
        // reasoning 丢弃;省略 type 的简易消息按 message 处理
        assert_eq!(msgs[4], json!({"role": "user", "content": "plain easy message"}));
        assert_eq!(msgs.len(), 5);

        // 内置工具 web_search 丢弃,function 工具包回嵌套形状
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "f");
        assert_eq!(out["tool_choice"], "auto");
    }

    #[test]
    fn request_responses_string_input_and_tool_choice_object() {
        let body = json!({
            "model": "m",
            "input": "hello",
            "tool_choice": {"type": "function", "name": "f"}
        });
        let out = convert_request(&body, "responses", "openai");
        assert_eq!(out["messages"], json!([{"role": "user", "content": "hello"}]));
        assert_eq!(out["tool_choice"], json!({"type": "function", "function": {"name": "f"}}));
        // 非流式不带 stream_options
        assert!(out["stream_options"].is_null());
    }

    #[test]
    fn request_responses_to_anthropic_composition() {
        let body = json!({
            "model": "claude-4",
            "instructions": "sys",
            "max_output_tokens": 64,
            "input": [{"role": "user", "content": "hi"}]
        });
        let out = convert_request(&body, "responses", "anthropic");
        assert_eq!(out["model"], "claude-4");
        assert_eq!(out["max_tokens"], 64);
        assert_eq!(out["system"], "sys");
        assert_eq!(
            out["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}])
        );
    }
}
