//! Live per-(channel, model) testing: a shared probe used by the admin
//! test-model endpoint and by a background loop that re-tests every active
//! channel's models every 30 minutes. Results are persisted to the
//! model_health table so the models page and the test matrix can show real
//! measured health instead of the channel-level ping status.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::join_all;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::channel::transport::{MAX_RESP_BODY, read_json_limited, send_request};
use crate::models::Provider;
use crate::store::AppStore;
use aikun_protocol::{Protocol, parse_sse_events};
use aikun_store::surreal::channels_store::ModelHealthRow;

/// How often the background loop re-tests every (channel, model) pair.
pub const MODEL_TEST_INTERVAL_SECS: u64 = 1800;

/// 每轮实测的总时长预算：超时即取消本轮并记录未测组合。
const ROUND_BUDGET_SECS: u64 = 600;
/// 实测并发 worker 数（参照前端 test-all 的有界并发模式）。
const ROUND_WORKERS: usize = 4;
/// 非流式响应体的读取上限。
const BODY_CAP: usize = MAX_RESP_BODY;
/// 流式探针的整体 deadline（秒）、SSE 缓冲上限与可见文本截断上限。
const STREAM_DEADLINE_SECS: u64 = 120;
const STREAM_BUF_CAP: usize = 1024 * 1024;
const STREAM_TEXT_CAP: usize = 64 * 1024;

/// Outcome of one live model test.
pub struct ModelProbe {
    pub ok: bool,
    pub latency_ms: f64,
    /// Upstream HTTP status (0 for transport errors).
    pub status: u16,
    pub error: Option<String>,
    pub snippet: Option<String>,
    /// Raw upstream body (non-stream) or an assembled stream summary —
    /// shown verbatim on the test page.
    pub response: Option<Value>,
}

/// Build the minimal upstream request body. `max_tokens` is small for the
/// background loop and larger for interactive tests (a 16-token cap truncates
/// greetings and yields empty content from reasoning models, which made test
/// responses look broken). For OpenAI both field names are sent: newer
/// o-series models reject `max_tokens`, older OpenAI-compatible servers
/// ignore unknown fields.
fn build_probe_body(
    model: &str,
    protocol: Protocol,
    messages: &[Value],
    max_tokens: u32,
    stream: bool,
) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
    });
    match protocol {
        Protocol::OpenAi | Protocol::Responses => body["max_completion_tokens"] = json!(max_tokens),
        Protocol::Anthropic => {}
    }
    if stream {
        body["stream"] = json!(true);
    }
    body
}

