//! 流式转换调度:按 (客户端协议, 上游协议) 组合各协议的流式状态机。


use serde_json::{json, Value};

#[allow(unused_imports)]
use crate::protocol::*;

/// An outbound SSE event ready to send to the client.
pub struct SseOut {
    pub event: Option<String>,
    pub data: String,
}

impl SseOut {
    pub(crate) fn json(event: &str, value: Value) -> Self {
        Self {
            event: Some(event.to_string()),
            data: value.to_string(),
        }
    }

    pub(crate) fn data_only(data: String) -> Self {
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
    pub fn new(client_protocol: Protocol, provider_protocol: Protocol, model: &str) -> Self {
        use Protocol::*;
        match (client_protocol, provider_protocol) {
            (OpenAi, OpenAi) => StreamConverter::PassOpenAi {
                usage: (0, 0, 0, 0),
                strip_usage_chunk: false,
                model: model.to_string(),
            },
            (Anthropic, Anthropic) => {
                StreamConverter::PassAnthropic { usage: (0, 0, 0, 0), model: model.to_string() }
            }
            (Anthropic, OpenAi) => StreamConverter::OpenAiToAnthropic(OaStreamState::new(model)),
            (OpenAi, Anthropic) => StreamConverter::AnthropicToOpenAi(AnStreamState::new(model)),
            (Responses, Responses) => {
                StreamConverter::PassResponses { usage: (0, 0, 0, 0), model: model.to_string() }
            }
            (Responses, OpenAi) => StreamConverter::OpenAiToResponses(OarStreamState::new(model)),
            (Responses, Anthropic) => StreamConverter::AnthropicToResponses {
                an: AnStreamState::new(model),
                oar: OarStreamState::new(model),
            },
            // Responses 上游只服务 Responses 客户端(models::upstream_protocol_for),
            // 以下组合不会被路由选中;显式列出保持穷举,行为同 0.4(按 OpenAI 直通)。
            (OpenAi, Responses) | (Anthropic, Responses) => StreamConverter::PassOpenAi {
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


#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::protocol::test_util::{out_types, sse, sse_typed};
    // ---- StreamConverter: pass-through ----

    #[test]
    fn stream_pass_openai_captures_usage() {
        let mut c = StreamConverter::new(Protocol::OpenAi, Protocol::OpenAi, "m");
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
        let mut c = StreamConverter::new(Protocol::OpenAi, Protocol::OpenAi, "m").strip_usage_chunk(true);
        let outs = c.push(&sse(r#"{"choices":[{"delta":{"content":"x"}}]}"#));
        assert_eq!(outs.len(), 1);
        let outs = c.push(&sse(r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#));
        assert!(outs.is_empty(), "usage-only chunk must not reach the client");
        assert_eq!(c.usage(), (1, 2, 3, 0));
    }

    #[test]
    fn stream_pass_anthropic_captures_usage() {
        let mut c = StreamConverter::new(Protocol::Anthropic, Protocol::Anthropic, "m");
        c.push(&sse_typed("message_start", r#"{"type":"message_start","message":{"usage":{"input_tokens":7}}}"#));
        assert_eq!(c.usage(), (7, 0, 7, 0));
        c.push(&sse_typed("message_delta", r#"{"type":"message_delta","usage":{"output_tokens":4}}"#));
        assert_eq!(c.usage(), (7, 4, 11, 0));
    }

    #[test]
    fn stream_pass_anthropic_splits_cache_usage() {
        // message_start 携带缓存字段:cache_creation 并入未缓存输入,
        // cache_read 记入缓存维度;total = 三者之和
        let mut c = StreamConverter::new(Protocol::Anthropic, Protocol::Anthropic, "m");
        c.push(&sse_typed("message_start", r#"{"type":"message_start","message":{"usage":{"input_tokens":7,"cache_creation_input_tokens":3,"cache_read_input_tokens":5}}}"#));
        assert_eq!(c.usage(), (10, 0, 15, 5));
        c.push(&sse_typed("message_delta", r#"{"type":"message_delta","usage":{"output_tokens":4}}"#));
        assert_eq!(c.usage(), (10, 4, 19, 5));
    }

    #[test]
    fn stream_pass_openai_rewrites_chunk_model() {
        // 映射场景:chunk 里的上游真实模型名回写为请求模型名。
        let mut c = StreamConverter::new(Protocol::OpenAi, Protocol::OpenAi, "gpt-4");
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
        let mut c = StreamConverter::new(Protocol::Anthropic, Protocol::Anthropic, "claude-alias");
        let outs = c.push(&sse_typed(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-real","usage":{"input_tokens":7}}}"#,
        ));
        let data: Value = serde_json::from_str(&outs[0].data).unwrap();
        assert_eq!(data["message"]["model"], "claude-alias");
        assert_eq!(c.usage(), (7, 0, 7, 0));
    }

    #[test]
    fn stream_converters_ignore_garbage() {
        let mut c1 = StreamConverter::new(Protocol::Anthropic, Protocol::OpenAi, "m");
        assert!(c1.push(&sse("not json")).is_empty());
        let mut c2 = StreamConverter::new(Protocol::OpenAi, Protocol::Anthropic, "m");
        assert!(c2.push(&sse_typed("ping", "not json")).is_empty());
        // Unstarted converter finalizes to nothing.
        let mut c3 = StreamConverter::new(Protocol::Anthropic, Protocol::OpenAi, "m");
        assert!(c3.push(&sse("[DONE]")).is_empty());
    }

    // ---- StreamConverter: Responses 客户端 ----

    #[test]
    fn stream_pass_responses_captures_usage_and_rewrites_model() {
        let mut c = StreamConverter::new(Protocol::Responses, Protocol::Responses, "client-model");
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
    fn stream_anthropic_to_responses_composition() {
        let mut c = StreamConverter::new(Protocol::Responses, Protocol::Anthropic, "claude-4");
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
}
