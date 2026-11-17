//! 流式转发(原 stream_response):SSE 转发循环、记账收尾、错误帧处理。

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
use crate::billing::ledger::{request_log_sync, spawn_request_log};
use crate::channel::selector::{spawn_record_failure, spawn_record_result};
use crate::pipeline::admission::ConcurrencyGuard;
use aikun_protocol::{
    Protocol, SseOut, StreamConverter, parse_sse_events, parse_sse_remaining, stream_error_event,
    stream_terminal_event,
};

/// Guard that accounts for streams aborted before completion (typically the
/// client disconnecting mid-stream). It lives inside the stream generator:
/// if the generator is dropped early, `Drop` writes a failure log with the
/// usage captured so far. Provider health metrics are intentionally not
/// touched — a client disconnect says nothing about the provider.
struct StreamAbortGuard {
    store: Arc<crate::store::AppStore>,
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
        let user_id = self.user_id.clone();
        let api_key_id = self.api_key_id.clone();
        let provider_id = self.provider_id.clone();
        let model = self.model.clone();
        let request_type = self.request_type.clone();
        let write = async move {
            crate::billing::ledger::spawn_request_log_for_guard(
                &store,
                user_id,
                api_key_id,
                Some(provider_id),
                model,
                &request_type,
                p,
                c,
                t,
                k,
                latency,
                499,
                false,
                Some("client disconnected".to_string()),
            );
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(write);
        } else {
            tracing::warn!("StreamAbortGuard dropped without runtime — 499 补账日志丢弃");
        }
    }
}

