//! 渠道公共传输层:HTTP client 复用(按代理 + 超时缓存)、上游 URL 拼接、
//! 日志脱敏,以及经渠道适配器定制后的请求发送。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use reqwest::Client;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::models::Provider;

/// 上游响应体大小上限(8MB):异常上游的超大 body 不再造成内存 DoS。
pub const MAX_RESP_BODY: usize = 8 * 1024 * 1024;

/// [`read_json_limited`] 的失败原因。对话路径沿用英文文案(`Display`),
/// 管理端(模型实测 / fetch-models)沿用中文文案([`BodyReadError::zh`]),
/// 与合并前两份实现的对外文案逐字一致。
#[derive(Debug)]
pub enum BodyReadError {
    Read(String),
    TooLarge(usize),
    Parse(String),
}

impl std::fmt::Display for BodyReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyReadError::Read(e) => write!(f, "failed to read upstream body: {}", e),
            BodyReadError::TooLarge(max) => {
                write!(f, "upstream response body exceeded {} bytes", max)
            }
            BodyReadError::Parse(e) => write!(f, "failed to parse upstream response: {}", e),
        }
    }
}

impl BodyReadError {
    /// 管理端中文文案。
    pub fn zh(&self) -> String {
        match self {
            BodyReadError::Read(e) => format!("读取响应体失败: {}", e),
            BodyReadError::TooLarge(max) => format!("响应体超过 {} 字节上限", max),
            BodyReadError::Parse(e) => format!("响应体不是合法 JSON: {}", e),
        }
    }
}

/// 读取上游 JSON 响应体,带硬性字节上限。读取失败、超限或解析失败都返回
/// Err —— 解析失败必须报错而不是当空对象:否则 200+HTML 错误页/非 JSON
/// 响应会被当成"合法的 200"。对话、直通、模型实测、fetch-models 共用。
pub async fn read_json_limited(
    resp: reqwest::Response,
    max_bytes: usize,
) -> Result<Value, BodyReadError> {
    use futures_util::StreamExt;
    let mut stream = std::pin::pin!(resp.bytes_stream());
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| BodyReadError::Read(e.to_string()))?;
        if buf.len() + bytes.len() > max_bytes {
            return Err(BodyReadError::TooLarge(max_bytes));
        }
        buf.extend_from_slice(&bytes);
    }
    serde_json::from_slice(&buf).map_err(|e| BodyReadError::Parse(e.to_string()))
}

/// Build a proper URL path based on provider type and base URL.
pub use aikun_channels::build_api_url; // 移至 channels crate,re-export 保持旧路径

pub fn sanitize_proxy_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
            format!("{}://{}", scheme, host)
        }
        None => match url.rsplit_once('@') {
            Some((_, host)) => host.to_string(),
            None => url.to_string(),
        },
    }
}

/// 上游 URL 日志脱敏：剥掉 userinfo 与 query（query 常带 api-version、key 等参数）。
pub fn sanitize_log_url(url: &str) -> String {
    let no_creds = sanitize_proxy_url(url);
    no_creds.split('?').next().unwrap_or("").to_string()
}

/// 9.8 凭证脱敏:把文本中出现的密钥明文替换为 `***`。短于 4 个字符的
/// "密钥"不替换(会把普通文本打烂,且无脱敏价值)。用于上游错误消息
/// 进入日志 / request_logs / 客户端错误体前的最后一道出口过滤——
/// one-api 系上游在 401/403 消息里常回显完整渠道 key。
pub fn redact_secrets(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        let s = secret.trim();
        if s.len() < 4 {
            continue;
        }
        if out.contains(s) {
            out = out.replace(s, "***");
        }
    }
    out
}

/// [`redact_secrets`] 的 JSON 版:对 JSON 值序列化后的文本做脱敏,再解析
/// 回 JSON(上游错误体可能在任意字符串字段回显凭证)。解析失败时回退
/// 对原始序列化文本脱敏后的安全字符串包装。
pub fn redact_json_secrets(body: &Value, secrets: &[&str]) -> Value {
    let raw = serde_json::to_string(body).unwrap_or_default();
    let redacted = redact_secrets(&raw, secrets);
    serde_json::from_str(&redacted).unwrap_or(json!({
        "error": {"message": redacted, "type": "upstream_error"}
    }))
}

