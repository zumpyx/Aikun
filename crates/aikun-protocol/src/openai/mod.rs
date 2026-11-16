//! OpenAI Chat Completions(星型枢纽形态):共享的内容块工具函数。

pub mod request;
pub mod response;
pub mod stream;

use serde_json::Value;

#[allow(unused_imports)]
use crate::*;

/// Anthropic / 中转站用的 cache_control 是对象。其它形状不转发。
pub fn cache_control_of(v: &Value) -> Option<&Value> {
    v.get("cache_control").filter(|c| c.is_object())
}

pub fn with_cache_control(mut block: Value, src: &Value) -> Value {
    if let Some(cc) = cache_control_of(src)
        && let Some(obj) = block.as_object_mut()
    {
        obj.insert("cache_control".into(), cc.clone());
    }
    block
}

/// Extract plain text from an OpenAI message content field (string or parts).
pub fn openai_content_text(content: &Value) -> String {
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
