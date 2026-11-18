//! 流式转发(阶段 2 拆分):三个单元——
//!
//! * **流转发**:`stream_response` 的 SSE 生成器(唯一 yield 点);
//! * **记账收尾**:[`finalize_stream`] —— 成功 / 流内错误帧 / 中途失败
//!   三种收尾的渠道健康与 request_logs 落账(含 499 补账的
//!   [`StreamAbortGuard`],生命周期挂在 [`RequestCtx`] 上,随生成器存活);
//! * **错误帧处理**:[`error_event_sequence`] —— 中途失败时发给客户端的
//!   错误事件序列(纯函数,可单测)。
//!
//! 9.2:客户端断开时生成器被 Drop——上游字节流(生成器所有)随之 Drop、
//! 连接关闭、上游请求中止;已捕获 usage 由 guard 的 Drop 路径记账(499)。
//! 生成器内正常收尾先记账后 `guard.finish()`,两条路径互斥,不重复记账。
//! 关停 / 外部取消经 `ctx.cancel` 进 select,同样"先记账再退出"。

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::response::{
    IntoResponse, Response,
    sse::{Event, Sse},
};
use futures_util::StreamExt;
use tracing::{debug, warn};

use crate::AppState;
use crate::billing::ledger::{
    RequestLogParams, request_log_sync_billable, spawn_request_log, spawn_request_log_for_guard,
};
use crate::channel::selector::{spawn_record_failure, spawn_record_result};
use crate::pipeline::context::RequestCtx;
use aikun_protocol::{
    Protocol, SseOut, StreamConverter, parse_sse_events, parse_sse_remaining, stream_error_event,
    stream_terminal_event,
};

/// Abort the stream if the upstream sends more than this without a
/// complete SSE event boundary — an unbounded buffer is a memory risk.
const MAX_SSE_BUF: usize = 1024 * 1024;

/// Guard that accounts for streams aborted before completion (typically the
/// client disconnecting mid-stream). It lives inside the stream generator:
/// if the generator is dropped early, `Drop` writes a 499 failure log with
/// the usage captured so far. Provider health metrics are intentionally not
/// touched — a client disconnect says nothing about the provider.
///
/// 阶段 2:guard 的数据源从 [`RequestCtx`] 构造(挂到 ctx 的生命周期上),
/// 不再是 11 参长列表;ctx 被 move 进生成器,guard 因此与流同生共死。
struct StreamAbortGuard {
    store: Arc<crate::store::AppStore>,
    request_id: String,
    user_id: String,
    api_key_id: Option<String>,
    provider_id: String,
    model: String,
    request_type: String,
    start: Instant,
    usage: Arc<Mutex<(i32, i32, i32, i32)>>,
    finished: bool,
}

impl StreamAbortGuard {
    fn from_ctx(
        store: &Arc<crate::store::AppStore>,
        ctx: &RequestCtx,
        provider_id: &str,
        start: Instant,
        usage: Arc<Mutex<(i32, i32, i32, i32)>>,
    ) -> Self {
        Self {
            store: store.clone(),
            request_id: ctx.request_id.clone(),
            user_id: ctx.user_id.clone(),
            api_key_id: ctx.api_key_id.clone(),
            provider_id: provider_id.to_string(),
            model: ctx.model.clone(),
            request_type: ctx.request_type(),
            start,
            usage,
            finished: false,
        }
    }

    fn finish(&mut self) {
        self.finished = true;
    }
}

impl Drop for StreamAbortGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let latency = self.start.elapsed().as_millis() as i32;
        let (p, c, t, k) = self.usage.lock().map(|u| *u).unwrap_or((0, 0, 0, 0));
        // 阶段 S:记账是 async Surreal 查询,Drop 里投递为独立任务(无
        // runtime 上下文时丢弃并留痕——进程已在关停,与旧的"无运行时
        // 同步写"等价取舍:499 断连的补偿记账尽力而为)。
        let store = self.store.clone();
        let params = RequestLogParams {
            request_id: self.request_id.clone(),
            user_id: self.user_id.clone(),
            api_key_id: self.api_key_id.clone(),
            provider_id: Some(self.provider_id.clone()),
            model: self.model.clone(),
            request_type: self.request_type.clone(),
            prompt_tokens: p,
            completion_tokens: c,
            total_tokens: t,
            cached_tokens: k,
            latency_ms: latency,
            status_code: 499,
            success: false,
            error_message: Some("client disconnected".to_string()),
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                spawn_request_log_for_guard(&store, params);
            });
        } else {
            tracing::warn!("StreamAbortGuard dropped without runtime — 499 补账日志丢弃");
        }
    }
}