/// 从渠道配置里收集"不得出现在任何出口"的凭证明文(上游 key 与代理
/// userinfo)。空串自动被 redact_secrets 的长度门槛滤掉。
pub fn provider_secrets(provider: &Provider) -> Vec<String> {
    let mut secrets = vec![provider.api_key.clone()];
    if let Some((_, rest)) = provider.proxy_url.split_once("://")
        && let Some((userinfo, _)) = rest.split_once('@')
        && userinfo.contains(':')
    {
        secrets.push(userinfo.to_string());
    }
    secrets
}

/// Build an HTTP client honoring the given proxy settings.
/// `timeout_secs` of 0 means no overall timeout (used for streaming requests,
/// where the total duration is unbounded). `connect_timeout_secs` is the
/// 9.2 connect timeout (was a hardcoded 15s). Build failures are logged
/// loudly and fall back to a default direct client.
fn build_client(
    proxy_url: &str,
    context: &str,
    timeout_secs: u64,
    connect_timeout_secs: u64,
) -> Client {
    let mut builder = Client::builder()
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs.max(1)))
        // 网关不跟随重定向，防止跨主机 302 把 x-api-key 泄露给第三方主机
        .redirect(reqwest::redirect::Policy::none());
    if timeout_secs > 0 {
        builder = builder.timeout(std::time::Duration::from_secs(timeout_secs));
    }
    if !proxy_url.is_empty() {
        match reqwest::Proxy::all(proxy_url) {
            Ok(proxy) => {
                builder = builder.proxy(proxy);
            }
            Err(e) => {
                warn!(
                    "Invalid proxy_url '{}' for {}: {} — connecting directly",
                    sanitize_proxy_url(proxy_url),
                    context,
                    e
                );
            }
        }
    }
    match builder.build() {
        Ok(client) => client,
        Err(e) => {
            tracing::error!(
                "Failed to build HTTP client for {}: {} — falling back to default client",
                context,
                e
            );
            Client::new()
        }
    }
}

/// Get a cached HTTP client for the (proxy_url, timeout_secs, connect) key,
/// building and caching one on first use. `context` is only used in logs.
pub fn cached_client(
    clients: &Mutex<HashMap<String, Client>>,
    proxy_url: &str,
    context: &str,
    timeout_secs: u64,
    connect_timeout_secs: u64,
) -> Client {
    let key = format!(
        "{}|{}|{}",
        proxy_url.trim(),
        timeout_secs,
        connect_timeout_secs
    );
    match clients.lock() {
        Ok(mut cache) => {
            // 防缓存无限增长：条目过多时整体清空重建
            if cache.len() >= 64 && !cache.contains_key(&key) {
                cache.clear();
            }
            cache
                .entry(key)
                .or_insert_with(|| {
                    build_client(proxy_url, context, timeout_secs, connect_timeout_secs)
                })
                .clone()
        }
        Err(_) => {
            warn!("Client cache lock poisoned — building uncached client");
            build_client(proxy_url, context, timeout_secs, connect_timeout_secs)
        }
    }
}