/// Extract the visible text from a completed (non-stream) upstream body.
fn extract_text(protocol: Protocol, v: &Value) -> String {
    match protocol {
        Protocol::Anthropic => v["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b["type"].as_str() == Some("text"))
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default(),
        // 探针按 chat 形状发送(含 Responses 渠道,与 0.4 行为一致)。
        Protocol::OpenAi | Protocol::Responses => v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    }
}

/// Extract incremental text from one SSE event's data payload.
fn extract_delta(protocol: Protocol, data: &str) -> String {
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    match protocol {
        Protocol::Anthropic => {
            if v["type"].as_str() == Some("content_block_delta") {
                return v["delta"]["text"].as_str().unwrap_or("").to_string();
            }
            String::new()
        }
        Protocol::OpenAi | Protocol::Responses => v["choices"][0]["delta"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    }
}

/// Run one live test of `model` through `provider` and report the outcome.
/// When `stream` is true the upstream is asked for SSE and the stream is
/// consumed to completion (bounded by the provider idle timeout) so streaming
/// support is verified, not just the first response byte.
#[allow(clippy::too_many_arguments)]
pub async fn probe_model(
    clients: &std::sync::Mutex<std::collections::HashMap<String, reqwest::Client>>,
    provider: &Provider,
    protocol: Protocol,
    model: &str,
    messages: Vec<Value>,
    max_tokens: u32,
    stream: bool,
    connect_timeout_secs: u64,
    ttfb_timeout_secs: u64,
) -> ModelProbe {
    let body = build_probe_body(model, protocol, &messages, max_tokens, stream);
    let (resp, latency) = match send_request(
        clients,
        provider,
        protocol,
        body,
        stream,
        120,
        connect_timeout_secs,
        ttfb_timeout_secs,
    )
    .await
    {
        Ok(v) => v,
        Err((err_body, status, latency)) => {
            return ModelProbe {
                ok: false,
                latency_ms: latency,
                status,
                error: Some(
                    err_body["error"]["message"]
                        .as_str()
                        .unwrap_or("upstream request failed")
                        .to_string(),
                ),
                snippet: None,
                response: None,
            };
        }
    };

    let status = resp.status().as_u16();
    if status != 200 {
        let v: Value = read_json_limited(resp, BODY_CAP)
            .await
            .unwrap_or_else(|_| json!({}));
        return ModelProbe {
            ok: false,
            latency_ms: latency,
            status,
            error: Some(
                v["error"]["message"]
                    .as_str()
                    .unwrap_or("upstream error")
                    .to_string(),
            ),
            snippet: None,
            response: Some(v),
        };
    }

    if !stream {
        let v = match read_json_limited(resp, BODY_CAP).await {
            Ok(v) => v,
            Err(e) => {
                return ModelProbe {
                    ok: false,
                    latency_ms: latency,
                    status,
                    error: Some(e.zh()),
                    snippet: None,
                    response: None,
                };
            }
        };
        // 与代理转发路径同一口径:200 但响应体不符合协议形状(MiniMax 式
        // 200+错误信封)判失败,否则会写成 healthy 并污染渠道 EMA。
        // 错误文案透传信封的 code/msg(zcode 3012/1005、MiniMax 等都有)——
        // 2026-11-20 排查实录:"不符合协议格式"挡住了上游 405+3012 客户端
        // 版本被拒的真实原因,定位绕了一大圈。
        if !aikun_protocol::valid_response_shape(&v, protocol) {
            let envelope = match (v["code"].as_i64(), v["msg"].as_str()) {
                (Some(code), Some(msg)) if !msg.is_empty() => format!("上游 {code}:{msg}"),
                (Some(code), _) => format!("上游 {code}"),
                _ => String::new(),
            };
            return ModelProbe {
                ok: false,
                latency_ms: latency,
                status,
                error: Some(if envelope.is_empty() {
                    "上游响应不符合协议格式".to_string()
                } else {
                    format!("上游响应不符合协议格式({envelope})")
                }),
                snippet: None,
                response: Some(v),
            };
        }
        let text = extract_text(protocol, &v);
        return ModelProbe {
            ok: true,
            latency_ms: latency,
            status,
            error: None,
            snippet: Some(text.chars().take(120).collect()),
            response: Some(v),
        };
    }

    // Streaming: consume events until EOF, a mid-stream stall, or a transport
    // error — anything short of a clean EOF counts as a failed probe.
    // Bounded by an overall 120s deadline, a 1MB SSE buffer cap and a 64KB
    // visible-text cap so a runaway stream cannot exhaust memory or time.
    use futures_util::StreamExt;
    let idle = std::time::Duration::from_secs(provider.timeout_secs.clamp(1, 600) as u64);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(STREAM_DEADLINE_SECS);
    let mut upstream = std::pin::pin!(resp.bytes_stream());
    let mut buf: Vec<u8> = Vec::new();
    let mut text = String::new();
    let mut events: u32 = 0;
    let mut stream_error: Option<String> = None;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            stream_error = Some(format!("流式响应超过整体 {}s 上限", STREAM_DEADLINE_SECS));
            break;
        }
        let wait = idle.min(deadline - now);
        let item = match tokio::time::timeout(wait, upstream.next()).await {
            Ok(Some(item)) => item,
            Ok(None) => break,
            Err(_) => {
                stream_error = Some(if tokio::time::Instant::now() >= deadline {
                    format!("流式响应超过整体 {}s 上限", STREAM_DEADLINE_SECS)
                } else {
                    format!("流式响应中断：超过 {}s 无数据", wait.as_secs())
                });
                break;
            }
        };
        match item {
            Ok(bytes) => {
                buf.extend_from_slice(&bytes);
                for ev in parse_sse_events(&mut buf) {
                    events += 1;
                    if text.len() < STREAM_TEXT_CAP {
                        text.push_str(&extract_delta(protocol, &ev.data));
                        if text.len() > STREAM_TEXT_CAP {
                            // 截断到 64KB（char 边界安全）
                            let mut end = STREAM_TEXT_CAP;
                            while !text.is_char_boundary(end) {
                                end -= 1;
                            }
                            text.truncate(end);
                        }
                    }
                }
                if buf.len() > STREAM_BUF_CAP {
                    stream_error = Some("流式缓冲超过 1MB 上限".to_string());
                    break;
                }
            }
            Err(e) => {
                stream_error = Some(format!("流式传输出错: {}", e));
                break;
            }
        }
    }

    let summary = json!({
        "stream": true,
        "events": events,
        "content": text,
    });
    ModelProbe {
        ok: stream_error.is_none() && events > 0,
        latency_ms: latency,
        status,
        error: stream_error,
        snippet: Some(
            summary["content"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(120)
                .collect(),
        ),
        response: Some(summary),
    }
}

/// Persist a probe outcome for one (channel, model) pair.
/// 模型测试是真实推理请求,延迟有代表性:顺带按代理转发路径同款口径
/// 更新渠道的 latency_ms / error_rate EMA(失败不会触发自动禁用)。
/// 阶段 S:model_health UPSERT 与 EMA 写都是 async Surreal 查询。
pub async fn record_model_health(
    store: &AppStore,
    provider_id: &str,
    model: &str,
    ok: bool,
    latency_ms: f64,
    error: Option<&str>,
) {
    if let Err(e) = store
        .channels
        .upsert_model_health(ModelHealthRow {
            provider_id: provider_id.to_string(),
            model: model.to_string(),
            status: if ok { "healthy" } else { "unhealthy" }.to_string(),
            latency_ms,
            error: error.unwrap_or("").to_string(),
            checked_at: Some(chrono::Utc::now()),
        })
        .await
    {
        tracing::warn!("record_model_health upsert failed: {e}");
    }
    crate::channel::selector::record_request_result(store, provider_id, latency_ms, ok).await;
}

/// Background loop: every MODEL_TEST_INTERVAL_SECS, probe every active
/// channel's every model with a tiny request and persist the results.
/// Each round runs with ROUND_WORKERS concurrent workers under a total
/// ROUND_BUDGET_SECS budget so combinations are covered breadth-first
/// instead of starving the tail sequentially.
pub async fn run_model_test_loop(
    store: Arc<AppStore>,
    config: Arc<crate::config::AppConfig>,
    clients: Arc<Mutex<std::collections::HashMap<String, reqwest::Client>>>,
) {
    // Wait one interval before the first round — the health ping loop already
    // covers startup, and upstreams may still be warming up.
    tokio::time::sleep(Duration::from_secs(MODEL_TEST_INTERVAL_SECS)).await;
    loop {
        run_model_test_round(&store, &config, &clients).await;
        tokio::time::sleep(Duration::from_secs(MODEL_TEST_INTERVAL_SECS)).await;
    }
}

/// One round: expand all (channel, model) pairs into a queue and drain it
/// with ROUND_WORKERS concurrent workers. If the ROUND_BUDGET_SECS budget
/// is exceeded the round is cancelled and the untested pairs are logged.
async fn run_model_test_round(
    store: &Arc<AppStore>,
    config: &Arc<crate::config::AppConfig>,
    clients: &Arc<Mutex<std::collections::HashMap<String, reqwest::Client>>>,
) {
    let providers: Vec<Provider> = match store.channels.active_channels().await {
        Ok(rows) => rows
            .into_iter()
            .map(|mut p: Provider| {
                // api_key 出库解密(enc:v1: 前缀密文,明文兼容)
                p.api_key = crate::crypto::decrypt_or_plain(&store.cipher, &p.api_key);
                p
            })
            .collect(),
        Err(e) => {
            tracing::error!("Model test loop failed to read providers: {e}");
            return;
        }
    };

    let providers = Arc::new(providers);
    // 展开 (渠道, 模型) 组合为任务队列，worker 弹出即测
    let mut pairs: VecDeque<(usize, String)> = VecDeque::new();
    for (idx, provider) in providers.iter().enumerate() {
        let models: Vec<String> = serde_json::from_str(&provider.models).unwrap_or_default();
        for model in models {
            pairs.push_back((idx, model));
        }
    }
    if pairs.is_empty() {
        info!("Model test round complete: 0 tested, 0 failed");
        return;
    }
    let queue = Arc::new(Mutex::new(pairs));
    let tested = Arc::new(AtomicU32::new(0));
    let failed = Arc::new(AtomicU32::new(0));

    let workers = join_all((0..ROUND_WORKERS).map(|_| {
        model_test_worker(
            store.clone(),
            config.clone(),
            clients.clone(),
            providers.clone(),
            queue.clone(),
            tested.clone(),
            failed.clone(),
        )
    }));

    if tokio::time::timeout(Duration::from_secs(ROUND_BUDGET_SECS), workers)
        .await
        .is_err()
    {
        let remaining: Vec<String> = match queue.lock() {
            Ok(q) => q
                .iter()
                .map(|(idx, model)| format!("{}/{}", providers[*idx].name, model))
                .collect(),
            Err(_) => vec![],
        };
        warn!(
            "Model test round cancelled after {}s; {} untested combos: {}",
            ROUND_BUDGET_SECS,
            remaining.len(),
            remaining.join(", ")
        );
    }
    info!(
        "Model test round complete: {} tested, {} failed",
        tested.load(Ordering::Relaxed),
        failed.load(Ordering::Relaxed)
    );
}

/// Pop (channel, model) pairs from the shared queue and probe them until
/// the queue is empty. The queue lock is never held across an await.
async fn model_test_worker(
    store: Arc<AppStore>,
    config: Arc<crate::config::AppConfig>,
    clients: Arc<Mutex<std::collections::HashMap<String, reqwest::Client>>>,
    providers: Arc<Vec<Provider>>,
    queue: Arc<Mutex<VecDeque<(usize, String)>>>,
    tested: Arc<AtomicU32>,
    failed: Arc<AtomicU32>,
) {
    loop {
        let task = match queue.lock() {
            Ok(mut q) => q.pop_front(),
            Err(_) => return,
        };
        let Some((idx, model)) = task else { return };
        let provider = &providers[idx];
        let protocol = crate::models::channel_protocol(provider);
        let probe = probe_model(
            &clients,
            provider,
            protocol,
            &model,
            vec![json!({"role": "user", "content": "Say OK"})],
            16,
            false,
            config.connect_timeout_secs,
            config.ttfb_timeout_secs,
        )
        .await;
        tested.fetch_add(1, Ordering::Relaxed);
        if !probe.ok {
            failed.fetch_add(1, Ordering::Relaxed);
            warn!(
                "Model test [{}/{}]: FAILED ({})",
                provider.name,
                model,
                probe.error.as_deref().unwrap_or("unknown")
            );
        }
        record_model_health(
            &store,
            &provider.id,
            &model,
            probe.ok,
            probe.latency_ms,
            probe.error.as_deref(),
        )
        .await;
    }
}
