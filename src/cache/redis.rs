//! Redis 缓存后端(`--features redis-cache`),多实例共享缓存时使用。
//! 连接 / 读写失败一律 fail-open(按未命中处理),不影响主流程。

use std::time::Duration;

use redis::AsyncCommands;

use super::{CacheBackend, CacheEntry, CacheFuture};

pub struct RedisCache {
    client: redis::Client,
}

impl RedisCache {
    pub fn new(url: &str) -> Result<Self, redis::RedisError> {
        Ok(Self { client: redis::Client::open(url)? })
    }
}

impl CacheBackend for RedisCache {
    fn name(&self) -> &'static str {
        "redis"
    }

    fn get<'a>(&'a self, key: &'a str) -> CacheFuture<'a, Option<CacheEntry>> {
        Box::pin(async move {
            let mut conn = match self.client.get_multiplexed_async_connection().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Redis 缓存连接失败(按未命中处理): {}", e);
                    return None;
                }
            };
            let raw: Option<String> = match conn.get(key).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("Redis 缓存读取失败(按未命中处理): {}", e);
                    return None;
                }
            };
            raw.and_then(|s| serde_json::from_str(&s).ok())
        })
    }

    fn set<'a>(&'a self, key: &'a str, entry: CacheEntry, ttl: Duration) -> CacheFuture<'a, ()> {
        Box::pin(async move {
            let Ok(payload) = serde_json::to_string(&entry) else {
                return;
            };
            let mut conn = match self.client.get_multiplexed_async_connection().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Redis 缓存连接失败(跳过写入): {}", e);
                    return;
                }
            };
            let res: Result<(), _> = redis::cmd("SET")
                .arg(key)
                .arg(payload)
                .arg("EX")
                .arg(ttl.as_secs().max(1))
                .query_async(&mut conn)
                .await;
            if let Err(e) = res {
                tracing::warn!("Redis 缓存写入失败: {}", e);
            }
        })
    }
}