/// Send a chat request to the provider and return the raw HTTP response.
/// The caller decides whether to read it as JSON or as an SSE byte stream.
/// On transport failure returns a synthetic (error_body, 502, latency).
///
/// Non-streaming requests use the provider's `timeout_secs`, falling back to
/// the global `fallback_timeout_secs` (REQUEST_TIMEOUT_SECS) when the
/// provider value is not positive (legacy rows). `connect_timeout_secs` /
/// `ttfb_timeout_secs` are the 9.2 connect / first-byte timeouts
/// (AIKUN_CONNECT_TIMEOUT_SECS / AIKUN_TTFB_TIMEOUT_SECS).
#[allow(clippy::too_many_arguments)]
pub async fn send_request(
    clients: &Mutex<HashMap<String, Client>>,
    provider: &Provider,
    protocol: aikun_protocol::Protocol,
    mut request_body: Value,
    stream: bool,
    fallback_timeout_secs: u64,
    connect_timeout_secs: u64,
    ttfb_timeout_secs: u64,
) -> Result<(reqwest::Response, f64), (Value, u16, f64)> {
    let start = Instant::now();
    // Streaming responses can run arbitrarily long — no overall timeout
    // (mid-stream stalls are bounded by the idle timeout in the stream reader).
    let non_stream_timeout = if provider.timeout_secs > 0 {
        provider.timeout_secs as u64
    } else {
        fallback_timeout_secs
    };
    let client = cached_client(
        clients,
        &provider.proxy_url,
        &provider.name,
        if stream { 0 } else { non_stream_timeout },
        connect_timeout_secs,
    );

    // URL、鉴权头与请求体修补都交给渠道适配器(见 aikun_channels)。
    let adapter = aikun_channels::adapter_for(provider);
    adapter.prepare_request(provider, protocol, &mut request_body);
    let url = adapter.chat_url(provider, protocol);

    // 同 chat.rs:转发日志是每请求热路径,降级 debug。
    debug!(
        "Forwarding {} request to {} ({}){}",
        protocol,
        provider.name,
        sanitize_log_url(&url),
        if provider.proxy_url.is_empty() {
            String::new()
        } else {
            format!(" via proxy {}", sanitize_proxy_url(&provider.proxy_url))
        }
    );

    let req = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&request_body);
    let mut req = req;
    for (k, v) in adapter.auth_headers(&provider.api_key, protocol) {
        req = req.header(k, v);
    }
    for (k, v) in adapter.extra_headers(protocol) {
        req = req.header(k, v);
    }

    // 流式响应总时长任意,client 不带整体超时(中段 stall 由流读取侧的
    // idle 超时约束);但首字节必须有界:上游 accept 后迟迟不发响应头时
    // 按传输失败处理并故障转移,而不是挂到整体重试 deadline 才被取消。
    // 首字节上界来自 AIKUN_TTFB_TIMEOUT_SECS(9.2),不再借道渠道超时。
    let send_result = if stream {
        match tokio::time::timeout(
            std::time::Duration::from_secs(ttfb_timeout_secs.max(1)),
            req.send(),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                let latency = start.elapsed().as_millis() as f64;
                warn!(
                    "Request to {} timed out waiting for response headers ({}s)",
                    provider.name, ttfb_timeout_secs
                );
                return Err((
                    json!({
                        "error": {
                            "message": "Upstream request failed",
                            "type": "upstream_error"
                        }
                    }),
                    502,
                    latency,
                ));
            }
        }
    } else {
        req.send().await
    };

    match send_result {
        Ok(resp) => {
            let latency = start.elapsed().as_millis() as f64;
            Ok((resp, latency))
        }
        Err(e) => {
            let latency = start.elapsed().as_millis() as f64;
            // 完整 reqwest 错误的 URL 可能带查询串凭证(key=…),剥离后
            // 才进服务端日志(9.8)。
            warn!("Request to {} failed: {}", provider.name, e.without_url());
            // 返回终端用户的文案保持通用，避免泄露渠道拓扑
            Err((
                json!({
                    "error": {
                        "message": "Upstream request failed",
                        "type": "upstream_error"
                    }
                }),
                502,
                latency,
            ))
        }
    }
}

