//! Protocol conversion between OpenAI and Anthropic API formats.
//!
//! Supports both non-streaming request/response conversion and streaming
//! (SSE) event conversion via stateful converters.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

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

// ============================================================================
// Request conversion (non-stream)
// ============================================================================

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

/// Convert an OpenAI Responses API request body to a chat/completions request.
///
/// 映射不上的 responses 专有字段(store/include/reasoning/previous_response_id
/// /background/truncation 与 web_search 等内置工具)在此丢弃;需要这些特性的
/// 客户端应走声明了 responses 协议的渠道透传。
fn responses_req_to_openai(body: &Value) -> Value {
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
fn responses_input_item_to_openai(item: &Value, messages: &mut Vec<Value>) {
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
fn responses_content_to_openai_parts(content: &Value) -> Vec<Value> {
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

/// Anthropic / 中转站用的 cache_control 是对象。其它形状不转发。
fn cache_control_of(v: &Value) -> Option<&Value> {
    v.get("cache_control").filter(|c| c.is_object())
}

fn with_cache_control(mut block: Value, src: &Value) -> Value {
    if let Some(cc) = cache_control_of(src) {
        if let Some(obj) = block.as_object_mut() {
            obj.insert("cache_control".into(), cc.clone());
        }
    }
    block
}

fn openai_content_to_anthropic_blocks(content: &Value) -> Vec<Value> {
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

fn openai_image_to_anthropic(url: &str) -> Value {
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

/// Extract plain text from an OpenAI message content field (string or parts).
fn openai_content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"].as_str() == Some("text"))
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn openai_req_to_anthropic(body: &Value) -> Value {
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

fn anthropic_content_to_openai_parts(blocks: &[Value]) -> Vec<Value> {
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
fn collapse_openai_content(parts: Vec<Value>) -> Value {
    if parts.len() == 1
        && parts[0]["type"].as_str() == Some("text")
        && cache_control_of(&parts[0]).is_none()
    {
        parts[0]["text"].clone()
    } else {
        json!(parts)
    }
}

fn anthropic_req_to_openai(body: &Value) -> Value {
    let mut out = Map::new();
    out.insert("model".into(), body["model"].clone());

    if !body["max_tokens"].is_null() {
        out.insert("max_tokens".into(), body["max_tokens"].clone());
    }
    let stream = body["stream"].as_bool().unwrap_or(false);
    if stream {
        out.insert("stream".into(), json!(true));
        // Ask the OpenAI upstream to report token usage in the final chunk.
        out.insert(
            "stream_options".into(),
            json!({"include_usage": true}),
        );
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
                        m.insert(
                            "reasoning_content".into(),
                            json!(reasoning_parts.join("")),
                        );
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

// ============================================================================
// Response conversion (non-stream)
// ============================================================================

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

fn anthropic_stop_to_openai(stop_reason: &str) -> &'static str {
    match stop_reason {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        _ => "stop", // end_turn, stop_sequence, ...
    }
}

fn openai_finish_to_anthropic(finish: &str) -> &'static str {
    match finish {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        _ => "end_turn", // stop, content_filter, ...
    }
}

/// OpenAI finish_reason → Responses incomplete_details.reason;
/// None 表示正常完成(completed)。
fn openai_finish_to_incomplete_reason(finish: &str) -> Option<&'static str> {
    match finish {
        "length" => Some("max_output_tokens"),
        "content_filter" => Some("content_filter"),
        _ => None,
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

fn anthropic_resp_to_openai(resp: &Value, model: &str) -> Value {
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

fn openai_resp_to_anthropic(resp: &Value) -> Value {
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
    let prompt = resp["usage"]["prompt_tokens"].as_i64().unwrap_or(0);
    let completion = resp["usage"]["completion_tokens"].as_i64().unwrap_or(0);

    json!({
        "id": resp["id"].as_str().unwrap_or("msg_unknown"),
        "type": "message",
        "role": "assistant",
        "model": resp["model"],
        "content": blocks,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": prompt,
            "output_tokens": completion,
        }
    })
}

/// Convert a chat/completions response to a Responses API response object.
fn openai_resp_to_responses(resp: &Value) -> Value {
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

/// 上游 token 计数不可信:负数按 0,超出 i32 范围的值做饱和转换,
/// 替代裸 `as i32`(后者会回绕成负数,污染计费与统计)。
fn clamp_token_count(v: Option<i64>) -> i32 {
    v.unwrap_or(0).clamp(0, i32::MAX as i64) as i32
}

/// Extract (prompt_uncached, completion, total, cached) token usage from a
/// response in either protocol (OpenAI `prompt_tokens`/`completion_tokens` or
/// Anthropic `input_tokens`/`output_tokens`)。
/// 缓存拆分:OpenAI 的 prompt_tokens 含命中缓存的部分,按
/// prompt_tokens_details.cached_tokens 拆出单独计价;Anthropic 的
/// input_tokens 本就不含缓存,cache_read_input_tokens 记为缓存,
/// cache_creation_input_tokens(写缓存)按输入价并入未缓存输入(近似)。
/// 上游数值不可信:负数按 0 处理,超出 i32 范围的值做饱和转换,避免污染计费与统计。
pub fn extract_usage_any(resp: &Value) -> (i32, i32, i32, i32) {
    let clamp = clamp_token_count;
    let u = &resp["usage"];
    let completion = clamp(
        u["completion_tokens"]
            .as_i64()
            .or_else(|| u["output_tokens"].as_i64()),
    );
    if !u["prompt_tokens"].is_null() {
        // OpenAI:prompt_tokens 含缓存,拆出后不得为负(上游数据不可信)
        let prompt = clamp(u["prompt_tokens"].as_i64());
        let cached = clamp(u["prompt_tokens_details"]["cached_tokens"].as_i64());
        let prompt_uncached = (prompt - cached).max(0);
        let total = u["total_tokens"]
            .as_i64()
            .map(|t| clamp(Some(t)))
            .unwrap_or(prompt.saturating_add(completion));
        (prompt_uncached, completion, total, cached)
    } else if !u["input_tokens"].is_null()
        && (!u["input_tokens_details"].is_null() || !u["total_tokens"].is_null())
    {
        // Responses API:input_tokens 同样含缓存,按 input_tokens_details
        // .cached_tokens 拆出。特征判定先于 Anthropic 分支:Anthropic 的
        // usage 没有 total_tokens/input_tokens_details,不会误入此分支。
        // input_tokens 必须存在:非标 OpenAI 缺 prompt_tokens 但带
        // total_tokens 时,不能凭 total 误入此分支把输入读成 0。
        let prompt = clamp(u["input_tokens"].as_i64());
        let cached = clamp(u["input_tokens_details"]["cached_tokens"].as_i64());
        let prompt_uncached = (prompt - cached).max(0);
        let total = u["total_tokens"]
            .as_i64()
            .map(|t| clamp(Some(t)))
            .unwrap_or(prompt.saturating_add(completion));
        (prompt_uncached, completion, total, cached)
    } else {
        // Anthropic:input_tokens 不含缓存;cache_read 按缓存价,
        // cache_creation 按输入价并入未缓存输入(近似)
        let cached = clamp(u["cache_read_input_tokens"].as_i64());
        let prompt_uncached = clamp(u["input_tokens"].as_i64())
            .saturating_add(clamp(u["cache_creation_input_tokens"].as_i64()));
        let total = prompt_uncached
            .saturating_add(cached)
            .saturating_add(completion);
        (prompt_uncached, completion, total, cached)
    }
}

// ============================================================================
// SSE parsing
// ============================================================================

/// One parsed SSE event (possibly multiple data: lines joined).
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Drain all complete SSE events (terminated by a blank line) from `buf`.
pub fn parse_sse_events(buf: &mut Vec<u8>) -> Vec<SseEvent> {
    let mut events = Vec::new();
    while let Some((idx, len)) = find_event_boundary(buf) {
        let raw: Vec<u8> = buf.drain(..idx + len).collect();
        if let Some(ev) = parse_sse_block(&raw[..idx]) {
            events.push(ev);
        }
    }
    events
}

/// Parse whatever remains in the buffer as a final event (stream ended
/// without a trailing blank line).
pub fn parse_sse_remaining(buf: &mut Vec<u8>) -> Option<SseEvent> {
    if buf.is_empty() {
        return None;
    }
    let raw = std::mem::take(buf);
    parse_sse_block(&raw)
}

fn find_event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        // 少数上游用裸 \r 分隔事件
        if buf[i] == b'\r' && buf[i + 1] == b'\r' {
            return Some((i, 2));
        }
        if i + 3 < buf.len()
            && buf[i] == b'\r'
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

fn parse_sse_block(raw: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(raw);
    let mut event = None;
    let mut data_lines: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("data:") {
            data_lines.push(v.trim_start_matches(' ').to_string());
        }
    }
    if data_lines.is_empty() {
        None
    } else {
        Some(SseEvent {
            event,
            data: data_lines.join("\n"),
        })
    }
}

// ============================================================================
// Streaming conversion
// ============================================================================

/// An outbound SSE event ready to send to the client.
pub struct SseOut {
    pub event: Option<String>,
    pub data: String,
}

impl SseOut {
    fn json(event: &str, value: Value) -> Self {
        Self {
            event: Some(event.to_string()),
            data: value.to_string(),
        }
    }

    fn data_only(data: String) -> Self {
        Self { event: None, data }
    }
}

/// Stateful SSE stream converter. `push` processes one upstream event;
/// `finish` is called when the upstream stream ends.
pub enum StreamConverter {
    /// Same protocol, OpenAI: pass events through, capturing usage.
    /// strip_usage_chunk 为 true 时剥掉仅含 usage 的结尾 chunk
    /// (客户端未请求 include_usage,由网关注入时不应多送一个 chunk)。
    /// model 为客户端请求的模型名:转发前回写 chunk 的 model 字段。
    /// usage 4 元组:(未缓存输入, 输出, 总计, 缓存),见 extract_usage_any。
    PassOpenAi { usage: (i32, i32, i32, i32), strip_usage_chunk: bool, model: String },
    /// Same protocol, Anthropic: pass events through, capturing usage.
    /// model 用途同上(message_start 事件的 message.model 回写)。
    PassAnthropic { usage: (i32, i32, i32, i32), model: String },
    /// OpenAI upstream → Anthropic client.
    OpenAiToAnthropic(OaStreamState),
    /// Anthropic upstream → OpenAI client.
    AnthropicToOpenAi(AnStreamState),
    /// Same protocol, Responses API: pass events through, capturing usage.
    /// usage 来自结尾的 response.completed 事件;response.created/completed
    /// 里的 response.model 回写为客户端请求的模型名。
    PassResponses { usage: (i32, i32, i32, i32), model: String },
    /// OpenAI chat-completions upstream → Responses API client.
    OpenAiToResponses(OarStreamState),
    /// Anthropic upstream → Responses API client:AnStreamState 先转成 chat
    /// chunks,再喂给 OarStreamState 组合出 responses 事件。
    AnthropicToResponses { an: AnStreamState, oar: OarStreamState },
}

impl StreamConverter {
    pub fn new(client_protocol: &str, provider_protocol: &str, model: &str) -> Self {
        match (client_protocol, provider_protocol) {
            (PROTOCOL_OPENAI, PROTOCOL_OPENAI) => StreamConverter::PassOpenAi {
                usage: (0, 0, 0, 0),
                strip_usage_chunk: false,
                model: model.to_string(),
            },
            (PROTOCOL_ANTHROPIC, PROTOCOL_ANTHROPIC) => {
                StreamConverter::PassAnthropic { usage: (0, 0, 0, 0), model: model.to_string() }
            }
            (PROTOCOL_ANTHROPIC, PROTOCOL_OPENAI) => {
                StreamConverter::OpenAiToAnthropic(OaStreamState::new(model))
            }
            (PROTOCOL_OPENAI, PROTOCOL_ANTHROPIC) => {
                StreamConverter::AnthropicToOpenAi(AnStreamState::new(model))
            }
            (PROTOCOL_RESPONSES, PROTOCOL_RESPONSES) => {
                StreamConverter::PassResponses { usage: (0, 0, 0, 0), model: model.to_string() }
            }
            (PROTOCOL_RESPONSES, PROTOCOL_OPENAI) => {
                StreamConverter::OpenAiToResponses(OarStreamState::new(model))
            }
            (PROTOCOL_RESPONSES, PROTOCOL_ANTHROPIC) => StreamConverter::AnthropicToResponses {
                an: AnStreamState::new(model),
                oar: OarStreamState::new(model),
            },
            _ => StreamConverter::PassOpenAi {
                usage: (0, 0, 0, 0),
                strip_usage_chunk: false,
                model: model.to_string(),
            },
        }
    }

    /// 设置是否剥掉仅含 usage 的结尾 chunk(仅对 OpenAI 直通生效)。
    /// 客户端自己请求了 include_usage 时应传 false,usage chunk 原样转发。
    pub fn strip_usage_chunk(mut self, strip: bool) -> Self {
        if let StreamConverter::PassOpenAi { strip_usage_chunk, .. } = &mut self {
            *strip_usage_chunk = strip;
        }
        self
    }

    pub fn push(&mut self, ev: &SseEvent) -> Vec<SseOut> {
        match self {
            StreamConverter::PassOpenAi { usage, strip_usage_chunk, model } => {
                let mut forward = true;
                // 解析成功且转发时重序列化(model 字段回写);解析失败
                // (如 [DONE])原样直通。
                let mut rewritten: Option<String> = None;
                if let Ok(mut chunk) = serde_json::from_str::<Value>(&ev.data) {
                    let (p, c, t, k) = extract_usage_any(&chunk);
                    if t > 0 {
                        *usage = (p, c, t, k);
                    }
                    // 仅含 usage 的结尾 chunk:choices 缺失或为空且带 usage。
                    if *strip_usage_chunk
                        && !chunk["usage"].is_null()
                        && chunk["choices"].as_array().map(|a| a.is_empty()).unwrap_or(true)
                    {
                        forward = false;
                    }
                    // model 统一回写为客户端请求的模型名:映射后的上游真实
                    // 模型名不透传给终端用户(泄漏渠道拓扑)。
                    if forward && chunk["model"].is_string() {
                        chunk["model"] = json!(model.as_str());
                        rewritten = Some(chunk.to_string());
                    }
                }
                if forward {
                    vec![SseOut {
                        event: ev.event.clone(),
                        data: rewritten.unwrap_or_else(|| ev.data.clone()),
                    }]
                } else {
                    vec![]
                }
            }
            StreamConverter::PassAnthropic { usage, model } => {
                let mut rewritten: Option<String> = None;
                if let Ok(mut data) = serde_json::from_str::<Value>(&ev.data) {
                    match data["type"].as_str() {
                        Some("message_start") => {
                            let mu = &data["message"]["usage"];
                            // input_tokens 不含缓存;cache_creation(写缓存)按输入价
                            // 并入未缓存输入,cache_read 记入缓存维度单独计价。
                            usage.0 = clamp_token_count(mu["input_tokens"].as_i64())
                                .saturating_add(clamp_token_count(
                                    mu["cache_creation_input_tokens"].as_i64(),
                                ));
                            usage.3 = clamp_token_count(mu["cache_read_input_tokens"].as_i64());
                            // message_start 携带上游真实模型名,同样回写为
                            // 客户端请求的模型名。
                            if data["message"].is_object() {
                                data["message"]["model"] = json!(model.as_str());
                                rewritten = Some(data.to_string());
                            }
                        }
                        Some("message_delta") => {
                            let du = &data["usage"];
                            usage.1 = clamp_token_count(du["output_tokens"].as_i64());
                            // 部分上游在 message_delta 里才补缓存字段,出现时同步更新
                            // (实践中只在 message_start 上报;两处重复上报会高估,
                            // 与 cache_creation 的近似口径一致,可接受)。
                            if du["cache_read_input_tokens"].as_i64().is_some() {
                                usage.3 = clamp_token_count(du["cache_read_input_tokens"].as_i64());
                            }
                            if let Some(v) = du["cache_creation_input_tokens"].as_i64() {
                                usage.0 = usage.0.saturating_add(clamp_token_count(Some(v)));
                            }
                        }
                        _ => {}
                    }
                    usage.2 = usage.0.saturating_add(usage.1).saturating_add(usage.3);
                }
                vec![SseOut {
                    event: ev.event.clone(),
                    data: rewritten.unwrap_or_else(|| ev.data.clone()),
                }]
            }
            StreamConverter::OpenAiToAnthropic(state) => state.push(ev),
            StreamConverter::AnthropicToOpenAi(state) => state.push(ev),
            StreamConverter::PassResponses { usage, model } => {
                let mut rewritten: Option<String> = None;
                if let Ok(mut data) = serde_json::from_str::<Value>(&ev.data) {
                    match data["type"].as_str() {
                        Some("response.completed") | Some("response.incomplete") => {
                            let (p, c, t, k) = extract_usage_any(&data["response"]);
                            if t > 0 {
                                *usage = (p, c, t, k);
                            }
                            if data["response"]["model"].is_string() {
                                data["response"]["model"] = json!(model.as_str());
                                rewritten = Some(data.to_string());
                            }
                        }
                        Some("response.created") | Some("response.in_progress")
                            if data["response"]["model"].is_string() =>
                        {
                            data["response"]["model"] = json!(model.as_str());
                            rewritten = Some(data.to_string());
                        }
                        _ => {}
                    }
                }
                vec![SseOut {
                    event: ev.event.clone(),
                    data: rewritten.unwrap_or_else(|| ev.data.clone()),
                }]
            }
            StreamConverter::OpenAiToResponses(state) => state.push(ev),
            StreamConverter::AnthropicToResponses { an, oar } => {
                let mut out = Vec::new();
                for mid in an.push(ev) {
                    out.extend(oar.push(&SseEvent { event: mid.event, data: mid.data }));
                }
                out
            }
        }
    }

    pub fn finish(&mut self) -> Vec<SseOut> {
        match self {
            StreamConverter::PassOpenAi { .. } | StreamConverter::PassAnthropic { .. } => vec![],
            StreamConverter::OpenAiToAnthropic(state) => state.finalize(),
            StreamConverter::AnthropicToOpenAi(_) => {
                vec![SseOut::data_only("[DONE]".to_string())]
            }
            StreamConverter::PassResponses { .. } => vec![],
            // OpenAiToResponses 在收到 [DONE] 时已产出 response.completed;
            // 上游未发 [DONE] 直接 EOF 时 finalize 补齐收尾事件。
            StreamConverter::OpenAiToResponses(state) => state.finalize(),
            // AnStreamState 无自身收尾([DONE] 由外层枚举补),直接收尾 oar。
            StreamConverter::AnthropicToResponses { oar, .. } => oar.finalize(),
        }
    }

    pub fn usage(&self) -> (i32, i32, i32, i32) {
        match self {
            StreamConverter::PassOpenAi { usage, .. } | StreamConverter::PassAnthropic { usage, .. } => {
                *usage
            }
            StreamConverter::OpenAiToAnthropic(state) => state.usage,
            StreamConverter::AnthropicToOpenAi(state) => state.usage,
            StreamConverter::PassResponses { usage, .. } => *usage,
            StreamConverter::OpenAiToResponses(state) => state.usage,
            StreamConverter::AnthropicToResponses { oar, .. } => oar.usage,
        }
    }
}

// ----------------------------------------------------------------------------
// OpenAI chunks → Anthropic events
// ----------------------------------------------------------------------------

pub struct OaStreamState {
    msg_id: String,
    model: String,
    started: bool,
    finished: bool,
    next_block: usize,
    text_block: Option<usize>,
    /// 当前打开的 thinking 块。推理增量通常在正文之前到达,正文或工具
    /// 开始前由 close_open_block 关掉。
    thinking_block: Option<usize>,
    tool_blocks: HashMap<u64, usize>,
    open_tool_block: Option<usize>,
    stop_reason: Option<&'static str>,
    usage: (i32, i32, i32, i32),
}

impl OaStreamState {
    fn new(model: &str) -> Self {
        Self {
            msg_id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
            model: model.to_string(),
            started: false,
            finished: false,
            next_block: 0,
            text_block: None,
            thinking_block: None,
            tool_blocks: HashMap::new(),
            open_tool_block: None,
            stop_reason: None,
            usage: (0, 0, 0, 0),
        }
    }

    fn push(&mut self, ev: &SseEvent) -> Vec<SseOut> {
        // 错误帧之后不再产出任何事件(避免"错误后又成功完成")
        if self.finished {
            return vec![];
        }
        if ev.data.trim() == "[DONE]" {
            return self.finalize();
        }
        let chunk: Value = match serde_json::from_str(&ev.data) {
            Ok(v) => v,
            Err(_) => return vec![],
        };

        // 部分中转上游在 SSE 流内返回错误帧(HTTP 200 + {"error": ...}):
        // 不能先 message_start 再正常收尾,直接转发 Anthropic error 事件并终止。
        if chunk["error"].is_object() {
            self.finished = true;
            let err = &chunk["error"];
            return vec![SseOut::json(
                "error",
                json!({
                    "type": "error",
                    "error": {
                        "type": anthropic_error_type(err["type"].as_str().unwrap_or("api_error")),
                        "message": err["message"].as_str().unwrap_or("upstream error"),
                    }
                }),
            )];
        }

        let mut out = Vec::new();

        if !self.started {
            self.started = true;
            if let Some(id) = chunk["id"].as_str() {
                self.msg_id = id.to_string();
            }
            out.push(SseOut::json(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": {
                        "id": self.msg_id,
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": self.model,
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": {"input_tokens": self.usage.0, "output_tokens": 0},
                    }
                }),
            ));
        }

        // Capture usage (present in the final chunk when include_usage is set).
        let (p, c, t, k) = extract_usage_any(&chunk);
        if t > 0 {
            self.usage = (p, c, t, k);
        }

        if let Some(choices) = chunk["choices"].as_array() {
            for choice in choices {
                let delta = &choice["delta"];

                if let Some(text) = delta["reasoning_content"].as_str() {
                    if !text.is_empty() {
                        let idx = self.ensure_thinking_block(&mut out);
                        out.push(SseOut::json(
                            "content_block_delta",
                            json!({
                                "type": "content_block_delta",
                                "index": idx,
                                "delta": {"type": "thinking_delta", "thinking": text},
                            }),
                        ));
                    }
                }

                if let Some(text) = delta["content"].as_str() {
                    if !text.is_empty() {
                        let idx = self.ensure_text_block(&mut out);
                        out.push(SseOut::json(
                            "content_block_delta",
                            json!({
                                "type": "content_block_delta",
                                "index": idx,
                                "delta": {"type": "text_delta", "text": text},
                            }),
                        ));
                    }
                }

                if let Some(tcs) = delta["tool_calls"].as_array() {
                    for tc in tcs {
                        let tc_index = tc["index"].as_u64().unwrap_or(0);
                        if !self.tool_blocks.contains_key(&tc_index) {
                            // 开启新块前先关闭当前打开的块（Anthropic 事件顺序要求）
                            self.close_open_block(&mut out);
                            let block_idx = self.next_block;
                            self.next_block += 1;
                            self.tool_blocks.insert(tc_index, block_idx);
                            self.open_tool_block = Some(block_idx);
                            out.push(SseOut::json(
                                "content_block_start",
                                json!({
                                    "type": "content_block_start",
                                    "index": block_idx,
                                    "content_block": {
                                        "type": "tool_use",
                                        "id": tc["id"].as_str().unwrap_or(""),
                                        "name": tc["function"]["name"].as_str().unwrap_or(""),
                                        "input": {},
                                    }
                                }),
                            ));
                        }
                        let block_idx = self.tool_blocks[&tc_index];
                        if let Some(args) = tc["function"]["arguments"].as_str() {
                            if !args.is_empty() {
                                out.push(SseOut::json(
                                    "content_block_delta",
                                    json!({
                                        "type": "content_block_delta",
                                        "index": block_idx,
                                        "delta": {"type": "input_json_delta", "partial_json": args},
                                    }),
                                ));
                            }
                        }
                    }
                }

                if let Some(finish) = choice["finish_reason"].as_str() {
                    self.stop_reason = Some(openai_finish_to_anthropic(finish));
                }
            }
        }

        out
    }

    fn ensure_thinking_block(&mut self, out: &mut Vec<SseOut>) -> usize {
        if let Some(idx) = self.thinking_block {
            return idx;
        }
        self.close_open_block(out);
        let idx = self.next_block;
        self.next_block += 1;
        self.thinking_block = Some(idx);
        out.push(SseOut::json(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {"type": "thinking", "thinking": ""},
            }),
        ));
        idx
    }

    fn ensure_text_block(&mut self, out: &mut Vec<SseOut>) -> usize {
        if let Some(idx) = self.text_block {
            return idx;
        }
        // 开启新文本块前先关闭当前打开的块（如未结束的 tool_use 块）
        self.close_open_block(out);
        let idx = self.next_block;
        self.next_block += 1;
        self.text_block = Some(idx);
        out.push(SseOut::json(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {"type": "text", "text": ""},
            }),
        ));
        idx
    }

    /// 关闭当前打开的块（文本或 tool_use）。任何新块开启前必须先调用，
    /// 否则违反 Anthropic 的 content_block_start/stop 顺序约束。
    fn close_open_block(&mut self, out: &mut Vec<SseOut>) {
        self.close_thinking_block(out);
        self.close_text_block(out);
        if let Some(idx) = self.open_tool_block.take() {
            out.push(SseOut::json(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": idx}),
            ));
        }
    }

    fn close_thinking_block(&mut self, out: &mut Vec<SseOut>) {
        if let Some(idx) = self.thinking_block.take() {
            out.push(SseOut::json(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": idx}),
            ));
        }
    }

    fn close_text_block(&mut self, out: &mut Vec<SseOut>) {
        if let Some(idx) = self.text_block.take() {
            out.push(SseOut::json(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": idx}),
            ));
        }
    }

    fn finalize(&mut self) -> Vec<SseOut> {
        if self.finished || !self.started {
            return vec![];
        }
        self.finished = true;
        let mut out = Vec::new();

        // 复用同一函数关闭流末尾仍打开的块
        self.close_open_block(&mut out);
        self.tool_blocks.clear();

        out.push(SseOut::json(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": self.stop_reason.unwrap_or("end_turn"),
                    "stop_sequence": null,
                },
                // OpenAI 上游的 usage 在结尾 chunk 才到达,message_start 时
                // input_tokens 只能填 0;finalize 时真实值已捕获,回填给
                // Anthropic 客户端(其容忍 usage 里的额外字段)。
                "usage": {
                    "input_tokens": self.usage.0,
                    "output_tokens": self.usage.1,
                },
            }),
        ));
        out.push(SseOut::json("message_stop", json!({"type": "message_stop"})));
        out
    }
}

