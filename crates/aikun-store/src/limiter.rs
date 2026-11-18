//! 进程内限流组件(roadmap 阶段 3 "state.rs Limiter" 的落位形态)。
//!
//! 合并 AppState 此前散落的四个 `Arc<Mutex<HashMap>>`(login_attempts /
//! api_key_rate / redeem_attempts / api_key_inflight)为统一组件,统一
//! "访问时清理 + 删除时清理"策略:
//! * [`SlidingWindows`]:滑动窗口计数(登录失败 / 兑换失败 / API key RPM),
//!   语义与迁移前逐处手写一致——retain 窗口外、空条目回收、锁中毒恢复
//!   (`into_inner`,不 fail-open 也不 panic);
//! * [`Counters`]:并发槽位(acquire/Drop 释放,归零移除条目);
//! * [`Limiters`]:四实例的聚合,AppState 持有一份。
//!
//! 放在本 crate 的原因:aikun-admin(登录/兑换)与二进制(admission 的
//! RPM/并发)都要持有同一实例,类型必须落在双方共同的依赖里;本 crate
//! 已承载口令哈希等运行时设施 precedent,此模块零 DB 依赖。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 滑动窗口计数(key -> 窗口内事件时刻)。
pub struct SlidingWindows {
    inner: Mutex<HashMap<String, Vec<Instant>>>,
}

impl Default for SlidingWindows {
    fn default() -> Self {
        Self::new()
    }
}

impl SlidingWindows {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Vec<Instant>>> {
        // 锁中毒(持锁线程 panic)取回数据继续工作:限流状态不值得panic
        // 整个进程,也不静默放行。
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// key 窗口内事件数(不含本次;顺手清理该 key 的过期项与空条目)。
    pub fn count(&self, key: &str, window: Duration) -> usize {
        let now = Instant::now();
        let mut map = self.lock();
        match map.get_mut(key) {
            Some(v) => {
                v.retain(|t| now.duration_since(*t) < window);
                let n = v.len();
                if n == 0 {
                    map.remove(key);
                }
                n
            }
            None => 0,
        }
    }

    /// 原子"判定 + 记一次":窗口内计数(不含本次)已达 `max` 则**不记**并
    /// 返回 false(被限流);否则记一次并返回 true。单次持锁完成,并发下
    /// 与逐处手写的"retain → 判 → push"语义一致。
    pub fn try_hit(&self, key: &str, window: Duration, max: usize) -> bool {
        let now = Instant::now();
        let mut map = self.lock();
        let v = map.entry(key.to_string()).or_default();
        v.retain(|t| now.duration_since(*t) < window);
        if v.len() >= max {
            if v.is_empty() {
                map.remove(key);
            }
            return false;
        }
        v.push(now);
        true
    }

    /// 无条件记一次(登录/兑换失败路径:失败总是要记的,判定在调用方)。
    pub fn record(&self, key: &str) {
        self.lock()
            .entry(key.to_string())
            .or_default()
            .push(Instant::now());
    }

    /// 批量记(auth 的用户名 + IP 双维度 key)。
    pub fn record_many(&self, keys: &[String]) {
        let now = Instant::now();
        let mut map = self.lock();
        for k in keys {
            map.entry(k.clone()).or_default().push(now);
        }
    }

    /// 删除条目(成功即清零 / 删 API key 时清 RPM 死条目)。
    pub fn clear(&self, key: &str) {
        self.lock().remove(key);
    }

    /// 429 `Retry-After` 建议秒数:窗口内最早一次事件到期为止,向上取整、
    /// 至少 1s(api-mapping 全局语义 #12)。
    pub fn retry_after_secs(&self, key: &str, window: Duration) -> u64 {
        let now = Instant::now();
        let map = self.lock();
        map.get(key)
            .and_then(|v| v.first())
            .map(|oldest| now.duration_since(*oldest))
            .map(|elapsed| {
                let rem = window.saturating_sub(elapsed);
                rem.as_secs() + u64::from(rem.subsec_nanos() > 0)
            })
            .unwrap_or(1)
            .max(1)
    }