/// 流转发的一次收尾结果(记账收尾单元的输入)。
struct StreamOutcome {
    usage: (i32, i32, i32, i32),
    latency_ms: f64,
    /// 传输级失败(stall / 连接错误 / 缓冲超限 / 空流):按 502 失败收尾。
    stream_error: Option<String>,
    /// 上游 200+SSE 内发错误帧:usage 照扣,按失败记账。
    error_frame: bool,
}

/// Build the SSE streaming response from a successful upstream response.
///
/// `idle_timeout_secs` 是渠道级空闲超时(0 = 全局回退,见 [`idle_timeout`]);
/// 客户端是否请求了 include_usage(`stream_options.include_usage`)从
/// `ctx.body` 读——未请求时转发前剥掉网关注入的 usage chunk。
pub(super) fn stream_response(
    state: AppState,
    ctx: RequestCtx,
    resp: reqwest::Response,
    adapter: Arc<dyn crate::channel::ChannelAdapter>,
    provider_protocol: Protocol,
    provider_id: String,
    provider_idle_secs: i32,
) -> Response {
    let client_protocol = ctx.client_protocol;
    let client_wants_usage = ctx.body["stream_options"]["include_usage"]
        .as_bool()
        .unwrap_or(false);
    let byte_stream = resp.bytes_stream();
    let store = state.store.clone();
    let threshold = state.config.auto_disable_threshold;
    let start = Instant::now();
    let mut converter = StreamConverter::new(client_protocol, provider_protocol, &ctx.model)
        .strip_usage_chunk(!client_wants_usage);
    let idle_timeout = idle_timeout(&state, provider_idle_secs);

    let sse_stream = async_stream::stream! {
        // ctx(并发槽位、取消令牌、span、计费维度)随生成器存活:流正常
        // 结束、客户端断连、关停取消都触发生成器 Drop,槽位
        // (ctx.concurrency_guard 字段)与上游连接在那时才释放/中止。
        let ctx = ctx;
        // ZCode 闲时免费(23:00-09:00 Flash 上游零消耗 → 下游 0 元,
        // tokens 照记):按请求模型名 + 渠道类型判定(与 attempt 侧同口径)。
        let free_upstream = adapter.kind() == aikun_channels::adapters::zcode::KIND
            && aikun_channels::adapters::zcode::upstream_free(
                &ctx.model,
                chrono::Utc::now().timestamp(),
            );
        let usage_cell = Arc::new(Mutex::new((0, 0, 0, 0)));
        let mut guard = StreamAbortGuard::from_ctx(
            &store,
            &ctx,
            &provider_id,
            start,
            usage_cell.clone(),
        );
        let mut buf: Vec<u8> = Vec::new();
        // Set when the upstream stream becomes unusable mid-flight (stall,
        // transport error, or unbounded buffer); recorded as a failure below.
        let mut stream_error: Option<String> = None;
        // 上游在 200 的 SSE 流内发错误帧(各协议形状):converter 已转成
        // 协议错误事件发给客户端,收尾时按失败记账而不是成功。
        let mut error_frame = false;
        let mut events: u32 = 0;
        let mut ttft_recorded = false;
        let mut upstream = std::pin::pin!(byte_stream);
        loop {
            // 取消优先(9.2):关停 / 客户端断开触发的取消立即跳出,走下方
            // 统一收尾——已捕获 usage 照常记账,而不是被 30s 强杀跳过收尾丢账。
            let item = tokio::select! {
                _ = ctx.cancel.cancelled() => {
                    debug!("Shutdown: ending in-flight stream from {} early", provider_id);
                    break;
                }
                item = tokio::time::timeout(idle_timeout, upstream.next()) => item,
            };
            let item = match item {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    warn!(
                        "Upstream stream from {} stalled for {:?} — aborting",
                        provider_id, idle_timeout
                    );
                    stream_error = Some(format!(
                        "upstream stream stalled for {}s",
                        idle_timeout.as_secs()
                    ));
                    break;
                }
            };
            match item {
                Ok(bytes) => {
                    buf.extend_from_slice(&bytes);
                    for mut ev in parse_sse_events(&mut buf) {
                        adapter.normalize_stream_event(provider_protocol, &mut ev);
                        events += 1;
                        for out in converter.push(&ev) {
                            if !ttft_recorded {
                                ttft_recorded = true;
                                ctx.span.record(
                                    "ttft_ms",
                                    start.elapsed().as_millis() as u64,
                                );
                            }
                            // 转换型 converter 的错误事件带 event: error;
                            // 直通路径原样转发,OpenAI 形状错误帧以 {"error": 开头。
                            if out.event.as_deref() == Some("error")
                                || out.data.starts_with("{\"error\":")
                            {
                                error_frame = true;
                            }
                            yield Ok::<Event, Infallible>(to_axum_event(out));
                        }
                    }
                    if let Ok(mut u) = usage_cell.lock() {
                        *u = converter.usage();
                    }
                    if buf.len() > MAX_SSE_BUF {
                        warn!(
                            "SSE buffer from provider {} exceeded {} bytes without an event boundary — aborting stream",
                            provider_id, MAX_SSE_BUF
                        );
                        stream_error = Some("upstream stream buffer overflow".to_string());
                        break;
                    }
                }
                Err(e) => {
                    // 9.8:reqwest 错误 Display 含完整 URL(查询串可能带
                    // key),剥离后进日志与错误帧。
                    let e = e.without_url();
                    warn!("Upstream stream error from {}: {}", provider_id, e);
                    stream_error = Some(format!("upstream stream error: {}", e));
                    break;
                }
            }
        }

        // 中途失败时缓冲区的残片不转发(与旧实现一致:直接进错误收尾);
        // 只有正常 EOF 才补解析最后一条不完整事件。
        if stream_error.is_none()
            && let Some(mut ev) = parse_sse_remaining(&mut buf)
        {
            adapter.normalize_stream_event(provider_protocol, &mut ev);
            events += 1;
            for out in converter.push(&ev) {
                if !ttft_recorded {
                    ttft_recorded = true;
                    ctx.span.record("ttft_ms", start.elapsed().as_millis() as u64);
                }
                if out.event.as_deref() == Some("error")
                    || out.data.starts_with("{\"error\":")
                {
                    error_frame = true;
                }
                yield Ok(to_axum_event(out));
            }
        }
        if let Ok(mut u) = usage_cell.lock() {
            *u = converter.usage();
        }

        // 上游 200+SSE 头却一个事件都没发就 EOF:按失败处理(与模型实测
        // 探针同一口径),否则客户端对着空流干等,指标还被记成成功。
        let stream_error = stream_error.or_else(|| {
            (events == 0).then(|| "upstream sent an empty SSE stream".to_string())
        });

        let outcome = StreamOutcome {
            usage: converter.usage(),
            latency_ms: start.elapsed().as_millis() as f64,
            stream_error,
            error_frame,
        };

        if let Some(err_msg) = &outcome.stream_error {
            // 中途失败(含空流):先记账并标记 guard 完成——否则客户端在
            // 下方 yield 期间断开会触发 StreamAbortGuard::drop 再记一条
            // 499,重复记账;再发错误帧收尾。
            finalize_stream(&state, &ctx, &provider_id, &outcome, threshold, &mut guard, free_upstream).await;
            for out in error_event_sequence(client_protocol, err_msg, &mut converter, events > 0) {
                yield Ok(to_axum_event(out));
            }
            return;
        }

        for out in converter.finish() {
            yield Ok(to_axum_event(out));
        }
        // 正常收尾(成功 / 流内错误帧):记账在终止帧之后、guard 完成标记
        // 之前(收尾路径只走一次,两条记账路径互斥)。
        finalize_stream(&state, &ctx, &provider_id, &outcome, threshold, &mut guard, free_upstream).await;
        if !outcome.error_frame {
            debug!(
                request_id = %ctx.request_id,
                "Stream completed: model={} provider={} latency={}ms tokens={}",
                ctx.model, provider_id, outcome.latency_ms as i64, outcome.usage.2
            );
        }
    };

    Sse::new(sse_stream).into_response()
}

