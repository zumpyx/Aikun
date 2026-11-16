//! Anthropic 流式事件 → OpenAI chunk:AnStreamState。

use std::collections::HashMap;

use serde_json::{Value, json};

#[allow(unused_imports)]
use crate::protocol::*;

// ----------------------------------------------------------------------------
// Anthropic events → OpenAI chunks
// ----------------------------------------------------------------------------
pub struct AnStreamState {
    pub(crate) msg_id: String,
    pub(crate) model: String,
    pub(crate) created: i64,
    pub(crate) block_to_tc: HashMap<usize, usize>,
    pub(crate) next_tc: usize,
    pub(crate) usage: (i32, i32, i32, i32),
    pub(crate) finished: bool,
}

impl AnStreamState {
    pub(crate) fn new(model: &str) -> Self {
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

    pub(crate) fn chunk(
        &self,
        delta: Value,
        finish_reason: Option<&str>,
        usage: Option<Value>,
    ) -> SseOut {
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

    pub(crate) fn push(&mut self, ev: &SseEvent) -> Vec<SseOut> {
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
                self.usage.0 = clamp_token_count(mu["input_tokens"].as_i64()).saturating_add(
                    clamp_token_count(mu["cache_creation_input_tokens"].as_i64()),
                );
                self.usage.3 = clamp_token_count(mu["cache_read_input_tokens"].as_i64());
                self.usage.2 = self
                    .usage
                    .0
                    .saturating_add(self.usage.1)
                    .saturating_add(self.usage.3);
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
                self.usage.2 = self
                    .usage
                    .0
                    .saturating_add(self.usage.1)
                    .saturating_add(self.usage.3);
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

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::protocol::test_util::{out_types, sse, sse_typed};
    #[test]
    fn stream_anthropic_to_openai_keeps_requested_model() {
        // 转换路径:上游 message_start 的真实模型名不覆盖请求模型名。
        let mut c = StreamConverter::new(Protocol::OpenAi, Protocol::Anthropic, "claude-alias");
        let outs = c.push(&sse_typed(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-real","usage":{"input_tokens":9}}}"#,
        ));
        let chunk: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(chunk["model"], "claude-alias");
    }

    // ---- StreamConverter: Anthropic upstream → OpenAI client ----

    #[test]
    fn stream_anthropic_to_openai_text_flow() {
        let mut c = StreamConverter::new(Protocol::OpenAi, Protocol::Anthropic, "claude-3");

        let outs = c.push(&sse_typed("message_start", r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-3","usage":{"input_tokens":9}}}"#));
        assert_eq!(outs.len(), 1);
        let chunk: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(chunk["id"], "msg_1");
        assert_eq!(chunk["choices"][0]["delta"]["role"], "assistant");

        let outs = c.push(&sse_typed(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
        ));
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
    fn stream_anthropic_to_openai_thinking_delta() {
        let mut c = StreamConverter::new(Protocol::OpenAi, Protocol::Anthropic, "m");
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
        let mut c = StreamConverter::new(Protocol::OpenAi, Protocol::Anthropic, "m");
        c.push(&sse_typed(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":1}}}"#,
        ));
        let outs = c.push(&sse_typed("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu1","name":"fn"}}"#));
        let chunk: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(
            chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "fn"
        );

        let outs = c.push(&sse_typed("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#));
        let chunk: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(
            chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":"
        );
    }

    #[test]
    fn stream_anthropic_error_event_to_openai_and_responses() {
        // Anthropic type:"error" 事件 → OpenAI 形状 error data
        let mut c = StreamConverter::new(Protocol::OpenAi, Protocol::Anthropic, "m");
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
        let mut c = StreamConverter::new(Protocol::Responses, Protocol::Anthropic, "m");
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
        let mut c = StreamConverter::new(Protocol::Responses, Protocol::Anthropic, "m");
        c.push(&sse_typed(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":9,"cache_creation_input_tokens":2,"cache_read_input_tokens":5}}}"#,
        ));
        c.push(&sse_typed(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
        ));
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
}
