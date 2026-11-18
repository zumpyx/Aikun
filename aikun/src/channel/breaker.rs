//! 渠道熔断(refactor-plan 9.5,阶段 3 尾巴):连续失败 N 次的渠道在
//! 冷却期内**优先跳过**,冷却结束后恢复可选(半开:下一次失败重新计数,
//! 再达阈值再次熔断)。这是选路层的短周期保护,**不取代**自动禁用
//! (较长周期,见 selector::record_failure)——自动禁用写库改变 is_active,
//! 熔断只在进程内;管理端手动启停因此也不受熔断影响(独立状态)。
//!
//! 状态放进程内(单实例纪律,不外置 Redis),挂在本模块的全局单例上,
//! 与 registry 快照同级。计数口径与 consecutive_failures 一致:**429 永不
//! 计入**(上游限流不代表渠道不健康,selector::counts_toward_disable 判定
//! 后才调进来);客户端 4xx 同样不进。成功即清零。
//!
//! 全部候选都被熔断时选路回退为忽略熔断(加权照常)——熔断是"优先
//! 跳过"不是硬墙,不应把服务打成 503。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 熔断参数(启动时 [`configure`],进程内不变)。
#[derive(Clone, Copy)]
struct Params {
    threshold: u32,
    cooldown: Duration,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            threshold: 3,
            cooldown: Duration::from_secs(30),
        }
    }
}

/// 单渠道熔断状态(closed = 条目不存在)。
#[derive(Clone, Copy, Debug, PartialEq)]
struct BreakerState {
    consecutive: u32,
    /// 熔断打开至该时刻;此前(含)视为跳过。
    open_until: Option<Instant>,
}

/// 纯状态机(单测锁定):一次失败/成功后的状态转移。
/// - 失败:consecutive + 1;达阈值 → open_until = now + cooldown。
/// - 成功:清零(closed)。
/// - 冷却过期后的失败:重新计数(半开后的失败从 1 数起)。
fn step(
    state: Option<BreakerState>,
    failure: bool,
    p: Params,
    now: Instant,
) -> Option<BreakerState> {
    if !failure {
        return None;
    }
    let mut s = state.unwrap_or(BreakerState {
        consecutive: 0,
        open_until: None,
    });
    // 上一轮熔断已过期 → 半开,失败从 1 重新计数。
    if s.open_until.is_some_and(|u| u <= now) {
        s.consecutive = 0;
    }
    s.consecutive += 1;
    s.open_until = if s.consecutive >= p.threshold {
        Some(now + p.cooldown)
    } else {
        None
    };
    Some(s)
}

/// 该渠道当前是否应被选路跳过(open 且未过期)。
fn skipped(state: Option<BreakerState>, now: Instant) -> bool {
    state.is_some_and(|s| s.open_until.is_some_and(|u| u > now))
}

struct Breaker {
    params: Mutex<Params>,
    states: Mutex<HashMap<String, BreakerState>>,
}

static BREAKER: OnceLock<Breaker> = OnceLock::new();

fn shared() -> &'static Breaker {
    BREAKER.get_or_init(|| Breaker {
        params: Mutex::new(Params::default()),
        states: Mutex::new(HashMap::new()),
    })
}

/// 启动时配置阈值与冷却(main.rs 调用;测试可覆盖)。
pub fn configure(threshold: u32, cooldown_secs: u64) {
    let mut p = shared().params.lock().unwrap();
    *p = Params {
        threshold,
        cooldown: Duration::from_secs(cooldown_secs),
    };
}

/// 记一次计入健康的失败(selector::record_failure 在
/// counts_toward_disable 判定通过后调用;429/客户端 4xx 不进)。
pub fn record_failure(provider_id: &str) {
    let p = *shared().params.lock().unwrap();
    let now = Instant::now();
    let mut states = shared().states.lock().unwrap();
    // 上限防御:渠道删除后的残留条目,超限时清一轮 closed(进程内
    // 渠道数量级很小,正常路径永不触发)。
    if states.len() > 1024 {
        states.retain(|_, s| s.open_until.is_some_and(|u| u > now));
    }
    let next = step(states.get(provider_id).copied(), true, p, now);
    match next {
        Some(s) => {
            states.insert(provider_id.to_string(), s);
        }
        None => {
            states.remove(provider_id);
        }
    }
}

/// 记一次成功:清零(冷却中的渠道被回退选中并成功,立即恢复)。
pub fn record_success(provider_id: &str) {
    shared().states.lock().unwrap().remove(provider_id);
}

/// 该渠道当前是否被熔断跳过。
pub fn is_skipped(provider_id: &str) -> bool {
    let now = Instant::now();
    let states = shared().states.lock().unwrap();
    skipped(states.get(provider_id).copied(), now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(threshold: u32, cooldown_secs: u64) -> Params {
        Params {
            threshold,
            cooldown: Duration::from_secs(cooldown_secs),
        }
    }

    fn state(consecutive: u32, open_until: Option<Instant>) -> Option<BreakerState> {
        Some(BreakerState {
            consecutive,
            open_until,
        })
    }

    #[test]
    fn step_truth_table() {
        let t0 = Instant::now();
        let p = params(3, 30);

        // 失败累计:1 → 2 → 3(达阈值,开)
        let s1 = step(None, true, p, t0);
        assert_eq!(s1, state(1, None));
        let s2 = step(s1, true, p, t0);
        assert_eq!(s2, state(2, None));
        let s3 = step(s2, true, p, t0);
        assert_eq!(
            s3,
            state(3, Some(t0 + Duration::from_secs(30))),
            "达阈值打开冷却"
        );

        // 成功清零(closed)
        assert_eq!(step(s2, false, p, t0), None);
        assert_eq!(step(s3, false, p, t0), None);

        // 冷却过期后的失败:半开重新计数
        let later = t0 + Duration::from_secs(31);
        let s4 = step(s3, true, p, later);
        assert_eq!(s4, state(1, None), "半开后失败从 1 数起");

        // 冷却未过期时再失败:仍在窗口内,计数继续(用于回退选中又失败)
        let mid = t0 + Duration::from_secs(5);
        let s5 = step(s3, true, p, mid);
        assert_eq!(
            s5.map(|s| (s.consecutive, s.open_until)),
            Some((4, Some(t0 + Duration::from_secs(35)))),
            "窗口内失败刷新冷却"
        );

        // threshold=1:单次失败即开
        let p1 = params(1, 10);
        assert_eq!(
            step(None, true, p1, t0),
            state(1, Some(t0 + Duration::from_secs(10)))
        );
    }

    #[test]
    fn skipped_window_semantics() {
        let t0 = Instant::now();
        let opened = state(3, Some(t0 + Duration::from_secs(30)));
        assert!(skipped(opened, t0), "窗口内跳过");
        assert!(
            !skipped(opened, t0 + Duration::from_secs(30)),
            "到期(含)恢复可选"
        );
        assert!(!skipped(state(2, None), t0), "未达阈值不跳过");
        assert!(!skipped(None, t0), "closed 不跳过");
    }

    #[test]
    fn breaker_lifecycle() {
        configure(2, 60);
        let pid = "test-provider-lifecycle";
        assert!(!is_skipped(pid));
        record_failure(pid);
        assert!(!is_skipped(pid), "1 次未达阈值 2");
        record_failure(pid);
        assert!(is_skipped(pid), "2 次达阈值,冷却中");
        // 成功即恢复(回退选中并成功的路径)。
        record_success(pid);
        assert!(!is_skipped(pid));
    }
}
