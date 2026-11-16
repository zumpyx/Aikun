//! 响应缓存层(可插拔后端)。
//!
//! - 后端抽象为 [`CacheBackend`];默认 [`memory::MemoryCache`](进程内、
//!   零外部依赖,保持"单二进制"部署);`--features redis-cache` 编译后可选
//!   Redis 后端,供多实例共享缓存。
//! - 只缓存**非流式**、成功(200 且通过协议形状校验)的响应;缓存的是已经
//!   转换成客户端协议的响应体,所以键里包含客户端协议。
//! - 默认 `mode = off`(与重构前行为一致);`deterministic` 只缓存
//!   `temperature == 0` 的请求;`always` 缓存全部非流式请求。
//! - 请求头 `Cache-Control: no-cache` / `no-store` 跳过缓存。
//! - 默认按用户隔离(`scope = user`),避免不同用户之间互相看到对方的
//!   响应;`global` 仅建议在单租户部署下开启。
//! - 后端故障一律 fail-open:读失败按未命中处理,写失败只记日志。

pub mod memory;
#[cfg(feature = "redis-cache")]
pub mod redis;

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
    pub fn new(backend: Arc<dyn CacheBackend>, mode: CacheMode, scope: CacheScope, ttl: Duration) -> Self {
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
    pub fn key_for(
        &self,
        client_protocol: crate::protocol::Protocol,
        user_id: &str,
        body: &Value,
        headers: &HeaderMap,
    ) -> Option<String> {
        if !should_cache(self.mode, body, headers) {
            return None;
        }
        let scope = match self.scope {
            CacheScope::User => user_id,
            CacheScope::Global => "*",
        };
        Some(cache_key(scope, client_protocol.as_str(), body))
    }

    pub async fn get(&self, key: &str) -> Option<CacheEntry> {
        self.backend.get(key).await
    }

    /// 写入缓存;超过单条大小上限的响应不缓存(防止大响应撑爆内存)。
    pub async fn put(&self, key: &str, entry: CacheEntry) {
        let size = serde_json::to_vec(&entry.body).map(|v| v.len()).unwrap_or(usize::MAX);
        if size > self.max_entry_bytes {
            tracing::debug!("响应 {} 字节超过缓存单条上限,不缓存", size);
            return;
        }
        self.backend.set(key, entry, self.ttl).await
    }
}

fn build_backend(config: &AppConfig) -> Arc<dyn CacheBackend> {
    match config.cache_backend.as_str() {
        "redis" => {
            #[cfg(feature = "redis-cache")]
            {
                let url = config.redis_url.clone().unwrap_or_else(|| {
                    panic!("AIKUN_CACHE_BACKEND=redis 需要同时设置 AIKUN_REDIS_URL")
                });
                match redis::RedisCache::new(&url) {
                    Ok(c) => Arc::new(c),
                    Err(e) => panic!("Redis 缓存初始化失败: {}", e),
                }
            }
            #[cfg(not(feature = "redis-cache"))]
            {
                panic!("AIKUN_CACHE_BACKEND=redis 需要以 `--features redis-cache` 编译")
            }
        }
        _ => Arc::new(memory::MemoryCache::new(config.cache_max_entries)),
    }
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

/// 缓存策略判定。
pub fn should_cache(mode: CacheMode, body: &Value, headers: &HeaderMap) -> bool {
    if mode == CacheMode::Off || body["stream"].as_bool().unwrap_or(false) {
        return false;
    }
    if cache_bypass_requested(headers) {
        return false;
    }
    match mode {
        CacheMode::Off => false,
        CacheMode::Always => true,
        CacheMode::Deterministic => body["temperature"].as_f64() == Some(0.0),
    }
}

/// 缓存键:SHA-256(版本 | 作用域 | 客户端协议 | 规范化请求体)。
/// serde_json 默认用 BTreeMap 存对象,序列化时键有序,同内容不同字段顺序
/// 得到同一个键。stream / stream_options 不参与(已保证非流式)。
pub fn cache_key(scope: &str, client_protocol: &str, body: &Value) -> String {
    let mut normalized = body.clone();
    if let Some(obj) = normalized.as_object_mut() {
        obj.remove("stream");
        obj.remove("stream_options");
    }
    let mut h = Sha256::new();
    h.update(b"aikun-cache:v1|");
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
        assert_ne!(cache_key("u", "openai", &a), cache_key("u", "anthropic", &a));
        assert_ne!(cache_key("u", "openai", &a), cache_key("u", "openai", &b));
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
        assert!(!should_cache(CacheMode::Always, &warm, &hdr("max-age=0, No-Store")));
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
        let k1 = cache.key_for(crate::protocol::Protocol::OpenAi, "alice", &body, &h).unwrap();
        let k2 = cache.key_for(crate::protocol::Protocol::OpenAi, "bob", &body, &h).unwrap();
        assert_ne!(k1, k2, "user scope must isolate users");
        let entry = CacheEntry { body: json!({"ok": true}), usage: (1, 2, 3, 0) };
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
        let entry = CacheEntry { body: json!({"text": "this is longer than eight bytes"}), usage: (0, 0, 0, 0) };
        cache.put("k", entry).await;
        assert_eq!(cache.get("k").await, None);
    }
}
