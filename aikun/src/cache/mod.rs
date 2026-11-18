//! 响应缓存层(可插拔后端)。
//!
//! - 后端抽象为 [`CacheBackend`],当前唯一实现 [`memory::MemoryCache`]
//!   (进程内、零外部依赖,保持"单二进制"部署)。roadmap-v0.5 明确不做
//!   多实例 / 外部 Redis 状态,trait 只作为将来的接缝保留。
//! - 只缓存**非流式**、成功(200 且通过协议形状校验)的响应;缓存的是已经
//!   转换成客户端协议的响应体,所以键里包含客户端协议。
//! - 默认 `mode = off`(与重构前行为一致);`deterministic` 只缓存
//!   `temperature == 0` 的请求;`always` 缓存全部非流式请求。
//! - 请求头 `Cache-Control: no-cache` / `no-store` 跳过缓存。
//! - 默认按用户隔离(`scope = user`):键含 user_id,且带 API key 调用时
//!   **按 key 隔离**(9.7:同一用户的不同 key 不得串缓存);
//!   `global` 仅建议在单租户部署下开启——**多租户部署下 A 用户的响应会
//!   被 B 用户命中(串缓存),属已知风险,勿在生产多租户环境开启**。
//! - 带 `tools` 或多模态内容(图片)的请求默认不缓存(9.7):工具调用
//!   依赖外部状态,图片响应体积大、命中率低,默认缓存收益差、风险高。
//! - 后端故障一律 fail-open:读失败按未命中处理,写失败只记日志。

pub mod memory;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::{AppConfig, CacheHitBilling, CacheMode, CacheScope};

/// 一条缓存记录:客户端协议形状的响应体 + 原始用量(命中时按此记账)。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CacheEntry {
    pub body: Value,
    /// (prompt, completion, total, cached) tokens,与 extract_usage_any 同口径。
    pub usage: (i32, i32, i32, i32),
}

pub type CacheFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 缓存后端接口。用手写 BoxFuture 保持 dyn 兼容,不引入 async-trait 依赖。
pub trait CacheBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn get<'a>(&'a self, key: &'a str) -> CacheFuture<'a, Option<CacheEntry>>;
    fn set<'a>(&'a self, key: &'a str, entry: CacheEntry, ttl: Duration) -> CacheFuture<'a, ()>;
}

/// 关闭缓存时使用的空后端。
pub struct NoopCache;

impl CacheBackend for NoopCache {
    fn name(&self) -> &'static str {
        "off"
    }
    fn get<'a>(&'a self, _key: &'a str) -> CacheFuture<'a, Option<CacheEntry>> {
        Box::pin(async { None })
    }
    fn set<'a>(&'a self, _key: &'a str, _entry: CacheEntry, _ttl: Duration) -> CacheFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// 网关使用的缓存门面:策略(是否可缓存、键怎么算)+ 后端。
pub struct GatewayCache {
    backend: Arc<dyn CacheBackend>,
    mode: CacheMode,
    scope: CacheScope,
    ttl: Duration,
    max_entry_bytes: usize,
    pub hit_billing: CacheHitBilling,
}

impl GatewayCache {
    pub fn from_config(config: &AppConfig) -> Self {
        let backend: Arc<dyn CacheBackend> = if config.cache_mode == CacheMode::Off {
            Arc::new(NoopCache)
        } else {
            build_backend(config)
        };
        if config.cache_mode != CacheMode::Off {
            tracing::info!(
                "响应缓存已启用: mode={:?} backend={} scope={:?} ttl={}s",
                config.cache_mode,
                backend.name(),
                config.cache_scope,
                config.cache_ttl_secs
            );
        }
        Self {
            backend,
            mode: config.cache_mode,
            scope: config.cache_scope,
            ttl: Duration::from_secs(config.cache_ttl_secs),
            max_entry_bytes: config.cache_max_entry_bytes,
            hit_billing: config.cache_hit_billing,
        }
    }

    /// 测试/嵌入用:直接指定后端与策略。
    pub fn new(
        backend: Arc<dyn CacheBackend>,
        mode: CacheMode,
        scope: CacheScope,
        ttl: Duration,
    ) -> Self {
        Self {
            backend,
            mode,
            scope,
            ttl,
            max_entry_bytes: 1024 * 1024,
            hit_billing: CacheHitBilling::Full,
        }
    }