// ----------------------------------------------------------------------------
// Anthropic events → OpenAI chunks
// ----------------------------------------------------------------------------

pub struct AnStreamState {
    msg_id: String,
    model: String,
    created: i64,
    block_to_tc: HashMap<usize, usize>,
    next_tc: usize,
    usage: (i32, i32, i32, i32),
    finished: bool,
}

impl AnStreamState {
    fn new(model: &str) -> Self {
        Self {
            msg_id: "chatcmpl-unknown".to_string(),
            model: model.to_string(),
            created: chrono::Utc::now().timestamp(),
            block_to_tc: HashMap::new(),
            next_tc: 0,
            usage: (0, 0, 0, 0),
            finished: false,
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>, usage: Option<Value>) -> SseOut {
        let mut obj = json!({
            "id": self.msg_id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        });
        if let Some(u) = usage {
            obj["usage"] = u;
        }
        SseOut::data_only(obj.to_string())
    }

    fn push(&mut self, ev: &SseEvent) -> Vec<SseOut> {
        // 错误事件之后不再产出任何 chunk
        if self.finished {
            return vec![];
        }
        let data: Value = match serde_json::from_str(&ev.data) {
            Ok(v) => v,
            Err(_) => return vec![],
        };
        let event_type = ev
            .event
            .clone()
            .or_else(|| data["type"].as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        match event_type.as_str() {
            // Anthropic 流内错误事件转成 OpenAI 形状的 error data;
            // Anthropic→Responses 组合链里再由 OarStreamState 转成 error 事件。
            "error" => {
                self.finished = true;
                let err = &data["error"];
                vec![SseOut::data_only(
                    json!({
                        "error": {
                            "message": err["message"].as_str().unwrap_or("upstream error"),
                            "type": err["type"].as_str().unwrap_or("api_error"),
                        }
                    })
                    .to_string(),
                )]
            }
            "message_start" => {
                let msg = &data["message"];
                if let Some(id) = msg["id"].as_str() {
                    self.msg_id = id.to_string();
                }
                // model 不随上游 message_start 更新:保持客户端请求的模型名,
                // 映射后的上游真实模型名不透传给终端用户。
                // 缓存拆分口径同 PassAnthropic:cache_creation 并入未缓存输入,
                // cache_read 记入缓存维度。
                let mu = &msg["usage"];
                self.usage.0 = clamp_token_count(mu["input_tokens"].as_i64())
                    .saturating_add(clamp_token_count(mu["cache_creation_input_tokens"].as_i64()));
                self.usage.3 = clamp_token_count(mu["cache_read_input_tokens"].as_i64());
                self.usage.2 = self.usage.0.saturating_add(self.usage.1).saturating_add(self.usage.3);
                vec![self.chunk(json!({"role": "assistant", "content": ""}), None, None)]
            }
            "content_block_start" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                let block = &data["content_block"];
                if block["type"].as_str() == Some("tool_use") {
                    let tc_index = self.next_tc;
                    self.next_tc += 1;
                    self.block_to_tc.insert(index, tc_index);
                    vec![self.chunk(
                        json!({
                            "tool_calls": [{
                                "index": tc_index,
                                "id": block["id"].as_str().unwrap_or(""),
                                "type": "function",
                                "function": {
                                    "name": block["name"].as_str().unwrap_or(""),
                                    "arguments": "",
                                }
                            }]
                        }),
                        None,
                        None,
                    )]
                } else {
                    vec![]
                }
            }
            "content_block_delta" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                let delta = &data["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let text = delta["text"].as_str().unwrap_or("");
                        if text.is_empty() {
                            vec![]
                        } else {
                            vec![self.chunk(json!({"content": text}), None, None)]
                        }
                    }
                    Some("thinking_delta") => {
                        let text = delta["thinking"].as_str().unwrap_or("");
                        if text.is_empty() {
                            vec![]
                        } else {
                            vec![self.chunk(json!({"reasoning_content": text}), None, None)]
                        }
                    }
                    // signature 没有 OpenAI 对应增量,丢掉即可。
                    Some("signature_delta") => vec![],
                    Some("input_json_delta") => {
                        let tc_index = *self.block_to_tc.get(&index).unwrap_or(&0);
                        vec![self.chunk(
                            json!({
                                "tool_calls": [{
                                    "index": tc_index,
                                    "function": {
                                        "arguments": delta["partial_json"].as_str().unwrap_or(""),
                                    }
                                }]
                            }),
                            None,
                            None,
                        )]
                    }
                    _ => vec![],
                }
            }
            "message_delta" => {
                let du = &data["usage"];
                if du["output_tokens"].as_i64().is_some() {
                    self.usage.1 = clamp_token_count(du["output_tokens"].as_i64());
                }
                // 缓存字段若在 message_delta 才上报,同步更新(口径同 PassAnthropic)
                if du["cache_read_input_tokens"].as_i64().is_some() {
                    self.usage.3 = clamp_token_count(du["cache_read_input_tokens"].as_i64());
                }
                if let Some(v) = du["cache_creation_input_tokens"].as_i64() {
                    self.usage.0 = self.usage.0.saturating_add(clamp_token_count(Some(v)));
                }
                self.usage.2 = self.usage.0.saturating_add(self.usage.1).saturating_add(self.usage.3);
                let mut out = Vec::new();
                if let Some(stop) = data["delta"]["stop_reason"].as_str() {
                    let finish = anthropic_stop_to_openai(stop);
                    // 客户端可见的 prompt_tokens 维持 OpenAI 口径(含缓存部分);
                    // cached_tokens 一并带出,Anthropic→Responses 组合链的
                    // OarStreamState 靠它把缓存从输入里拆出单独计价。
                    let usage = json!({
                        "prompt_tokens": self.usage.0.saturating_add(self.usage.3),
                        "prompt_tokens_details": {"cached_tokens": self.usage.3},
                        "completion_tokens": self.usage.1,
                        "total_tokens": self.usage.2,
                    });
                    out.push(self.chunk(json!({}), Some(finish), Some(usage)));
                }
                out
            }
            // content_block_stop, message_stop, ping: no OpenAI equivalent.
            _ => vec![],
        }
    }
}

