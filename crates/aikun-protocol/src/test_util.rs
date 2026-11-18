//! 协议层单测共用的小工具。

use serde_json::Value;

use crate::{SseEvent, SseOut};

pub(crate) fn sse(data: &str) -> SseEvent {
    SseEvent {
        event: None,
        data: data.to_string(),
    }
}

pub(crate) fn sse_typed(event: &str, data: &str) -> SseEvent {
    SseEvent {
        event: Some(event.to_string()),
        data: data.to_string(),
    }
}

pub(crate) fn out_types(outs: &[SseOut]) -> Vec<String> {
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