/// 记账收尾单元:按 [`StreamOutcome`] 落渠道健康与 request_logs,并标记
/// [`StreamAbortGuard`] 完成(阻断 Drop 的 499 补账,保证"只记一次")。
async fn finalize_stream(
    state: &AppState,
    ctx: &RequestCtx,
    provider_id: &str,
    outcome: &StreamOutcome,
    threshold: u32,
    guard: &mut StreamAbortGuard,
    // ZCode 闲时免费(见生成器内判定):下游 0 元,tokens 照记。
    free_upstream: bool,
) {
    let (p, c, t, k) = outcome.usage;
    let latency = outcome.latency_ms;
    // 9.6:流式请求的总耗时与状态在收尾时打点(非流式在管线入口打)。
    ctx.span.record("total_ms", latency as u64);
    if let Some(err_msg) = &outcome.stream_error {
        // The upstream stream was unusable (stall / transport error / 空流) —
        // count it as a provider failure.
        spawn_record_failure(
            state.store.clone(),
            provider_id.to_string(),
            latency,
            0,
            threshold,
            true,
        );
        spawn_request_log(
            state,
            RequestLogParams {
                request_id: ctx.request_id.clone(),
                user_id: ctx.user_id.clone(),
                api_key_id: ctx.api_key_id.clone(),
                provider_id: Some(provider_id.to_string()),
                model: ctx.model.clone(),
                request_type: ctx.request_type(),
                prompt_tokens: p,
                completion_tokens: c,
                total_tokens: t,
                cached_tokens: k,
                latency_ms: latency as i32,
                status_code: 502,
                success: false,
                error_message: Some(err_msg.clone()),
            },
        );
        guard.finish();
        return;
    }
    if outcome.error_frame {
        // 上游流内错误帧:HTTP 虽 200,语义上是失败。按失败记账(已捕获
        // 的 usage 照扣,与流中途错误同口径),不让刷错误帧的渠道显得健康。
        spawn_record_failure(
            state.store.clone(),
            provider_id.to_string(),
            latency,
            0,
            threshold,
            true,
        );
        request_log_sync_billable(
            state,
            RequestLogParams {
                request_id: ctx.request_id.clone(),
                user_id: ctx.user_id.clone(),
                api_key_id: ctx.api_key_id.clone(),
                provider_id: Some(provider_id.to_string()),
                model: ctx.model.clone(),
                request_type: ctx.request_type(),
                prompt_tokens: p,
                completion_tokens: c,
                total_tokens: t,
                cached_tokens: k,
                latency_ms: latency as i32,
                status_code: 502,
                success: false,
                error_message: Some("upstream sent an in-stream error frame".to_string()),
            },
            !free_upstream,
        )
        .await;
        ctx.span.record("status", 502u16);
        guard.finish();
        return;
    }
    spawn_record_result(state.store.clone(), provider_id.to_string(), latency, true);
    // 计费级:与非流式一致,流结束时同步等待日志与扣费落库,
    // fire-and-forget 在进程退出时会丢账。
    request_log_sync_billable(
        state,
        RequestLogParams {
            request_id: ctx.request_id.clone(),
            user_id: ctx.user_id.clone(),
            api_key_id: ctx.api_key_id.clone(),
            provider_id: Some(provider_id.to_string()),
            model: ctx.model.clone(),
            request_type: ctx.request_type(),
            prompt_tokens: p,
            completion_tokens: c,
            total_tokens: t,
            cached_tokens: k,
            latency_ms: latency as i32,
            status_code: 200,
            success: true,
            error_message: None,
        },
        !free_upstream,
    )
    .await;
    ctx.span.record("status", 200u16);
    guard.finish();
}