// ----------------------------------------------------------------------------
// OpenAI chat chunks → Responses API events
// ----------------------------------------------------------------------------

/// One in-flight function_call output item being assembled from chat
/// tool_call deltas.
struct OarToolItem {
    output_index: usize,
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
}

pub struct OarStreamState {
    resp_id: String,
    msg_item_id: String,
    model: String,
    created: i64,
    started: bool,
    finished: bool,
    /// 文本 output item 的 output_index;None 表示尚未开启。
    text_index: Option<usize>,
    text: String,
    /// chat tool_call index → output item。
    tool_items: HashMap<u64, OarToolItem>,
    next_output_index: usize,
    usage: (i32, i32, i32, i32),
    /// 最终非空 finish_reason:finalize 据此判断 completed 还是 incomplete。
    finish_reason: Option<String>,
}

impl OarStreamState {
    fn new(model: &str) -> Self {
        Self {
            resp_id: format!("resp_{}", uuid::Uuid::new_v4().simple()),
            msg_item_id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
            model: model.to_string(),
            created: chrono::Utc::now().timestamp(),
            started: false,
            finished: false,
            text_index: None,
            text: String::new(),
            tool_items: HashMap::new(),
            next_output_index: 0,
            usage: (0, 0, 0, 0),
            finish_reason: None,
        }
    }

