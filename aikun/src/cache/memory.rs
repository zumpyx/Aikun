//! 进程内缓存后端:HashMap + 插入顺序队列,TTL 过期 + 条目数上限
//! (超限时先清过期项,再按插入顺序淘汰最旧的)。零外部依赖。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{CacheBackend, CacheEntry, CacheFuture};

pub struct MemoryCache {
    max_entries: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    map: HashMap<String, (Instant, CacheEntry)>,
    order: VecDeque<String>,
}

impl MemoryCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries: max_entries.max(1),
            inner: Mutex::new(Inner::default()),
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().map(|i| i.map.len()).unwrap_or(0)
    }

    fn get_sync(&self, key: &str) -> Option<CacheEntry> {
        let mut inner = self.inner.lock().ok()?;
        match inner.map.get(key) {
            Some((expires, entry)) if *expires > Instant::now() => Some(entry.clone()),
            Some(_) => {
                inner.map.remove(key);
                None
            }
            None => None,
        }
    }

    fn set_sync(&self, key: &str, entry: CacheEntry, ttl: Duration) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let now = Instant::now();
        if !inner.map.contains_key(key) && inner.map.len() >= self.max_entries {
            inner.map.retain(|_, (exp, _)| *exp > now);
            while inner.map.len() >= self.max_entries {
                match inner.order.pop_front() {
                    Some(old) => {
                        inner.map.remove(&old);
                    }
                    None => break,
                }
            }
        }
        if inner
            .map
            .insert(key.to_string(), (now + ttl, entry))
            .is_none()
        {
            inner.order.push_back(key.to_string());
        }
        // order 里可能残留已过期/已淘汰的键:长度明显超过 map 时整理一次。
        if inner.order.len() > inner.map.len() * 2 + 16 {
            let Inner { map, order } = &mut *inner;
            order.retain(|k| map.contains_key(k));
        }
    }
}

impl CacheBackend for MemoryCache {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn get<'a>(&'a self, key: &'a str) -> CacheFuture<'a, Option<CacheEntry>> {
        Box::pin(async move { self.get_sync(key) })
    }

    fn set<'a>(&'a self, key: &'a str, entry: CacheEntry, ttl: Duration) -> CacheFuture<'a, ()> {
        Box::pin(async move { self.set_sync(key, entry, ttl) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn e(n: i32) -> CacheEntry {
        CacheEntry {
            body: json!({"n": n}),
            usage: (n, 0, n, 0),
        }
    }

    #[test]
    fn ttl_expiry() {
        let c = MemoryCache::new(10);
        c.set_sync("a", e(1), Duration::from_millis(0));
        assert_eq!(c.get_sync("a"), None);
        c.set_sync("b", e(2), Duration::from_secs(60));
        assert_eq!(c.get_sync("b"), Some(e(2)));
    }

    #[test]
    fn capacity_evicts_oldest() {
        let c = MemoryCache::new(2);
        let ttl = Duration::from_secs(60);
        c.set_sync("a", e(1), ttl);
        c.set_sync("b", e(2), ttl);
        c.set_sync("c", e(3), ttl);
        assert_eq!(c.len(), 2);
        assert_eq!(c.get_sync("a"), None);
        assert_eq!(c.get_sync("c"), Some(e(3)));
        // 覆盖已有键不触发淘汰
        c.set_sync("c", e(4), ttl);
        assert_eq!(c.get_sync("b"), Some(e(2)));
        assert_eq!(c.get_sync("c"), Some(e(4)));
    }
}
