//! OpenAI 流式 chunk → 其他协议事件:OaStreamState(→ Anthropic)、OarStreamState(→ Responses)。

use std::collections::HashMap;

use serde_json::{json, Value};

#[allow(unused_imports)]
use crate::protocol::*;

// ----------------------------------------------------------------------------
// OpenAI chunks → Anthropic events
// ----------------------------------------------------------------------------
pub struct OaStreamState {
    pub(crate) msg_id: String,
    pub(crate) model: String,
    pub(crate) started: bool,
    pub(crate) finished: bool,
    pub(crate) next_block: usize,
    pub(crate) text_block: Option<usize>,
    /// 当前打开的 thinking 块。推理增量通常在正文之前到达,正文或工具
    /// 开始前由 close_open_block 关掉。
    pub(crate) thinking_block: Option<usize>,
    pub(crate) tool_blocks: HashMap<u64, usize>,
    pub(crate) open_tool_block: Option<usize>,
    pub(crate) stop_reason: Option<&'static str>,
    pub(crate) usage: (i32, i32, i32, i32),
}

impl OaStreamState {
    pub(crate) fn new(model: &str) -> Self {
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

    pub(crate) fn push(&mut self, ev: &SseEvent) -> Vec<SseOut> {
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

    pub(crate) fn ensure_thinking_block(&mut self, out: &mut Vec<SseOut>) -> usize {
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

    pub(crate) fn ensure_text_block(&mut self, out: &mut Vec<SseOut>) -> usize {
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
    pub(crate) fn close_open_block(&mut self, out: &mut Vec<SseOut>) {
        self.close_thinking_block(out);
        self.close_text_block(out);
        if let Some(idx) = self.open_tool_block.take() {
            out.push(SseOut::json(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": idx}),
            ));
        }
    }

    pub(crate) fn close_thinking_block(&mut self, out: &mut Vec<SseOut>) {
        if let Some(idx) = self.thinking_block.take() {
            out.push(SseOut::json(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": idx}),
            ));
        }
    }

    pub(crate) fn close_text_block(&mut self, out: &mut Vec<SseOut>) {
        if let Some(idx) = self.text_block.take() {
            out.push(SseOut::json(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": idx}),
            ));
        }
    }

    /// usage 4 元组 → Anthropic message_delta.usage。
    pub(crate) fn anthropic_usage(&self) -> Value {
        let mut u = json!({
            "input_tokens": self.usage.0,
            "output_tokens": self.usage.1,
        });
        if self.usage.3 > 0 {
            u["cache_read_input_tokens"] = json!(self.usage.3);
        }
        u
    }

    pub(crate) fn finalize(&mut self) -> Vec<SseOut> {
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
                // usage.0 已是未缓存输入;缓存命中单列 cache_read_input_tokens
                // (bug ① 修复,与非流式口径一致)。
                "usage": self.anthropic_usage(),
            }),
        ));
        out.push(SseOut::json("message_stop", json!({"type": "message_stop"})));
        out
    }
}

