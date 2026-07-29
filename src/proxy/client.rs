use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use reqwest::Client;
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::models::Provider;
use crate::proxy::convert::PROTOCOL_ANTHROPIC;

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
    ["/chat/completions", "/messages", "/models"]
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
fn sanitize_log_url(url: &str) -> String {
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

/// Apply protocol-specific auth headers to a request builder.
fn apply_auth(
    req: reqwest::RequestBuilder,
    provider: &Provider,
    protocol: &str,
) -> reqwest::RequestBuilder {
    if protocol == PROTOCOL_ANTHROPIC {
        req.header("x-api-key", &provider.api_key)
            .header("anthropic-version", "2023-06-01")
    } else {
        req.header("Authorization", format!("Bearer {}", provider.api_key))
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
    protocol: &str,
    request_body: &Value,
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

    let path = if protocol == PROTOCOL_ANTHROPIC {
        "/messages"
    } else {
        "/chat/completions"
    };
    let url = build_api_url(&provider.base_url, path);

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
        .json(request_body);
    let req = apply_auth(req, provider, protocol);

    match req.send().await {
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