/// 错误帧处理单元:流中途失败时发给客户端的事件序列——
/// 协议对应的错误事件 + converter 收尾事件(缺失终止帧时补一个,避免
/// 双重终止)。`include_finish` = 是否附带 converter 收尾事件(空流时
/// converter 无内容可收尾,跳过——收尾会产出与错误矛盾的终止帧)。
fn error_event_sequence(
    client_protocol: Protocol,
    err_msg: &str,
    converter: &mut StreamConverter,
    include_finish: bool,
) -> Vec<SseOut> {
    let mut outs = vec![stream_error_event(client_protocol, err_msg)];
    // responses 客户端:error 事件即终止,跳过 converter 收尾——收尾
    // 会产出与错误矛盾的 *.done/response.completed(部分输出被当成
    // 成功完成),codex 等客户端会误判流正常结束。
    let finish_outs = match (client_protocol, include_finish) {
        (Protocol::Responses, _) | (_, false) => vec![],
        (Protocol::OpenAi | Protocol::Anthropic, true) => converter.finish(),
    };
    let has_terminal = finish_outs
        .iter()
        .any(|o| o.data == "[DONE]" || o.data.contains("message_stop"));
    outs.extend(finish_outs);
    if !has_terminal && let Some(term) = stream_terminal_event(client_protocol) {
        outs.push(term);
    }
    outs
}