    /// usage 4 元组(未缓存输入, 输出, 总计, 缓存)→ Responses usage 对象;
    /// 客户端可见的 input_tokens 含缓存部分,与 extract_usage_any 的
    /// OpenAI 口径互逆。
    fn responses_usage(&self) -> Value {
        json!({
            "input_tokens": self.usage.0.saturating_add(self.usage.3),
            "input_tokens_details": {"cached_tokens": self.usage.3},
            "output_tokens": self.usage.1,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": self.usage.2,
        })
    }

    fn push(&mut self, ev: &SseEvent) -> Vec<SseOut> {
        // 错误帧之后不再产出任何事件(避免"错误后又成功完成")
        if self.finished {
            return vec![];
        }
        if ev.data.trim() == "[DONE]" {
            return self.finalize();
        }
        let chunk: Value = match serde_json::from_str(&ev.data) {
            Ok(v) => v,
            Err(_) => return vec![],
        };

        // 流内错误帧(形状见 OaStreamState):Responses 的 error 事件本身是
        // 终止帧,不先发 response.created/in_progress,也不再 response.completed。
        if chunk["error"].is_object() {
            self.finished = true;
            let err = &chunk["error"];
            let code = err["code"]
                .as_str()
                .or_else(|| err["type"].as_str())
                .unwrap_or("upstream_error");
            return vec![SseOut::json(
                "error",
                json!({
                    "type": "error",
                    "code": code,
                    "message": err["message"].as_str().unwrap_or("upstream error"),
                }),
            )];
        }

        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            let resp = json!({
                "id": self.resp_id,
                "object": "response",
                "created_at": self.created,
                "status": "in_progress",
                "model": self.model,
                "output": [],
            });
            out.push(SseOut::json(
                "response.created",
                json!({"type": "response.created", "response": resp}),
            ));
            out.push(SseOut::json(
                "response.in_progress",
                json!({"type": "response.in_progress", "response": resp}),
            ));
        }

        // Capture usage (present in the final chunk when include_usage is set).
        let (p, c, t, k) = extract_usage_any(&chunk);
        if t > 0 {
            self.usage = (p, c, t, k);
        }

        if let Some(choices) = chunk["choices"].as_array() {
            for choice in choices {
                let delta = &choice["delta"];

                if let Some(finish) = choice["finish_reason"].as_str().filter(|f| !f.is_empty()) {
                    self.finish_reason = Some(finish.to_string());
                }

                if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
                    self.ensure_text_item(&mut out);
                    let idx = self.text_index.unwrap_or(0);
                    out.push(SseOut::json(
                        "response.output_text.delta",
                        json!({
                            "type": "response.output_text.delta",
                            "item_id": self.msg_item_id,
                            "output_index": idx,
                            "content_index": 0,
                            "delta": text,
                        }),
                    ));
                    self.text.push_str(text);
                }

                if let Some(tcs) = delta["tool_calls"].as_array() {
                    for tc in tcs {
                        let tc_index = tc["index"].as_u64().unwrap_or(0);
                        if !self.tool_items.contains_key(&tc_index) {
                            let output_index = self.next_output_index;
                            self.next_output_index += 1;
                            let item = OarToolItem {
                                output_index,
                                item_id: format!("fc_{}", uuid::Uuid::new_v4().simple()),
                                call_id: tc["id"].as_str().unwrap_or("").to_string(),
                                name: tc["function"]["name"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string(),
                                arguments: String::new(),
                            };
                            out.push(SseOut::json(
                                "response.output_item.added",
                                json!({
                                    "type": "response.output_item.added",
                                    "output_index": output_index,
                                    "item": {
                                        "id": item.item_id,
                                        "type": "function_call",
                                        "status": "in_progress",
                                        "call_id": item.call_id,
                                        "name": item.name,
                                        "arguments": "",
                                    }
                                }),
                            ));
                            self.tool_items.insert(tc_index, item);
                        }
                        if let Some(args) = tc["function"]["arguments"].as_str().filter(|a| !a.is_empty()) {
                            let item = self.tool_items.get_mut(&tc_index).unwrap();
                            item.arguments.push_str(args);
                            out.push(SseOut::json(
                                "response.function_call_arguments.delta",
                                json!({
                                    "type": "response.function_call_arguments.delta",
                                    "item_id": item.item_id,
                                    "output_index": item.output_index,
                                    "delta": args,
                                }),
                            ));
                        }
                    }
                }
            }
        }