/// 字节级透传发送(通用端点并入管线后的 NoConversion 形态):按适配器的
/// OpenAI 鉴权头原样转发 body 到 `{openai_base_url}/<path>`,无协议转换、
/// 无请求体修补。失败语义与 [`send_request`] 一致(合成 502 错误体)。
#[allow(clippy::too_many_arguments)]
pub async fn send_raw(
    clients: &Mutex<HashMap<String, Client>>,
    provider: &Provider,
    path: &str,
    request_body: &Value,
    fallback_timeout_secs: u64,
    connect_timeout_secs: u64,
    ttfb_timeout_secs: u64,
) -> Result<(reqwest::Response, f64), (Value, u16, f64)> {
    let start = Instant::now();
    let timeout = if provider.timeout_secs > 0 {
        provider.timeout_secs as u64
    } else {
        fallback_timeout_secs
    };
    let client = cached_client(
        clients,
        &provider.proxy_url,
        &provider.name,
        timeout,
        connect_timeout_secs,
    );
    let adapter = aikun_channels::adapter_for(provider);
    let url = build_api_url(
        aikun_channels::base_url_for(provider, aikun_protocol::Protocol::OpenAi),
        &format!("/{path}"),
    );
    debug!(
        "Forwarding passthrough /{} to {} ({})",
        path,
        provider.name,
        sanitize_log_url(&url)
    );
    let req = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(request_body);
    let mut req = req;
    // 免密钥本地上游不携带 Authorization(空 Bearer 会让部分上游 401)。
    if !provider.api_key.is_empty() {
        for (k, v) in adapter.auth_headers(&provider.api_key, aikun_protocol::Protocol::OpenAi) {
            req = req.header(k, v);
        }
        for (k, v) in adapter.extra_headers(aikun_protocol::Protocol::OpenAi) {
            req = req.header(k, v);
        }
    }
    // 透传恒为非流式:整体超时由 client(timeout)承担,与旧 passthrough
    // 一致;ttfb 参数在此路径仅保持签名对齐(整体超时已覆盖首字节)。
    let _ = ttfb_timeout_secs;
    match req.send().await {
        Ok(resp) => Ok((resp, start.elapsed().as_millis() as f64)),
        Err(e) => {
            warn!(
                "Passthrough request to {} failed: {}",
                provider.name,
                e.without_url()
            );
            Err((
                json!({
                    "error": {"message": "Upstream request failed", "type": "upstream_error"}
                }),
                502,
                start.elapsed().as_millis() as f64,
            ))
        }
    }
}