// ----------------------------------------------------------------------------
// OpenAI chat chunks → Responses API events
// ----------------------------------------------------------------------------
/// One in-flight function_call output item being assembled from chat
/// tool_call deltas.
pub(crate) struct OarToolItem {
    pub(crate) output_index: usize,
    pub(crate) item_id: String,
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

pub struct OarStreamState {
    pub(crate) resp_id: String,
    pub(crate) msg_item_id: String,
    pub(crate) model: String,
    pub(crate) created: i64,
    pub(crate) started: bool,
    pub(crate) finished: bool,
    /// 文本 output item 的 output_index;None 表示尚未开启。
    pub(crate) text_index: Option<usize>,
    pub(crate) text: String,
    /// chat tool_call index → output item。
    pub(crate) tool_items: HashMap<u64, OarToolItem>,
    pub(crate) next_output_index: usize,
    pub(crate) usage: (i32, i32, i32, i32),
    /// 最终非空 finish_reason:finalize 据此判断 completed 还是 incomplete。
    pub(crate) finish_reason: Option<String>,
}

impl OarStreamState {
    pub(crate) fn new(model: &str) -> Self {
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
    pub(crate) fn responses_usage(&self) -> Value {
        json!({
            "input_tokens": self.usage.0.saturating_add(self.usage.3),
            "input_tokens_details": {"cached_tokens": self.usage.3},
            "output_tokens": self.usage.1,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": self.usage.2,
        })
    }

    pub(crate) fn push(&mut self, ev: &SseEvent) -> Vec<SseOut> {
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
    pub(crate) fn ensure_text_item(&mut self, out: &mut Vec<SseOut>) {
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
    pub(crate) fn completed_output(&self) -> Vec<Value> {
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

    pub(crate) fn finalize(&mut self) -> Vec<SseOut> {
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
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::protocol::test_util::{out_types, sse, sse_typed};
    // ---- StreamConverter: OpenAI upstream → Anthropic client ----

    #[test]
    fn stream_openai_to_anthropic_text_flow() {
        let mut c = StreamConverter::new(Protocol::Anthropic, Protocol::OpenAi, "claude-x");

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
    fn stream_openai_to_anthropic_splits_cached_tokens() {
        // 回归(bug ①):流式 O→A 收尾的 message_delta.usage 也要拆出缓存维度。
        let mut c = StreamConverter::new(Protocol::Anthropic, Protocol::OpenAi, "m");
        c.push(&sse(r#"{"id":"c","choices":[{"delta":{"content":"x"}}]}"#));
        c.push(&sse(r#"{"id":"c","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14,"prompt_tokens_details":{"cached_tokens":6}}}"#));
        let outs = c.push(&sse("[DONE]"));
        let delta: Value = serde_json::from_str(
            &outs.iter().find(|o| o.event.as_deref() == Some("message_delta")).unwrap().data,
        )
        .unwrap();
        assert_eq!(delta["usage"]["input_tokens"], 4);
        assert_eq!(delta["usage"]["cache_read_input_tokens"], 6);
        assert_eq!(delta["usage"]["output_tokens"], 4);
        assert_eq!(c.usage(), (4, 4, 14, 6));
    }

    #[test]
    fn stream_openai_to_anthropic_finish_reason_mapping() {
        let mut c = StreamConverter::new(Protocol::Anthropic, Protocol::OpenAi, "m");
        c.push(&sse(r#"{"choices":[{"delta":{"content":"x"}}]}"#));
        c.push(&sse(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#));
        let outs = c.push(&sse("[DONE]"));
        let delta: Value = serde_json::from_str(&outs[1].data).unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "max_tokens");
    }

    #[test]
    fn stream_openai_to_anthropic_closes_block_before_new_one() {
        let mut c = StreamConverter::new(Protocol::Anthropic, Protocol::OpenAi, "m");

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
        let mut c = StreamConverter::new(Protocol::Anthropic, Protocol::OpenAi, "m");
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

    #[test]
    fn stream_openai_to_anthropic_reasoning_before_text() {
        let mut c = StreamConverter::new(Protocol::Anthropic, Protocol::OpenAi, "m");
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
    fn stream_openai_to_responses_text_flow() {
        let mut c = StreamConverter::new(Protocol::Responses, Protocol::OpenAi, "gpt-5");

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
        let mut c = StreamConverter::new(Protocol::Responses, Protocol::OpenAi, "m");
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
        let mut c = StreamConverter::new(Protocol::Responses, Protocol::OpenAi, "m");
        c.push(&sse(r#"{"choices":[{"delta":{"content":"x"}}]}"#));
        let outs = c.finish();
        assert_eq!(
            out_types(&outs),
            vec!["response.output_text.done", "response.content_part.done", "response.output_item.done", "response.completed"]
        );
    }

    // ---- 流内错误帧 ----

    #[test]
    fn stream_openai_error_chunk_to_anthropic_error_event() {
        // 中转上游 HTTP 200 + SSE 流内错误帧:不得先 message_start 再正常收尾
        let mut c = StreamConverter::new(Protocol::Anthropic, Protocol::OpenAi, "m");
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
    fn stream_openai_error_chunk_mid_stream_to_anthropic() {
        // 流中途出错:已 message_start,错误事件后不再 message_delta/message_stop
        let mut c = StreamConverter::new(Protocol::Anthropic, Protocol::OpenAi, "m");
        let outs = c.push(&sse(r#"{"id":"c1","choices":[{"delta":{"content":"Hel"}}]}"#));
        assert!(out_types(&outs).contains(&"message_start".to_string()));
        let outs = c.push(&sse(r#"{"error":{"message":"mid boom","type":"server_error"}}"#));
        assert_eq!(out_types(&outs), vec!["error"]);
        assert!(c.push(&sse("[DONE]")).is_empty());
    }

    #[test]
    fn stream_openai_error_chunk_to_responses_error_event() {
        let mut c = StreamConverter::new(Protocol::Responses, Protocol::OpenAi, "m");
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
    fn stream_openai_to_responses_incomplete_on_length() {
        let mut c = StreamConverter::new(Protocol::Responses, Protocol::OpenAi, "m");
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
}
