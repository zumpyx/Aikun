//! 渠道公共传输层:HTTP client 复用(按代理 + 超时缓存)、上游 URL 拼接、
//! 日志脱敏,以及经渠道插件定制后的请求发送。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use reqwest::Client;
use serde_json::{json, Value};
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
            BodyReadError::TooLarge(max) => write!(f, "upstream response body exceeded {} bytes", max),
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
pub async fn read_json_limited(resp: reqwest::Response, max_bytes: usize) -> Result<Value, BodyReadError> {
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
pub fn build_api_url(base_url: &str, path: &str) -> String {
    // base_url 可能带 query（如 Azure 的 ?api-version=...），先拆出来最后拼回
    let (base, query) = match base_url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (base_url, None),
    };
    let base = base.trim_end_matches('/');
    let joined = if is_endpoint_url(base) {
        // base 已含完整端点路径（如 Azure 部署 URL），原样使用
        base.to_string()
    } else if is_version_segment(base.rsplit('/').next().unwrap_or("")) {
        // 已含版本段（/v1、/v1beta、/v2 等），不再追加 /v1
        format!("{}{}", base, path)
    } else {
        format!("{}/v1{}", base, path)
    };
    match query {
        Some(q) => format!("{}?{}", joined, q),
        None => joined,
    }
}

/// base 是否已是完整端点（以已知端点路径结尾）。
fn is_endpoint_url(base: &str) -> bool {
    ["/chat/completions", "/messages", "/models", "/responses"]
        .iter()
        .any(|ep| base.ends_with(ep))
}

/// 路径段是否为版本段：v 开头紧跟数字，其余仅字母数字/./-（v1、v1beta、v2.0）。
fn is_version_segment(seg: &str) -> bool {
    seg.len() >= 2
        && seg.starts_with('v')
        && seg.as_bytes()[1].is_ascii_digit()
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// Strip embedded credentials from a proxy URL for logging:
/// `socks5://user:pass@host:1080` → `socks5://host:1080`.
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

/// Build an HTTP client honoring the given proxy settings.
/// `timeout_secs` of 0 means no overall timeout (used for streaming requests,
/// where the total duration is unbounded). Build failures are logged loudly
/// and fall back to a default direct client.
fn build_client(proxy_url: &str, context: &str, timeout_secs: u64) -> Client {
    let mut builder = Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
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
                    sanitize_proxy_url(proxy_url), context, e
                );
            }
        }
    }
    match builder.build() {
        Ok(client) => client,
        Err(e) => {
            tracing::error!(
                "Failed to build HTTP client for {}: {} — falling back to default client",
                context, e
            );
            Client::new()
        }
    }
}

/// Get a cached HTTP client for the (proxy_url, timeout_secs) combination,
/// building and caching one on first use. `context` is only used in logs.
pub fn cached_client(
    clients: &Mutex<HashMap<String, Client>>,
    proxy_url: &str,
    context: &str,
    timeout_secs: u64,
) -> Client {
    let key = format!("{}|{}", proxy_url.trim(), timeout_secs);
    match clients.lock() {
        Ok(mut cache) => {
            // 防缓存无限增长：条目过多时整体清空重建
            if cache.len() >= 64 && !cache.contains_key(&key) {
                cache.clear();
            }
            cache
                .entry(key)
                .or_insert_with(|| build_client(proxy_url, context, timeout_secs))
                .clone()
        }
        Err(_) => {
            warn!("Client cache lock poisoned — building uncached client");
            build_client(proxy_url, context, timeout_secs)
        }
    }
}

/// Send a chat request to the provider and return the raw HTTP response.
/// The caller decides whether to read it as JSON or as an SSE byte stream.
/// On transport failure returns a synthetic (error_body, 502, latency).
///
/// Non-streaming requests use the provider's `timeout_secs`, falling back to
/// the global `fallback_timeout_secs` (REQUEST_TIMEOUT_SECS) when the
/// provider value is not positive (legacy rows).
pub async fn send_request(
    clients: &Mutex<HashMap<String, Client>>,
    provider: &Provider,
    protocol: crate::protocol::Protocol,
    mut request_body: Value,
    stream: bool,
    fallback_timeout_secs: u64,
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
    );

    // URL、鉴权头与请求体修补都交给渠道插件(见 crate::channels)。
    let adapter = crate::channel::adapter_for(provider);
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
    let req = adapter.apply_auth(req, &provider.api_key, protocol);

    // 流式响应总时长任意,client 不带整体超时(中段 stall 由流读取侧的
    // idle 超时约束);但首字节必须有界:上游 accept 后迟迟不发响应头时
    // 按传输失败处理并故障转移,而不是挂到整体重试 deadline 才被取消。
    let send_result = if stream {
        let ttfb_secs = non_stream_timeout.clamp(1, 60);
        match tokio::time::timeout(std::time::Duration::from_secs(ttfb_secs), req.send()).await {
            Ok(r) => r,
            Err(_) => {
                let latency = start.elapsed().as_millis() as f64;
                warn!(
                    "Request to {} timed out waiting for response headers ({}s)",
                    provider.name, ttfb_secs
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
            // 完整 reqwest 错误（含上游 URL）只进服务端日志
            warn!("Request to {} failed: {}", provider.name, e);
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
            build_api_url("https://x.com/openai/deployments/d/chat/completions", "/chat/completions"),
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
            build_api_url("https://x.com/d/chat/completions?api-version=1&x=2", "/chat/completions"),
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
        assert_eq!(sanitize_log_url("https://x.com/v1/models"), "https://x.com/v1/models");
    }

    #[test]
    fn model_extraction() {
        assert_eq!(extract_model(&json!({"model": "gpt-4"})), "gpt-4");
        assert_eq!(extract_model(&json!({})), "unknown");
        assert_eq!(extract_model(&json!({"model": null})), "unknown");
    }
}