        out
    }

    /// 首次文本 delta 前开启 message output item 与 output_text content part。
    fn ensure_text_item(&mut self, out: &mut Vec<SseOut>) {
        if self.text_index.is_some() {
            return;
        }
        let idx = self.next_output_index;
        self.next_output_index += 1;
        self.text_index = Some(idx);
        out.push(SseOut::json(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": idx,
                "item": {
                    "id": self.msg_item_id,
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                }
            }),
        ));
        out.push(SseOut::json(
            "response.content_part.added",
            json!({
                "type": "response.content_part.added",
                "item_id": self.msg_item_id,
                "output_index": idx,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []},
            }),
        ));
    }

    /// 汇总已完成的 output items,用于 response.completed 的完整 response 对象。
    fn completed_output(&self) -> Vec<Value> {
        let mut items: Vec<(usize, Value)> = Vec::new();
        if let Some(idx) = self.text_index {
            items.push((
                idx,
                json!({
                    "id": self.msg_item_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": self.text, "annotations": []}],
                }),
            ));
        }
        let mut tools: Vec<&OarToolItem> = self.tool_items.values().collect();
        tools.sort_by_key(|t| t.output_index);
        for t in tools {
            items.push((
                t.output_index,
                json!({
                    "id": t.item_id,
                    "type": "function_call",
                    "status": "completed",
                    "call_id": t.call_id,
                    "name": t.name,
                    "arguments": t.arguments,
                }),
            ));
        }
        items.sort_by_key(|(idx, _)| *idx);
        items.into_iter().map(|(_, v)| v).collect()
    }

    fn finalize(&mut self) -> Vec<SseOut> {
        if self.finished || !self.started {
            return vec![];
        }
        self.finished = true;
        let mut out = Vec::new();

        // 收尾顺序按 output_index:先关闭文本 item,再逐个关闭 function_call。
        if let Some(idx) = self.text_index {
            out.push(SseOut::json(
                "response.output_text.done",
                json!({
                    "type": "response.output_text.done",
                    "item_id": self.msg_item_id,
                    "output_index": idx,
                    "content_index": 0,
                    "text": self.text,
                }),
            ));
            out.push(SseOut::json(
                "response.content_part.done",
                json!({
                    "type": "response.content_part.done",
                    "item_id": self.msg_item_id,
                    "output_index": idx,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": self.text, "annotations": []},
                }),
            ));
            out.push(SseOut::json(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": idx,
                    "item": {
                        "id": self.msg_item_id,
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": self.text, "annotations": []}],
                    }
                }),
            ));
        }
        let mut tools: Vec<&OarToolItem> = self.tool_items.values().collect();
        tools.sort_by_key(|t| t.output_index);
        for t in tools {
            out.push(SseOut::json(
                "response.function_call_arguments.done",
                json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": t.item_id,
                    "output_index": t.output_index,
                    "arguments": t.arguments,
                }),
            ));
            out.push(SseOut::json(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": t.output_index,
                    "item": {
                        "id": t.item_id,
                        "type": "function_call",
                        "status": "completed",
                        "call_id": t.call_id,
                        "name": t.name,
                        "arguments": t.arguments,
                    }
                }),
            ));
        }

        // 截断/审核命中(length/content_filter)的响应发 response.incomplete,
        // 不能标 completed,否则客户端把截断结果当成完整结果。
        let incomplete = self
            .finish_reason
            .as_deref()
            .and_then(openai_finish_to_incomplete_reason);
        let (done_event, status, incomplete_details) = match incomplete {
            Some(reason) => ("response.incomplete", "incomplete", json!({"reason": reason})),
            None => ("response.completed", "completed", Value::Null),
        };
        out.push(SseOut::json(
            done_event,
            json!({
                "type": done_event,
                "response": {
                    "id": self.resp_id,
                    "object": "response",
                    "created_at": self.created,
                    "status": status,
                    "incomplete_details": incomplete_details,
                    "model": self.model,
                    "output": self.completed_output(),
                    "usage": self.responses_usage(),
                }
            }),
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn sse(data: &str) -> SseEvent {
        SseEvent { event: None, data: data.to_string() }
    }

    fn sse_typed(event: &str, data: &str) -> SseEvent {
        SseEvent { event: Some(event.to_string()), data: data.to_string() }
    }

    fn out_types(outs: &[SseOut]) -> Vec<String> {
        outs.iter()
            .map(|o| {
                o.event.clone().unwrap_or_else(|| {
                    serde_json::from_str::<Value>(&o.data)
                        .ok()
                        .and_then(|v| v["type"].as_str().map(String::from))
                        .unwrap_or_else(|| o.data.clone())
                })
            })
            .collect()
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
    fn request_openai_to_anthropic_basic() {
        let body = json!({
            "model": "claude-3",
            "messages": [
                {"role": "system", "content": "be nice"},
                {"role": "user", "content": "hello"}
            ]
        });
        let out = convert_request(&body, "openai", "anthropic");
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
        let out = convert_request(&body, "openai", "anthropic");
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
        let out = convert_request(&body, "openai", "anthropic");
        assert_eq!(out["stop_sequences"], json!(["END"]));
        assert_eq!(out["tools"][0]["name"], "f");
        assert_eq!(out["tools"][0]["input_schema"], json!({"type": "object"}));
        assert_eq!(out["tool_choice"], json!({"type": "any"}));
    }

    #[test]
    fn request_anthropic_to_openai_basic() {
        let body = json!({
            "model": "gpt-4",
            "max_tokens": 50,
            "stream": true,
            "system": "sys",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = convert_request(&body, "anthropic", "openai");
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
        let out = convert_request(&body, "anthropic", "openai");
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0], json!({"role": "tool", "tool_call_id": "t1", "content": "result-text"}));
        assert_eq!(msgs[1], json!({"role": "user", "content": "follow-up"}));
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
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
        let out = convert_request(&body, "openai", "anthropic");
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
        let out = convert_request(&body, "anthropic", "openai");
        assert_eq!(out["thinking"]["budget_tokens"], 1024);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(msgs[1]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(msgs[2]["reasoning_content"], "because");
        assert_eq!(msgs[2]["reasoning_signature"], "sig");
        assert_eq!(msgs[2]["content"], "answer");
    }

    // ---- convert_response ----

    #[test]
    fn response_openai_to_anthropic() {
        let resp = json!({
            "id": "chatcmpl-1",
            "model": "gpt-4",
            "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "length"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
        });
        let out = convert_response(&resp, "openai", "anthropic", "gpt-4");
        assert_eq!(out["type"], "message");
        assert_eq!(out["content"], json!([{"type": "text", "text": "hi"}]));
        assert_eq!(out["stop_reason"], "max_tokens"); // length → max_tokens
        assert_eq!(out["usage"], json!({"input_tokens": 3, "output_tokens": 4}));
    }

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
        let an = convert_response(&oa, "openai", "anthropic", "m");
        assert_eq!(an["content"][0]["type"], "thinking");
        assert_eq!(an["content"][0]["thinking"], "think");
        assert_eq!(an["content"][0]["signature"], "sig");
        assert_eq!(an["content"][1]["text"], "ans");

        let back = convert_response(&an, "anthropic", "openai", "m");
        assert_eq!(back["choices"][0]["message"]["reasoning_content"], "think");
        assert_eq!(back["choices"][0]["message"]["content"], "ans");
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

    #[test]
    fn response_openai_to_anthropic_invalid_choices() {
        // choices 为空或缺 message（如上游错误体）时按上游错误处理
        for bad in [
            json!({"choices": []}),
            json!({"choices": [{}]}),
            json!({"error": {"message": "boom"}}),
        ] {
            let out = convert_response(&bad, "openai", "anthropic", "m");
            assert_eq!(out["type"], "error");
            assert_eq!(out["error"]["type"], "upstream_error");
        }
    }

    // ---- extract_usage_any ----

    #[test]
    fn usage_extraction_both_protocols() {
        let oa = json!({"usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}});
        assert_eq!(extract_usage_any(&oa), (1, 2, 3, 0));
        let an = json!({"usage": {"input_tokens": 4, "output_tokens": 5}});
        assert_eq!(extract_usage_any(&an), (4, 5, 9, 0)); // total derived
        let none = json!({});
        assert_eq!(extract_usage_any(&none), (0, 0, 0, 0));
    }

    #[test]
    fn usage_extraction_splits_cached_tokens() {
        // OpenAI:prompt_tokens 含缓存,按 prompt_tokens_details.cached_tokens 拆出
        let oa = json!({"usage": {
            "prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12,
            "prompt_tokens_details": {"cached_tokens": 4}
        }});
        assert_eq!(extract_usage_any(&oa), (6, 2, 12, 4));
        // cached 超过 prompt_tokens 时未缓存部分饱和为 0(上游数据不可信)
        let oa_bad = json!({"usage": {
            "prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3,
            "prompt_tokens_details": {"cached_tokens": 5}
        }});
        assert_eq!(extract_usage_any(&oa_bad), (0, 2, 3, 5));
        // Anthropic:cache_read 按缓存价,cache_creation 按输入价并入未缓存输入;
        // total = 未缓存输入 + 缓存 + 输出
        let an = json!({"usage": {
            "input_tokens": 4, "output_tokens": 5,
            "cache_read_input_tokens": 7, "cache_creation_input_tokens": 3
        }});
        assert_eq!(extract_usage_any(&an), (7, 5, 19, 7));
        // 负数缓存字段按 0 钳制
        let an_neg = json!({"usage": {"input_tokens": 4, "output_tokens": 5, "cache_read_input_tokens": -2}});
        assert_eq!(extract_usage_any(&an_neg), (4, 5, 9, 0));
    }

    // ---- SSE parsing ----

    #[test]
    fn sse_lf_boundary() {
        let mut buf = b"data: one\n\ndata: two\n\npartial".to_vec();
        let events = parse_sse_events(&mut buf);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "one");
        assert_eq!(events[1].data, "two");
        assert_eq!(buf, b"partial");
    }

    #[test]
    fn sse_crlf_boundary() {
        let mut buf = b"data: one\r\n\r\ndata: two\r\n\r\n".to_vec();
        let events = parse_sse_events(&mut buf);
        assert_eq!(events.len(), 2);
        assert!(buf.is_empty());
    }

    #[test]
    fn sse_bare_cr_boundary() {
        let mut buf = b"data: one\r\rdata: two\r\rpartial".to_vec();
        let events = parse_sse_events(&mut buf);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "one");
        assert_eq!(events[1].data, "two");
        assert_eq!(buf, b"partial");
    }

    #[test]
    fn sse_split_across_chunks() {
        let mut buf = b"data: hel".to_vec();
        assert!(parse_sse_events(&mut buf).is_empty());
        buf.extend_from_slice(b"lo\n\n");
        let events = parse_sse_events(&mut buf);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn sse_multiline_data_and_event_field() {
        let mut buf = b"event: message_start\ndata: first line\ndata: second line\n\n".to_vec();
        let events = parse_sse_events(&mut buf);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "first line\nsecond line");
    }

    #[test]
    fn sse_block_without_data_ignored() {
        let mut buf = b": ping comment\n\nevent: only-event\n\n".to_vec();
        assert!(parse_sse_events(&mut buf).is_empty());
        assert!(buf.is_empty());
    }

    #[test]
    fn sse_remaining_parses_tail() {
        let mut buf = b"data: tail".to_vec();
        let ev = parse_sse_remaining(&mut buf).unwrap();
        assert_eq!(ev.data, "tail");
        assert!(buf.is_empty());
        assert!(parse_sse_remaining(&mut buf).is_none());
    }

    // ---- StreamConverter: pass-through ----

    #[test]
    fn stream_pass_openai_captures_usage() {
        let mut c = StreamConverter::new("openai", "openai", "m");
        let outs = c.push(&sse(r#"{"choices":[{"delta":{"content":"x"}}]}"#));
        assert_eq!(outs.len(), 1);
        assert_eq!(c.usage(), (0, 0, 0, 0));
        c.push(&sse(r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#));
        assert_eq!(c.usage(), (1, 2, 3, 0));
        assert!(c.finish().is_empty());
    }

    #[test]
    fn stream_pass_openai_strips_unrequested_usage_chunk() {
        // 客户端未请求 include_usage(由网关注入):usage chunk 只记账、不转发。
        let mut c = StreamConverter::new("openai", "openai", "m").strip_usage_chunk(true);
        let outs = c.push(&sse(r#"{"choices":[{"delta":{"content":"x"}}]}"#));
        assert_eq!(outs.len(), 1);
        let outs = c.push(&sse(r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#));
        assert!(outs.is_empty(), "usage-only chunk must not reach the client");
        assert_eq!(c.usage(), (1, 2, 3, 0));
    }

    #[test]
    fn stream_pass_anthropic_captures_usage() {
        let mut c = StreamConverter::new("anthropic", "anthropic", "m");
        c.push(&sse_typed("message_start", r#"{"type":"message_start","message":{"usage":{"input_tokens":7}}}"#));
        assert_eq!(c.usage(), (7, 0, 7, 0));
        c.push(&sse_typed("message_delta", r#"{"type":"message_delta","usage":{"output_tokens":4}}"#));
        assert_eq!(c.usage(), (7, 4, 11, 0));
    }

    #[test]
    fn stream_pass_anthropic_splits_cache_usage() {
        // message_start 携带缓存字段:cache_creation 并入未缓存输入,
        // cache_read 记入缓存维度;total = 三者之和
        let mut c = StreamConverter::new("anthropic", "anthropic", "m");
        c.push(&sse_typed("message_start", r#"{"type":"message_start","message":{"usage":{"input_tokens":7,"cache_creation_input_tokens":3,"cache_read_input_tokens":5}}}"#));
        assert_eq!(c.usage(), (10, 0, 15, 5));
        c.push(&sse_typed("message_delta", r#"{"type":"message_delta","usage":{"output_tokens":4}}"#));
        assert_eq!(c.usage(), (10, 4, 19, 5));
    }

    #[test]
    fn stream_pass_openai_rewrites_chunk_model() {
        // 映射场景:chunk 里的上游真实模型名回写为请求模型名。
        let mut c = StreamConverter::new("openai", "openai", "gpt-4");
        let outs = c.push(&sse(r#"{"model":"real-gpt-4-2024","choices":[{"delta":{"content":"x"}}]}"#));
        assert_eq!(outs.len(), 1);
        let chunk: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(chunk["model"], "gpt-4");
        assert_eq!(chunk["choices"][0]["delta"]["content"], "x");
        // [DONE] 等非 JSON 帧原样直通
        let outs = c.push(&sse("[DONE]"));
        assert_eq!(outs[0].data, "[DONE]");
    }

    #[test]
    fn stream_pass_anthropic_rewrites_message_model() {
        let mut c = StreamConverter::new("anthropic", "anthropic", "claude-alias");
        let outs = c.push(&sse_typed(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-real","usage":{"input_tokens":7}}}"#,
        ));
        let data: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(data["message"]["model"], "claude-alias");
        assert_eq!(c.usage(), (7, 0, 7, 0));
    }

    #[test]
    fn stream_anthropic_to_openai_keeps_requested_model() {
        // 转换路径:上游 message_start 的真实模型名不覆盖请求模型名。
        let mut c = StreamConverter::new("openai", "anthropic", "claude-alias");
        let outs = c.push(&sse_typed(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-real","usage":{"input_tokens":9}}}"#,
        ));
        let chunk: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(chunk["model"], "claude-alias");
    }

    // ---- StreamConverter: OpenAI upstream → Anthropic client ----

    #[test]
    fn stream_openai_to_anthropic_text_flow() {
        let mut c = StreamConverter::new("anthropic", "openai", "claude-x");

        let outs = c.push(&sse(r#"{"id":"chatcmpl-1","choices":[{"delta":{"role":"assistant","content":"Hel"},"finish_reason":null}]}"#));
        assert_eq!(out_types(&outs), vec!["message_start", "content_block_start", "content_block_delta"]);

        let outs = c.push(&sse(r#"{"id":"chatcmpl-1","choices":[{"delta":{"content":"lo"}}]}"#));
        assert_eq!(out_types(&outs), vec!["content_block_delta"]);

        c.push(&sse(r#"{"id":"chatcmpl-1","choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#));
        assert_eq!(c.usage(), (5, 2, 7, 0));

        let outs = c.push(&sse("[DONE]"));
        assert_eq!(out_types(&outs), vec!["content_block_stop", "message_delta", "message_stop"]);
        let delta: Value = serde_json::from_str(&outs[1].data).unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "end_turn");
        assert_eq!(delta["usage"]["output_tokens"], 2);

        // finalize is one-shot
        assert!(c.finish().is_empty());
    }

    #[test]
    fn stream_openai_to_anthropic_finish_reason_mapping() {
        let mut c = StreamConverter::new("anthropic", "openai", "m");
        c.push(&sse(r#"{"choices":[{"delta":{"content":"x"}}]}"#));
        c.push(&sse(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#));
        let outs = c.push(&sse("[DONE]"));
        let delta: Value = serde_json::from_str(&outs[1].data).unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "max_tokens");
    }

    #[test]
    fn stream_openai_to_anthropic_closes_block_before_new_one() {
        let mut c = StreamConverter::new("anthropic", "openai", "m");

        // 文本块 → tool_use 块 → 第二个 tool_use 块 → 流结束
        let outs = c.push(&sse(r#"{"choices":[{"delta":{"content":"hi"}}]}"#));
        assert_eq!(out_types(&outs), vec!["message_start", "content_block_start", "content_block_delta"]);

        let outs = c.push(&sse(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"t1","function":{"name":"f1","arguments":"{"}}]}}]}"#));
        // 文本块先 stop，再开启 tool_use 块
        assert_eq!(
            out_types(&outs),
            vec!["content_block_stop", "content_block_start", "content_block_delta"]
        );
        let stop: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(stop["index"], 0);
        let start: Value = serde_json::from_str(&outs[1].data).unwrap();
        assert_eq!(start["index"], 1);

        let outs = c.push(&sse(r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"t2","function":{"name":"f2","arguments":"{}"}}]}}]}"#));
        // 前一个 tool_use 块先 stop，再开启第二个
        assert_eq!(
            out_types(&outs),
            vec!["content_block_stop", "content_block_start", "content_block_delta"]
        );
        let stop: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(stop["index"], 1);
        let start: Value = serde_json::from_str(&outs[1].data).unwrap();
        assert_eq!(start["index"], 2);

        // finalize 复用同一关闭逻辑：只停当前打开的第 2 块
        let outs = c.push(&sse("[DONE]"));
        assert_eq!(out_types(&outs), vec!["content_block_stop", "message_delta", "message_stop"]);
        let stop: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(stop["index"], 2);
    }

    #[test]
    fn stream_openai_to_anthropic_text_after_tool_closes_tool_block() {
        let mut c = StreamConverter::new("anthropic", "openai", "m");
        c.push(&sse(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"t1","function":{"name":"f","arguments":"{}"}}]}}]}"#));
        // 工具块之后又出现文本：先停 tool_use 块再开新文本块
        let outs = c.push(&sse(r#"{"choices":[{"delta":{"content":"tail"}}]}"#));
        assert_eq!(
            out_types(&outs),
            vec!["content_block_stop", "content_block_start", "content_block_delta"]
        );
        let stop: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(stop["index"], 0);
        let start: Value = serde_json::from_str(&outs[1].data).unwrap();
        assert_eq!(start["index"], 1);
    }

    // ---- StreamConverter: Anthropic upstream → OpenAI client ----

    #[test]
    fn stream_anthropic_to_openai_text_flow() {
        let mut c = StreamConverter::new("openai", "anthropic", "claude-3");

        let outs = c.push(&sse_typed("message_start", r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-3","usage":{"input_tokens":9}}}"#));
        assert_eq!(outs.len(), 1);
        let chunk: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(chunk["id"], "msg_1");
        assert_eq!(chunk["choices"][0]["delta"]["role"], "assistant");

        let outs = c.push(&sse_typed("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#));
        let chunk: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(chunk["choices"][0]["delta"]["content"], "Hi");

        let outs = c.push(&sse_typed("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#));
        let chunk: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(chunk["choices"][0]["finish_reason"], "stop");
        assert_eq!(
            chunk["usage"],
            json!({
                "prompt_tokens": 9,
                "prompt_tokens_details": {"cached_tokens": 0},
                "completion_tokens": 3,
                "total_tokens": 12,
            })
        );
        assert_eq!(c.usage(), (9, 3, 12, 0));

        let outs = c.finish();
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].data, "[DONE]");
    }

    #[test]
    fn stream_openai_to_anthropic_reasoning_before_text() {
        let mut c = StreamConverter::new("anthropic", "openai", "m");
        let outs = c.push(&sse(
            r#"{"choices":[{"delta":{"reasoning_content":"think"}}]}"#,
        ));
        assert_eq!(
            out_types(&outs),
            vec!["message_start", "content_block_start", "content_block_delta"]
        );
        let start: Value = serde_json::from_str(&outs[1].data).unwrap();
        assert_eq!(start["content_block"]["type"], "thinking");
        let delta: Value = serde_json::from_str(&outs[2].data).unwrap();
        assert_eq!(delta["delta"]["thinking"], "think");

        let outs = c.push(&sse(r#"{"choices":[{"delta":{"content":"ans"}}]}"#));
        assert_eq!(
            out_types(&outs),
            vec!["content_block_stop", "content_block_start", "content_block_delta"]
        );
        let start: Value = serde_json::from_str(&outs[1].data).unwrap();
        assert_eq!(start["content_block"]["type"], "text");
        assert_eq!(start["index"], 1);
    }

    #[test]
    fn stream_anthropic_to_openai_thinking_delta() {
        let mut c = StreamConverter::new("openai", "anthropic", "m");
        c.push(&sse_typed(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":1}}}"#,
        ));
        let outs = c.push(&sse_typed(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        ));
        assert!(outs.is_empty());
        let outs = c.push(&sse_typed(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
        ));
        let chunk: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(chunk["choices"][0]["delta"]["reasoning_content"], "hmm");
        let outs = c.push(&sse_typed(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
        ));
        assert!(outs.is_empty());
    }

    #[test]
    fn stream_anthropic_to_openai_tool_flow() {
        let mut c = StreamConverter::new("openai", "anthropic", "m");
        c.push(&sse_typed("message_start", r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":1}}}"#));
        let outs = c.push(&sse_typed("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu1","name":"fn"}}"#));
        let chunk: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], "fn");

        let outs = c.push(&sse_typed("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#));
        let chunk: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{\"a\":");
    }

    #[test]
    fn stream_converters_ignore_garbage() {
        let mut c1 = StreamConverter::new("anthropic", "openai", "m");
        assert!(c1.push(&sse("not json")).is_empty());
        let mut c2 = StreamConverter::new("openai", "anthropic", "m");
        assert!(c2.push(&sse_typed("ping", "not json")).is_empty());
        // Unstarted converter finalizes to nothing.
        let mut c3 = StreamConverter::new("anthropic", "openai", "m");
        assert!(c3.push(&sse("[DONE]")).is_empty());
    }

    // ---- Responses API ----

    #[test]
    fn request_responses_passthrough_untouched() {
        let body = json!({"model": "gpt-5", "stream": true, "input": "hi", "store": false});
        assert_eq!(convert_request(&body, "responses", "responses"), body);
    }

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
        let out = convert_response(&resp, "openai", "responses", "gpt-5");
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

    #[test]
    fn extract_usage_responses_shape_splits_cache() {
        // Responses:input_tokens 含缓存,按 input_tokens_details 拆出
        let resp = json!({"usage": {"input_tokens": 10, "output_tokens": 4, "total_tokens": 14,
                                    "input_tokens_details": {"cached_tokens": 6}}});
        assert_eq!(extract_usage_any(&resp), (4, 4, 14, 6));
        // Anthropic 形状不受影响:input_tokens 不含缓存,cache_read 单独计价
        let an = json!({"usage": {"input_tokens": 10, "output_tokens": 4, "cache_read_input_tokens": 6}});
        assert_eq!(extract_usage_any(&an), (10, 4, 20, 6));
    }

    // ---- StreamConverter: Responses 客户端 ----

    #[test]
    fn stream_pass_responses_captures_usage_and_rewrites_model() {
        let mut c = StreamConverter::new("responses", "responses", "client-model");
        let outs = c.push(&sse_typed(
            "response.created",
            r#"{"type":"response.created","response":{"id":"r1","model":"upstream-real","status":"in_progress"}}"#,
        ));
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].event.as_deref(), Some("response.created"));
        let v: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(v["response"]["model"], "client-model"); // 上游真实模型名不透传

        let outs = c.push(&sse_typed(
            "response.output_text.delta",
            r#"{"type":"response.output_text.delta","delta":"hi"}"#,
        ));
        assert_eq!(outs[0].data, r#"{"type":"response.output_text.delta","delta":"hi"}"#);

        let outs = c.push(&sse_typed(
            "response.completed",
            r#"{"type":"response.completed","response":{"id":"r1","model":"upstream-real","status":"completed","usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14,"input_tokens_details":{"cached_tokens":6}}}}"#,
        ));
        let v: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(v["response"]["model"], "client-model");
        assert_eq!(c.usage(), (4, 4, 14, 6));
        assert!(c.finish().is_empty());
    }

    #[test]
    fn stream_openai_to_responses_text_flow() {
        let mut c = StreamConverter::new("responses", "openai", "gpt-5");

        let outs = c.push(&sse(r#"{"id":"chatcmpl-1","choices":[{"delta":{"role":"assistant","content":""}}]}"#));
        assert_eq!(out_types(&outs), vec!["response.created", "response.in_progress"]);

        let outs = c.push(&sse(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#));
        assert_eq!(
            out_types(&outs),
            vec!["response.output_item.added", "response.content_part.added", "response.output_text.delta"]
        );
        let delta: Value = serde_json::from_str(&outs[2].data).unwrap();
        assert_eq!(delta["delta"], "Hel");
        assert_eq!(delta["output_index"], 0);

        let outs = c.push(&sse(r#"{"choices":[{"delta":{"content":"lo"}}]}"#));
        assert_eq!(out_types(&outs), vec!["response.output_text.delta"]);

        // usage chunk(choices 为空)+ [DONE] 收尾
        let outs = c.push(&sse(r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14,"prompt_tokens_details":{"cached_tokens":6}}}"#));
        assert!(outs.is_empty());
        let outs = c.push(&sse("[DONE]"));
        assert_eq!(
            out_types(&outs),
            vec!["response.output_text.done", "response.content_part.done", "response.output_item.done", "response.completed"]
        );
        let completed: Value = serde_json::from_str(&outs[3].data).unwrap();
        assert_eq!(completed["response"]["status"], "completed");
        assert_eq!(completed["response"]["model"], "gpt-5");
        assert_eq!(completed["response"]["output"][0]["content"][0]["text"], "Hello");
        assert_eq!(completed["response"]["usage"]["input_tokens"], 10);
        assert_eq!(completed["response"]["usage"]["input_tokens_details"]["cached_tokens"], 6);
        assert_eq!(c.usage(), (4, 4, 14, 6));
    }

    #[test]
    fn stream_openai_to_responses_tool_flow() {
        let mut c = StreamConverter::new("responses", "openai", "m");
        c.push(&sse(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#));
        let outs = c.push(&sse(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"f","arguments":"{\"a\":"}}]}}]}"#));
        assert_eq!(
            out_types(&outs),
            vec!["response.output_item.added", "response.function_call_arguments.delta"]
        );
        let added: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(added["item"]["type"], "function_call");
        assert_eq!(added["item"]["call_id"], "c1");
        assert_eq!(added["output_index"], 0);

        let outs = c.push(&sse(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}"#));
        let delta: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(delta["delta"], "1}");

        let outs = c.push(&sse("[DONE]"));
        assert_eq!(
            out_types(&outs),
            vec!["response.function_call_arguments.done", "response.output_item.done", "response.completed"]
        );
        let completed: Value = serde_json::from_str(&outs[2].data).unwrap();
        assert_eq!(completed["response"]["output"][0]["type"], "function_call");
        assert_eq!(completed["response"]["output"][0]["arguments"], "{\"a\":1}");
    }

    #[test]
    fn stream_openai_to_responses_eof_without_done() {
        // 上游未发 [DONE] 直接 EOF:finish() 补齐收尾与 response.completed
        let mut c = StreamConverter::new("responses", "openai", "m");
        c.push(&sse(r#"{"choices":[{"delta":{"content":"x"}}]}"#));
        let outs = c.finish();
        assert_eq!(
            out_types(&outs),
            vec!["response.output_text.done", "response.content_part.done", "response.output_item.done", "response.completed"]
        );
    }

    #[test]
    fn stream_anthropic_to_responses_composition() {
        let mut c = StreamConverter::new("responses", "anthropic", "claude-4");
        let outs = c.push(&sse_typed("message_start", r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":9}}}"#));
        // an → chat chunk → oar:response.created/in_progress
        assert_eq!(out_types(&outs), vec!["response.created", "response.in_progress"]);

        let outs = c.push(&sse_typed("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#));
        assert_eq!(
            out_types(&outs),
            vec!["response.output_item.added", "response.content_part.added", "response.output_text.delta"]
        );

        c.push(&sse_typed("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#));
        let outs = c.finish();
        let types = out_types(&outs);
        assert!(types.contains(&"response.completed".to_string()));
        assert_eq!(c.usage(), (9, 3, 12, 0));
    }

    // ---- 流内错误帧 ----

    #[test]
    fn stream_openai_error_chunk_to_anthropic_error_event() {
        // 中转上游 HTTP 200 + SSE 流内错误帧:不得先 message_start 再正常收尾
        let mut c = StreamConverter::new("anthropic", "openai", "m");
        let outs = c.push(&sse(r#"{"error":{"message":"boom","type":"server_error"}}"#));
        assert_eq!(out_types(&outs), vec!["error"]);
        assert_eq!(outs[0].event.as_deref(), Some("error"));
        let v: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(v["type"], "error");
        // OpenAI 的 server_error 不是 Anthropic 标准错误类型,映射为 api_error
        assert_eq!(v["error"]["type"], "api_error");
        assert_eq!(v["error"]["message"], "boom");

        // 后续正常帧被忽略;[DONE]/finish 不再产出 message_stop
        assert!(c.push(&sse(r#"{"choices":[{"delta":{"content":"x"}}]}"#)).is_empty());
        assert!(c.push(&sse("[DONE]")).is_empty());
        assert!(c.finish().is_empty());
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
    fn stream_openai_error_chunk_mid_stream_to_anthropic() {
        // 流中途出错:已 message_start,错误事件后不再 message_delta/message_stop
        let mut c = StreamConverter::new("anthropic", "openai", "m");
        let outs = c.push(&sse(r#"{"id":"c1","choices":[{"delta":{"content":"Hel"}}]}"#));
        assert!(out_types(&outs).contains(&"message_start".to_string()));
        let outs = c.push(&sse(r#"{"error":{"message":"mid boom","type":"server_error"}}"#));
        assert_eq!(out_types(&outs), vec!["error"]);
        assert!(c.push(&sse("[DONE]")).is_empty());
    }

    #[test]
    fn stream_openai_error_chunk_to_responses_error_event() {
        let mut c = StreamConverter::new("responses", "openai", "m");
        let outs = c.push(&sse(r#"{"error":{"message":"boom","type":"server_error"}}"#));
        // 无 response.created/in_progress
        assert_eq!(out_types(&outs), vec!["error"]);
        let v: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["code"], "server_error");
        assert_eq!(v["message"], "boom");

        // 无 response.completed
        assert!(c.push(&sse("[DONE]")).is_empty());
        assert!(c.finish().is_empty());
    }

    #[test]
    fn stream_anthropic_error_event_to_openai_and_responses() {
        // Anthropic type:"error" 事件 → OpenAI 形状 error data
        let mut c = StreamConverter::new("openai", "anthropic", "m");
        let outs = c.push(&sse_typed(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ));
        assert_eq!(outs.len(), 1);
        let v: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(v["error"]["type"], "overloaded_error");
        assert_eq!(v["error"]["message"], "Overloaded");
        // 后续正常帧被忽略
        assert!(c
            .push(&sse_typed("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}"#))
            .is_empty());

        // 组合链:Anthropic error → Responses error 事件
        let mut c = StreamConverter::new("responses", "anthropic", "m");
        let outs = c.push(&sse_typed(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ));
        assert_eq!(out_types(&outs), vec!["error"]);
        let v: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["code"], "overloaded_error");
        assert_eq!(v["message"], "Overloaded");
        assert!(c.finish().is_empty());
    }

    // ---- 缓存计费与截断状态 ----

    #[test]
    fn stream_anthropic_to_responses_carries_cached_tokens() {
        // Anthropic 流式带 cache_read:经 chat chunk 的 prompt_tokens_details
        // 传给 OarStreamState,response.completed 里 cached_tokens 正确拆分。
        let mut c = StreamConverter::new("responses", "anthropic", "m");
        c.push(&sse_typed(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":9,"cache_creation_input_tokens":2,"cache_read_input_tokens":5}}}"#,
        ));
        c.push(&sse_typed("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#));
        c.push(&sse_typed("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#));

        // usage:(未缓存输入 9+2=11, 输出 3, 总 19, 缓存 5)
        assert_eq!(c.usage(), (11, 3, 19, 5));

        let outs = c.finish();
        let completed: Value = serde_json::from_str(&outs.last().unwrap().data).unwrap();
        assert_eq!(completed["type"], "response.completed");
        let usage = &completed["response"]["usage"];
        // 客户端可见 input_tokens 含缓存;cached_tokens 单独标出
        assert_eq!(usage["input_tokens"], 16);
        assert_eq!(usage["input_tokens_details"]["cached_tokens"], 5);
        assert_eq!(usage["output_tokens"], 3);
        assert_eq!(usage["total_tokens"], 19);
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

    #[test]
    fn response_openai_to_responses_incomplete_on_length() {
        let resp = json!({
            "id": "chatcmpl-1",
            "model": "gpt-5",
            "created": 1_700_000_000,
            "choices": [{"message": {"role": "assistant", "content": "partial"}, "finish_reason": "length"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14}
        });
        let out = convert_response(&resp, "openai", "responses", "gpt-5");
        assert_eq!(out["status"], "incomplete");
        assert_eq!(out["incomplete_details"]["reason"], "max_output_tokens");

        // content_filter 同理;正常 stop 仍是 completed 且 incomplete_details 为 null
        let mut resp_cf = resp.clone();
        resp_cf["choices"][0]["finish_reason"] = json!("content_filter");
        let out = convert_response(&resp_cf, "openai", "responses", "gpt-5");
        assert_eq!(out["status"], "incomplete");
        assert_eq!(out["incomplete_details"]["reason"], "content_filter");

        let mut resp_ok = resp.clone();
        resp_ok["choices"][0]["finish_reason"] = json!("stop");
        let out = convert_response(&resp_ok, "openai", "responses", "gpt-5");
        assert_eq!(out["status"], "completed");
        assert!(out["incomplete_details"].is_null());
    }

    #[test]
    fn stream_openai_to_responses_incomplete_on_length() {
        let mut c = StreamConverter::new("responses", "openai", "m");
        c.push(&sse(r#"{"choices":[{"delta":{"content":"partial"}}]}"#));
        c.push(&sse(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#));
        let outs = c.push(&sse("[DONE]"));
        let last: Value = serde_json::from_str(&outs.last().unwrap().data).unwrap();
        assert_eq!(last["type"], "response.incomplete");
        assert_eq!(outs.last().unwrap().event.as_deref(), Some("response.incomplete"));
        assert_eq!(last["response"]["status"], "incomplete");
        assert_eq!(
            last["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
    }

    #[test]
    fn usage_extraction_nonstandard_openai_not_misread_as_responses() {
        // 非标 OpenAI:缺 prompt_tokens/input_tokens 但带 total_tokens,
        // 不得误入 Responses 分支把输入读成 0 并照单全收 total;
        // 按 Anthropic 分支兜底(输入未知按 0,total 由分项推导)。
        let weird = json!({"usage": {"completion_tokens": 5, "total_tokens": 15}});
        assert_eq!(extract_usage_any(&weird), (0, 5, 5, 0));
        let only_total = json!({"usage": {"total_tokens": 10}});
        assert_eq!(extract_usage_any(&only_total), (0, 0, 0, 0));
        // 带 input_tokens 的 Responses 形状不受影响
        let responses = json!({"usage": {"input_tokens": 7, "output_tokens": 3, "total_tokens": 10}});
        assert_eq!(extract_usage_any(&responses), (7, 3, 10, 0));
    }
}