    pub fn enabled(&self) -> bool {
        self.mode != CacheMode::Off
    }

    /// 请求可缓存时返回缓存键,否则 None。
    ///
    /// 9.7:`scope = user` 时键的隔离粒度是 **API key**(带 key 调用按
    /// key 隔离,JWT 会话回退 user_id)——不同 key 的相同请求键不同,
    /// 防止同用户多 key 之间串提示缓存之外的东西;`scope = global` 全站
    /// 共享(仅单租户,见模块头)。
    pub fn key_for(
        &self,
        client_protocol: aikun_protocol::Protocol,
        user_id: &str,
        api_key_id: Option<&str>,
        body: &Value,
        headers: &HeaderMap,
    ) -> Option<String> {
        if !should_cache(self.mode, body, headers) {
            return None;
        }
        let scope = match self.scope {
            CacheScope::User => api_key_id.filter(|k| !k.is_empty()).unwrap_or(user_id),
            CacheScope::Global => "*",
        };
        Some(cache_key(scope, client_protocol.as_str(), body))
    }

    pub async fn get(&self, key: &str) -> Option<CacheEntry> {
        self.backend.get(key).await
    }

    /// 写入缓存;超过单条大小上限的响应不缓存(防止大响应撑爆内存)。
    pub async fn put(&self, key: &str, entry: CacheEntry) {
        let size = serde_json::to_vec(&entry.body)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if size > self.max_entry_bytes {
            tracing::debug!("响应 {} 字节超过缓存单条上限,不缓存", size);
            return;
        }
        self.backend.set(key, entry, self.ttl).await
    }
}

fn build_backend(config: &AppConfig) -> Arc<dyn CacheBackend> {
    Arc::new(memory::MemoryCache::new(config.cache_max_entries))
}

/// Cache-Control 含 no-cache / no-store 时跳过缓存。
pub fn cache_bypass_requested(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(',').any(|p| {
                let p = p.trim();
                p.eq_ignore_ascii_case("no-cache") || p.eq_ignore_ascii_case("no-store")
            })
        })
        .unwrap_or(false)
}

/// 缓存策略判定(9.7:带 `tools` 或图片内容的请求默认不缓存)。
pub fn should_cache(mode: CacheMode, body: &Value, headers: &HeaderMap) -> bool {
    if mode == CacheMode::Off || body["stream"].as_bool().unwrap_or(false) {
        return false;
    }
    if cache_bypass_requested(headers) {
        return false;
    }
    if has_tools(body) || has_image_content(body) {
        return false;
    }
    match mode {
        CacheMode::Off => false,
        CacheMode::Always => true,
        CacheMode::Deterministic => body["temperature"].as_f64() == Some(0.0),
    }
}

/// 请求是否携带工具定义(OpenAI / Anthropic / Responses 同名 `tools`)。
fn has_tools(body: &Value) -> bool {
    body["tools"].as_array().is_some_and(|t| !t.is_empty())
}

/// 请求是否含多模态图片内容:OpenAI 的 `image_url`、Anthropic 的
/// `image` / `source` 块、Responses 的 `input_image`。只扫常见载体
/// (messages / input),深度有界——排除规则宁可漏放(漏放的请求只是
/// 可能被缓存,且 deterministic 模式下温度仍须为 0),不做全树递归。
fn has_image_content(body: &Value) -> bool {
    const MARKERS: [&str; 3] = ["image_url", "image", "input_image"];
    fn scan_parts(parts: &[Value]) -> bool {
        parts.iter().any(|part| {
            let kind = part["type"].as_str().unwrap_or_default();
            MARKERS.contains(&kind) || part.get("source").is_some()
        })
    }
    for carrier in ["messages", "input"] {
        if let Some(msgs) = body[carrier].as_array() {
            for msg in msgs {
                if let Some(parts) = msg["content"].as_array()
                    && scan_parts(parts)
                {
                    return true;
                }
                // Responses 的嵌套 input 数组
                if let Some(parts) = msg["content"]["input"].as_array()
                    && scan_parts(parts)
                {
                    return true;
                }
            }
            // Responses 顶层 input 可能直接是内容块数组
            if scan_parts(msgs) {
                return true;
            }
        }
    }
    false
}