    /// 全表机会式清理(limited 类调用顺手做,回收无流量 key 的空/过期条目)。
    pub fn sweep(&self, window: Duration) {
        let now = Instant::now();
        let mut map = self.lock();
        map.retain(|_, v| {
            v.retain(|t| now.duration_since(*t) < window);
            !v.is_empty()
        });
    }
}

/// 并发槽位计数(key -> 在途数)。
pub struct Counters {
    inner: Mutex<HashMap<String, usize>>,
}

impl Default for Counters {
    fn default() -> Self {
        Self::new()
    }
}

impl Counters {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, usize>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 占用槽位:在途数已达 `max` 返回 None(并发超限);否则 +1 并返回
    /// guard(Drop 时 -1,归零移除条目——"删除时清理")。
    pub fn acquire(self: &Arc<Self>, key: &str, max: usize) -> Option<CounterGuard> {
        let mut map = self.lock();
        let n = map.entry(key.to_string()).or_insert(0);
        if *n >= max {
            if *n == 0 {
                map.remove(key);
            }
            return None;
        }
        *n += 1;
        Some(CounterGuard {
            key: key.to_string(),
            counters: Arc::clone(self),
        })
    }
}

/// 并发槽位守卫:Drop 释放(请求结束/流终止)。
pub struct CounterGuard {
    key: String,
    counters: Arc<Counters>,
}

impl Drop for CounterGuard {
    fn drop(&mut self) {
        let mut map = self.counters.lock();
        if let Some(n) = map.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.key);
            }
        }
    }
}

/// AppState 持有的限流器聚合(四个实例,各自独立窗口语义由调用方定义)。
/// Clone = 四个 Arc 的浅拷贝(共享同一状态;AppState 派生 Clone 需要)。
#[derive(Clone)]
pub struct Limiters {
    /// 登录失败(用户名 + ip: 前缀双维度)。
    pub login_attempts: Arc<SlidingWindows>,
    /// API key RPM(60s 滑动窗口;删 key 时清理)。
    pub api_key_rate: Arc<SlidingWindows>,
    /// 兑换失败(keyed by user id)。
    pub redeem_attempts: Arc<SlidingWindows>,
    /// API key 并发在途计数。
    pub api_key_inflight: Arc<Counters>,
}

impl Default for Limiters {
    fn default() -> Self {
        Self::new()
    }
}

impl Limiters {
    pub fn new() -> Self {
        Self {
            login_attempts: Arc::new(SlidingWindows::new()),
            api_key_rate: Arc::new(SlidingWindows::new()),
            redeem_attempts: Arc::new(SlidingWindows::new()),
            api_key_inflight: Arc::new(Counters::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Duration = Duration::from_secs(60);

    #[test]
    fn sliding_window_count_record_and_clear() {
        let w = SlidingWindows::new();
        assert_eq!(w.count("k", W), 0);
        w.record("k");
        w.record("k");
        assert_eq!(w.count("k", W), 2);
        // 成功清零
        w.clear("k");
        assert_eq!(w.count("k", W), 0);
        // record_many(双维度 key)
        w.record_many(&["a".to_string(), "ip:b".to_string()]);
        assert_eq!(w.count("a", W), 1);
        assert_eq!(w.count("ip:b", W), 1);
    }

    #[test]
    fn sliding_window_try_hit_respects_max() {
        let w = SlidingWindows::new();
        assert!(w.try_hit("k", W, 2));
        assert!(w.try_hit("k", W, 2));
        assert!(!w.try_hit("k", W, 2), "第三次被限流");
        assert_eq!(w.count("k", W), 2, "被限流的尝试不占用窗口");
        // 窗口滑出后恢复
        // (语义由 count/retain 保证;此处锁定"被拒不 push"的关键行为)
        w.clear("k");
        assert!(w.try_hit("k", W, 2));
    }

    #[test]
    fn sliding_window_sweep_and_retry_after() {
        let w = SlidingWindows::new();
        assert_eq!(w.retry_after_secs("k", W), 1, "无记录至少 1s");
        w.record("k");
        let ra = w.retry_after_secs("k", W);
        assert!((1..=60).contains(&ra), "窗口剩余时长向上取整: {ra}");
        // sweep 清理空条目(不含未过期项)
        w.record("gone");
        w.clear("gone");
        w.sweep(W);
        assert_eq!(w.count("k", W), 1);
    }

    #[test]
    fn counters_acquire_and_release() {
        let c = Arc::new(Counters::new());
        let g1 = c.acquire("k", 2).expect("第一槽");
        let _g2 = c.acquire("k", 2).expect("第二槽");
        assert!(c.acquire("k", 2).is_none(), "第三槽超限");
        drop(g1);
        let _g3 = c.acquire("k", 2).expect("释放后可再占");
        // 归零移除条目(删除时清理)
        drop(_g2);
        drop(_g3);
        assert!(c.acquire("k", 2).is_some());
    }
}