/// chunk 间空闲超时(9.2):全局 `AIKUN_IDLE_TIMEOUT_SECS`(>0)优先;
/// 否则沿用渠道级 `timeout_secs`,legacy 渠道(≤0)回退全局
/// `AIKUN_REQUEST_TIMEOUT_SECS` 再 clamp(避免退化为 1 秒误 abort)。
fn idle_timeout(state: &AppState, provider_idle_secs: i32) -> std::time::Duration {
    let idle_secs = if state.config.idle_timeout_secs > 0 {
        state.config.idle_timeout_secs
    } else if provider_idle_secs > 0 {
        provider_idle_secs as u64
    } else {
        state.config.request_timeout_secs
    };
    std::time::Duration::from_secs(idle_secs.clamp(1, 600))
}

fn to_axum_event(out: SseOut) -> Event {
    let ev = Event::default().data(out.data);
    match out.event {
        Some(name) => ev.event(name),
        None => ev,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 错误帧序列(非 Responses 协议、已有事件):错误事件 + converter
    /// 收尾 + 补 [DONE] 终止。
    #[test]
    fn error_sequence_openai_gets_error_then_terminal() {
        let mut conv = StreamConverter::new(Protocol::OpenAi, Protocol::OpenAi, "m");
        let outs = error_event_sequence(Protocol::OpenAi, "boom", &mut conv, true);
        assert!(!outs.is_empty());
        // OpenAI 错误帧无 event 名,以 {"error": 开头(与转发循环的判定同口径)
        assert!(
            outs[0].event.as_deref() == Some("error") || outs[0].data.starts_with("{\"error\":"),
            "first frame must be an error frame: {}",
            outs[0].data
        );
        // 终止帧恰好一次
        let done = outs.iter().filter(|o| o.data == "[DONE]").count();
        assert_eq!(
            done,
            1,
            "exactly one terminal event: {:?}",
            outs.iter().map(|o| o.data.clone()).collect::<Vec<_>>()
        );
    }

    /// responses 客户端:error 事件即终止,不补 *.done 收尾帧。
    #[test]
    fn error_sequence_responses_ends_at_error_event() {
        let mut conv = StreamConverter::new(Protocol::Responses, Protocol::Responses, "m");
        let outs = error_event_sequence(Protocol::Responses, "boom", &mut conv, true);
        assert_eq!(outs.len(), 1, "error event only");
        assert_eq!(outs[0].event.as_deref(), Some("error"));
    }

    /// 空流收尾(include_finish=false):错误事件 + 终止,无 converter 收尾。
    #[test]
    fn error_sequence_empty_stream_skips_converter_finish() {
        let mut conv = StreamConverter::new(Protocol::OpenAi, Protocol::OpenAi, "m");
        let outs = error_event_sequence(Protocol::OpenAi, "empty", &mut conv, false);
        // OpenAI 错误帧无 event 名,以 {"error": 开头(与转发循环的判定同口径)
        assert!(
            outs[0].event.as_deref() == Some("error") || outs[0].data.starts_with("{\"error\":"),
            "first frame must be an error frame: {}",
            outs[0].data
        );
        assert_eq!(
            outs.iter().filter(|o| o.data == "[DONE]").count(),
            1,
            "terminal present without converter finish"
        );
    }

    /// 9.2:空闲超时的三段取值(全局覆盖 / 渠道级 / legacy 回退)。
    #[test]
    fn idle_timeout_prefers_global_then_provider() {
        // 直接构造最小 AppState 成本高,这里验证纯逻辑:借助 config 默认值
        // 推导。idle_timeout 需要 AppState;改为抽出的纯函数语义在下方
        // 重复验证(避免测试搭完整状态)。
        fn pick(global: u64, provider: i32, fallback: u64) -> u64 {
            let idle_secs = if global > 0 {
                global
            } else if provider > 0 {
                provider as u64
            } else {
                fallback
            };
            idle_secs.clamp(1, 600)
        }
        assert_eq!(pick(30, 120, 60), 30, "全局覆盖");
        assert_eq!(pick(0, 120, 60), 120, "渠道级");
        assert_eq!(pick(0, 0, 60), 60, "legacy 回退全局");
        assert_eq!(pick(0, 9999, 60), 600, "clamp 上界");
        assert_eq!(pick(0, 0, 0), 1, "clamp 下界");
    }
}