/// 缓存键:SHA-256(版本 | 作用域 | 客户端协议 | 规范化请求体)。
/// serde_json 默认用 BTreeMap 存对象,序列化时键有序,同内容不同字段顺序
/// 得到同一个键。stream / stream_options 不参与(已保证非流式)。
/// 版本 v2:作用域语义收紧为 API key 隔离(9.7),旧 v1 键自然失效。
pub fn cache_key(scope: &str, client_protocol: &str, body: &Value) -> String {
    let mut normalized = body.clone();
    if let Some(obj) = normalized.as_object_mut() {
        obj.remove("stream");
        obj.remove("stream_options");
    }
    let mut h = Sha256::new();
    h.update(b"aikun-cache:v2|");
    h.update(scope.as_bytes());
    h.update(b"|");
    h.update(client_protocol.as_bytes());
    h.update(b"|");
    h.update(serde_json::to_vec(&normalized).unwrap_or_default());
    format!("aikun:cache:{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hdr(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("cache-control", v.parse().unwrap());
        h
    }

    #[test]
    fn key_is_order_insensitive_and_ignores_stream_flag() {
        let a = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}], "temperature": 0});
        let b: Value = serde_json::from_str(
            r#"{"temperature":0,"messages":[{"content":"hi","role":"user"}],"model":"m","stream":false}"#,
        )
        .unwrap();
        assert_eq!(cache_key("u", "openai", &a), cache_key("u", "openai", &b));
    }

    #[test]
    fn key_separates_scope_protocol_and_content() {
        let a = json!({"model": "m", "messages": [], "temperature": 0});
        let b = json!({"model": "m2", "messages": [], "temperature": 0});
        assert_ne!(cache_key("u1", "openai", &a), cache_key("u2", "openai", &a));
        assert_ne!(
            cache_key("u", "openai", &a),
            cache_key("u", "anthropic", &a)
        );
        assert_ne!(cache_key("u", "openai", &a), cache_key("u", "openai", &b));
    }

    /// 9.7 回归:同一用户的不同 API key,缓存键必须不同。
    #[test]
    fn key_isolates_api_keys_of_the_same_user() {
        let cache = GatewayCache::new(
            Arc::new(memory::MemoryCache::new(16)),
            CacheMode::Always,
            CacheScope::User,
            Duration::from_secs(60),
        );
        let body = json!({"model": "m", "temperature": 0});
        let h = HeaderMap::new();
        let k1 = cache
            .key_for(
                aikun_protocol::Protocol::OpenAi,
                "alice",
                Some("key-a"),
                &body,
                &h,
            )
            .unwrap();
        let k2 = cache
            .key_for(
                aikun_protocol::Protocol::OpenAi,
                "alice",
                Some("key-b"),
                &body,
                &h,
            )
            .unwrap();
        let k3 = cache
            .key_for(aikun_protocol::Protocol::OpenAi, "alice", None, &body, &h)
            .unwrap();
        assert_ne!(k1, k2, "不同 API key 必须隔离");
        assert_ne!(k1, k3, "key 调用与 JWT 会话必须隔离");
        // 不同用户(JWT)仍隔离
        let k4 = cache
            .key_for(aikun_protocol::Protocol::OpenAi, "bob", None, &body, &h)
            .unwrap();
        assert_ne!(k3, k4);
    }

    /// 9.7 回归:带 tools 或图片的请求默认不入缓存。
    #[test]
    fn tools_and_images_are_not_cached() {
        let none = HeaderMap::new();
        let with_tools = json!({
            "model": "m", "temperature": 0,
            "tools": [{"type": "function", "function": {"name": "f"}}]
        });
        let with_image = json!({
            "model": "m", "temperature": 0,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "hi"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,..."}}
            ]}]
        });
        let anthropic_image = json!({
            "model": "m", "temperature": 0,
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64"}}
            ]}]
        });
        let responses_image = json!({
            "model": "m", "temperature": 0,
            "input": [
                {"role": "user", "content": [{"type": "input_image", "image_url": "..."}]}
            ]
        });
        let plain = json!({"model": "m", "temperature": 0});
        assert!(!should_cache(CacheMode::Always, &with_tools, &none));
        assert!(!should_cache(CacheMode::Always, &with_image, &none));
        assert!(!should_cache(CacheMode::Always, &anthropic_image, &none));
        assert!(!should_cache(CacheMode::Always, &responses_image, &none));
        assert!(should_cache(CacheMode::Always, &plain, &none));
        // key_for 与 should_cache 同口径
        let cache = GatewayCache::new(
            Arc::new(memory::MemoryCache::new(16)),
            CacheMode::Always,
            CacheScope::User,
            Duration::from_secs(60),
        );
        assert!(
            cache
                .key_for(
                    aikun_protocol::Protocol::OpenAi,
                    "u",
                    Some("k"),
                    &with_tools,
                    &none
                )
                .is_none()
        );
        assert!(
            cache
                .key_for(
                    aikun_protocol::Protocol::OpenAi,
                    "u",
                    Some("k"),
                    &with_image,
                    &none
                )
                .is_none()
        );
    }

    /// 键版本 v2:与旧 v1 前缀生成的键必然不同(9.7 语义收紧后旧键失效)。
    #[test]
    fn cache_key_version_bumped_to_v2() {
        let body = json!({"model": "m"});
        let mut h = Sha256::new();
        h.update(b"aikun-cache:v1|u|openai|");
        h.update(serde_json::to_vec(&body).unwrap_or_default());
        let v1 = format!("aikun:cache:{:x}", h.finalize());
        assert_ne!(v1, cache_key("u", "openai", &body));
    }

    #[test]
    fn should_cache_rules() {
        let none = HeaderMap::new();
        let zero = json!({"model": "m", "temperature": 0});
        let warm = json!({"model": "m", "temperature": 0.7});
        let unset = json!({"model": "m"});
        let streamed = json!({"model": "m", "temperature": 0, "stream": true});

        assert!(!should_cache(CacheMode::Off, &zero, &none));
        assert!(should_cache(CacheMode::Deterministic, &zero, &none));
        assert!(!should_cache(CacheMode::Deterministic, &warm, &none));
        assert!(!should_cache(CacheMode::Deterministic, &unset, &none));
        assert!(!should_cache(CacheMode::Deterministic, &streamed, &none));
        assert!(should_cache(CacheMode::Always, &warm, &none));
        assert!(!should_cache(CacheMode::Always, &streamed, &none));
        assert!(!should_cache(CacheMode::Always, &warm, &hdr("no-cache")));
        assert!(!should_cache(
            CacheMode::Always,
            &warm,
            &hdr("max-age=0, No-Store")
        ));
    }

    #[tokio::test]
    async fn gateway_cache_roundtrip_and_scope() {
        let cache = GatewayCache::new(
            Arc::new(memory::MemoryCache::new(16)),
            CacheMode::Deterministic,
            CacheScope::User,
            Duration::from_secs(60),
        );
        let body = json!({"model": "m", "temperature": 0});
        let h = HeaderMap::new();
        let k1 = cache
            .key_for(aikun_protocol::Protocol::OpenAi, "alice", None, &body, &h)
            .unwrap();
        let k2 = cache
            .key_for(aikun_protocol::Protocol::OpenAi, "bob", None, &body, &h)
            .unwrap();
        assert_ne!(k1, k2, "user scope must isolate users");
        let entry = CacheEntry {
            body: json!({"ok": true}),
            usage: (1, 2, 3, 0),
        };
        cache.put(&k1, entry.clone()).await;
        assert_eq!(cache.get(&k1).await, Some(entry));
        assert_eq!(cache.get(&k2).await, None);
    }

    #[tokio::test]
    async fn oversized_entries_are_not_cached() {
        let mut cache = GatewayCache::new(
            Arc::new(memory::MemoryCache::new(16)),
            CacheMode::Always,
            CacheScope::Global,
            Duration::from_secs(60),
        );
        cache.max_entry_bytes = 8;
        let entry = CacheEntry {
            body: json!({"text": "this is longer than eight bytes"}),
            usage: (0, 0, 0, 0),
        };
        cache.put("k", entry).await;
        assert_eq!(cache.get("k").await, None);
    }
}