/// Build the SSE streaming response from a successful upstream response.
///
/// `idle_timeout_secs` bounds the gap between upstream chunks: a stalled
/// stream (no bytes for that long) is aborted and counted as a provider
/// failure instead of hanging the connection forever.
#[allow(clippy::too_many_arguments)]
pub(super) fn stream_response(
    state: AppState,
    resp: reqwest::Response,
    adapter: Arc<dyn crate::channel::ChannelAdapter>,
    client_protocol: Protocol,
    provider_protocol: Protocol,
    provider_id: String,
    idle_timeout_secs: i32,
    model: String,
    user_id: String,
    api_key_id: Option<String>,
    client_wants_usage: bool,
    concurrency_guard: Option<ConcurrencyGuard>,
) -> Response {
    let byte_stream = resp.bytes_stream();
    let store = state.store.clone();
    let shutdown = state.shutdown.clone();
    let threshold = state.config.auto_disable_threshold;
    let model_log = model.clone();
    let start = Instant::now();
    let mut converter = StreamConverter::new(client_protocol, provider_protocol, &model)
        .strip_usage_chunk(!client_wants_usage);
    // idle_timeout_secs<=0 的遗留渠道先回退全局 request_timeout_secs 再
    // clamp，与 client.rs 的非流式超时语义一致（避免退化为 1 秒误 abort）。
    let idle_secs = if idle_timeout_secs > 0 {
        idle_timeout_secs as u64
    } else {
        state.config.request_timeout_secs
    };
    let idle_timeout = std::time::Duration::from_secs(idle_secs.clamp(1, 600));

    // Abort the stream if the upstream sends more than this without a
    // complete SSE event boundary — an unbounded buffer is a memory risk.
    const MAX_SSE_BUF: usize = 1024 * 1024;

    let sse_stream = async_stream::stream! {
        // 并发槽位随生成器存活:流正常结束、客户端断连、关停取消都会
        // 触发生成器 Drop,槽位在那时才归还。
        let _concurrency_guard = concurrency_guard;
        let usage_cell = Arc::new(Mutex::new((0, 0, 0, 0)));
        let mut guard = StreamAbortGuard {
            store: store.clone(),
            user_id: user_id.clone(),
            api_key_id: api_key_id.clone(),
            provider_id: provider_id.clone(),
            model: model_log.clone(),
            request_type: client_protocol.as_str().to_string(),
            start,
            usage: usage_cell.clone(),
            finished: false,
        };
        let mut buf: Vec<u8> = Vec::new();
        // Set when the upstream stream becomes unusable mid-flight (stall,
        // transport error, or unbounded buffer); recorded as a failure below.
        let mut stream_error: Option<String> = None;
        // 上游在 200 的 SSE 流内发错误帧(各协议形状):converter 已转成
        // 协议错误事件发给客户端,收尾时按失败记账而不是成功。
        let mut error_frame = false;
        let mut events: u32 = 0;
        let mut upstream = std::pin::pin!(byte_stream);
        loop {
            // 关停优先:收到取消立即跳出,走下方统一收尾——已捕获 usage
            // 照常同步记账,而不是被 30s 强杀跳过收尾丢账。
            let item = tokio::select! {
                _ = shutdown.cancelled() => {
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
                    warn!("Upstream stream error from {}: {}", provider_id, e);
                    stream_error = Some(format!("upstream stream error: {}", e));
                    break;
                }
            }
        }

        if let Some(err_msg) = stream_error {
            // The upstream stream was unusable — count it as a provider failure.
            let latency = start.elapsed().as_millis() as f64;
            spawn_record_failure(store.clone(), provider_id.clone(), latency, 0, threshold, true);
            let (p, c, t, k) = converter.usage();
            spawn_request_log(
                &state, user_id.clone(), api_key_id.clone(), Some(provider_id.clone()),
                model_log.clone(), client_protocol.as_str(),
                p, c, t, k, latency as i32, 502, false, Some(err_msg.clone()),
            );
            // 502 已记账,立即标记 guard 完成:否则客户端在下面 yield 期间
            // 断开会触发 StreamAbortGuard::drop 再记一条 499,重复记账。
            guard.finish();
            // 通知客户端流异常终止：协议对应的错误事件 + converter 收尾事件。
            // finish() 对转换型 converter 已含终止帧([DONE]/message_stop),
            // 仅在其缺失时补一个,避免客户端收到双重终止。
            yield Ok(to_axum_event(stream_error_event(client_protocol, &err_msg)));
            // responses 客户端:error 事件即终止,跳过 converter 收尾——收尾
            // 会产出与错误矛盾的 *.done/response.completed(部分输出被当成
            // 成功完成),codex 等客户端会误判流正常结束。
            let finish_outs = match client_protocol {
                Protocol::Responses => vec![],
                Protocol::OpenAi | Protocol::Anthropic => converter.finish(),
            };
            let has_terminal = finish_outs
                .iter()
                .any(|o| o.data == "[DONE]" || o.data.contains("message_stop"));
            for out in finish_outs {
                yield Ok(to_axum_event(out));
            }
            if !has_terminal
                && let Some(term) = stream_terminal_event(client_protocol) {
                    yield Ok(to_axum_event(term));
                }
            return;
        }

        if let Some(mut ev) = parse_sse_remaining(&mut buf) {
            adapter.normalize_stream_event(provider_protocol, &mut ev);
            events += 1;
            for out in converter.push(&ev) {
                if out.event.as_deref() == Some("error")
                    || out.data.starts_with("{\"error\":")
                {
                    error_frame = true;
                }
                yield Ok(to_axum_event(out));
            }
        }
        // 上游 200+SSE 头却一个事件都没发就 EOF:按失败处理(与模型实测
        // 探针同一口径),否则客户端对着空流干等,指标还被记成成功。
        if events == 0 {
            let err_msg = "upstream sent an empty SSE stream".to_string();
            let latency = start.elapsed().as_millis() as f64;
            spawn_record_failure(store.clone(), provider_id.clone(), latency, 0, threshold, true);
            spawn_request_log(
                &state, user_id.clone(), api_key_id.clone(), Some(provider_id.clone()),
                model_log.clone(), client_protocol.as_str(),
                0, 0, 0, 0, latency as i32, 502, false, Some(err_msg.clone()),
            );
            // 同上面的 stream_error 路径:先标记完成再 yield,防止重复记账。
            guard.finish();
            yield Ok(to_axum_event(stream_error_event(client_protocol, &err_msg)));
            if let Some(term) = stream_terminal_event(client_protocol) {
                yield Ok(to_axum_event(term));
            }
            return;
        }
        for out in converter.finish() {
            yield Ok(to_axum_event(out));
        }

        // Stream finished — record metrics and request log.
        let latency = start.elapsed().as_millis() as f64;
        let (p, c, t, k) = converter.usage();
        if error_frame {
            // 上游流内错误帧:HTTP 虽 200,语义上是失败。按失败记账(已捕获
            // 的 usage 照扣,与流中途错误同口径),不让刷错误帧的渠道显得健康。
            spawn_record_failure(store.clone(), provider_id.clone(), latency, 0, threshold, true);
            request_log_sync(
                &state, user_id.clone(), api_key_id.clone(), Some(provider_id.clone()),
                model_log.clone(), client_protocol.as_str(),
                p, c, t, k, latency as i32, 502, false,
                Some("upstream sent an in-stream error frame".to_string()),
            )
            .await;
            guard.finish();
            return;
        }
        spawn_record_result(store.clone(), provider_id.clone(), latency, true);
        // 计费级:与非流式一致,流结束时同步等待日志与扣费落库,
        // fire-and-forget 在进程退出时会丢账。
        request_log_sync(
            &state, user_id.clone(), api_key_id.clone(), Some(provider_id.clone()),
            model_log.clone(), client_protocol.as_str(),
            p, c, t, k, latency as i32, 200, true, None,
        )
        .await;
        debug!(
            "Stream completed: model={} provider={} latency={}ms tokens={}",
            model_log, provider_id, latency as i64, t
        );
        guard.finish();
    };

    Sse::new(sse_stream).into_response()
}

fn to_axum_event(out: SseOut) -> Event {
    let ev = Event::default().data(out.data);
    match out.event {
        Some(name) => ev.event(name),
        None => ev,
    }
}