/// Extract the model name from a request body.
pub fn extract_model(request_body: &Value) -> String {
    request_body["model"]
        .as_str()
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_url_appends_v1_when_missing() {
        assert_eq!(
            build_api_url("https://api.openai.com", "/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        // Trailing slash on the base is trimmed.
        assert_eq!(
            build_api_url("https://api.openai.com/", "/models"),
            "https://api.openai.com/v1/models"
        );
    }

    #[test]
    fn api_url_keeps_existing_v1() {
        assert_eq!(
            build_api_url("https://api.openai.com/v1", "/models"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            build_api_url("https://x.com/v1/", "/messages"),
            "https://x.com/v1/messages"
        );
    }

    #[test]
    fn api_url_keeps_other_version_segments() {
        assert_eq!(
            build_api_url("https://x.com/v1beta", "/messages"),
            "https://x.com/v1beta/messages"
        );
        assert_eq!(
            build_api_url("https://x.com/v2", "/models"),
            "https://x.com/v2/models"
        );
        // 非版本段（如网关前缀）仍追加 /v1
        assert_eq!(
            build_api_url("https://x.com/gateway", "/models"),
            "https://x.com/gateway/v1/models"
        );
    }

    #[test]
    fn api_url_full_endpoint_not_appended() {
        // base 已含完整端点路径时原样使用（如 Azure 部署 URL）
        assert_eq!(
            build_api_url(
                "https://x.com/openai/deployments/d/chat/completions",
                "/chat/completions"
            ),
            "https://x.com/openai/deployments/d/chat/completions"
        );
        assert_eq!(
            build_api_url("https://x.com/v1/messages", "/messages"),
            "https://x.com/v1/messages"
        );
    }

    #[test]
    fn api_url_preserves_query() {
        assert_eq!(
            build_api_url("https://x.com/v1beta?key=abc", "/messages"),
            "https://x.com/v1beta/messages?key=abc"
        );
        assert_eq!(
            build_api_url("https://x.com?api-version=2024-01-01", "/chat/completions"),
            "https://x.com/v1/chat/completions?api-version=2024-01-01"
        );
        // 完整端点 + query：不追加，query 原样保留
        assert_eq!(
            build_api_url(
                "https://x.com/d/chat/completions?api-version=1&x=2",
                "/chat/completions"
            ),
            "https://x.com/d/chat/completions?api-version=1&x=2"
        );
    }

    #[test]
    fn proxy_url_sanitized_for_logs() {
        assert_eq!(
            sanitize_proxy_url("socks5://user:pass@proxy.local:1080"),
            "socks5://proxy.local:1080"
        );
        assert_eq!(
            sanitize_proxy_url("http://proxy.local:8080"),
            "http://proxy.local:8080"
        );
        assert_eq!(sanitize_proxy_url("user:pass@host:1"), "host:1");
        assert_eq!(sanitize_proxy_url(""), "");
    }

    #[test]
    fn log_url_strips_query_and_userinfo() {
        assert_eq!(
            sanitize_log_url("https://user:pw@x.com/v1/chat/completions?api-version=1&key=abc"),
            "https://x.com/v1/chat/completions"
        );
        assert_eq!(
            sanitize_log_url("https://x.com/v1/models"),
            "https://x.com/v1/models"
        );
    }

    #[test]
    fn model_extraction() {
        assert_eq!(extract_model(&json!({"model": "gpt-4"})), "gpt-4");
        assert_eq!(extract_model(&json!({})), "unknown");
        assert_eq!(extract_model(&json!({"model": null})), "unknown");
    }

    /// 9.8:带密钥的输入跑一遍,输出不得出现明文。
    #[test]
    fn redact_secrets_masks_upstream_keys() {
        let key = "sk-live-0123456789abcdef";
        let msg = format!("Invalid API key {key} provided by proxy user:pw@host");
        let out = redact_secrets(&msg, &[key, "user:pw"]);
        assert!(!out.contains(key), "key leaked: {out}");
        assert!(!out.contains("user:pw"), "proxy creds leaked: {out}");
        assert!(out.contains("***"));
        // 短"密钥"不替换(< 4 字符无脱敏价值,只会打烂文本)
        assert_eq!(redact_secrets("abc", &["abc"]), "abc");
        assert_eq!(redact_secrets("", &[""]), "");
    }

    #[test]
    fn redact_json_secrets_masks_every_string_field() {
        let key = "sk-up-abcdef123456";
        let body = json!({
            "error": {"message": format!("unauthorized: bad key {key}"), "type": "auth"},
            "detail": {"echo": key}
        });
        let out = redact_json_secrets(&body, &[key]);
        let raw = serde_json::to_string(&out).unwrap();
        assert!(!raw.contains(key), "key leaked: {raw}");
        assert!(raw.contains("***"));
    }

    #[test]
    fn provider_secrets_collects_key_and_proxy_userinfo() {
        let p = crate::models::Provider {
            api_key: "sk-xyz".into(),
            proxy_url: "socks5://alice:secret@proxy.local:1080".into(),
            ..transport_test_provider()
        };
        let secrets = provider_secrets(&p);
        assert!(secrets.contains(&"sk-xyz".to_string()));
        assert!(secrets.contains(&"alice:secret".to_string()));
        // 无代理 userinfo 的渠道只收集 api_key
        let p2 = crate::models::Provider {
            api_key: "sk-abc".into(),
            proxy_url: "http://proxy.local:8080".into(),
            ..transport_test_provider()
        };
        assert_eq!(provider_secrets(&p2), vec!["sk-abc".to_string()]);
    }

    fn transport_test_provider() -> crate::models::Provider {
        crate::models::Provider {
            id: "p".into(),
            name: "p".into(),
            provider_type: "openai".into(),
            openai_base_url: "http://x".into(),
            anthropic_base_url: "http://x".into(),
            api_key: String::new(),
            key_decrypt_failed: false,
            models: "[]".into(),
            priority: 0,
            weight: 1.0,
            is_active: true,
            health_status: "healthy".into(),
            latency_ms: 0.0,
            error_rate: 0.0,
            last_health_check: None,
            max_retries: 3,
            timeout_secs: 120,
            proxy_url: String::new(),
            model_mapping: String::new(),
            consecutive_failures: 0,
            disabled_reason: String::new(),
            protocols: "[\"openai\"]".into(),
            default_protocol: "openai".into(),
            note: String::new(),
            website_url: String::new(),
            channel_kind: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }
}
