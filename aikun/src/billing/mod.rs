//! 计费:模型价格匹配与费用计算。
//! 价格存 model_price 表,单位为每 1M tokens 的价格(元)。
//! 匹配规则:精确命中优先;否则取以 `*` 结尾的通配条目中最长的前缀;
//! 单独的 `*` 是兜底条目,匹配一切但优先级最低。
//! 无匹配返回 None,调用方按 cost=0 处理(不动用户余额)。
//!
//! 金额口径:DB 一律存整数微元(1 元 = 1,000,000 微元,与前端 fmtCost
//! 的 6 位小数精度一致),浮点只存在于价格费率(元/1M tokens)与对外 API
//! (元)两侧边界。单请求费用常远小于 1 分,按分取整会让大量请求免费,
//! 故取微元;整数累加/扣减无浮点误差。转换只走 yuan_to_micro/micro_to_yuan。
//!
//! 阶段 S:价格表读侧经 SurrealDB 能力 trait(async;docs/surreal-migration.md
//! §六),直查 SQL 已随 SQLite 后端移除;缓存结构(世代 + 表令牌)不变。

use std::sync::{Arc, OnceLock, RwLock};

use aikun_store::surreal::billing_store::BillingStore;
use aikun_store::{CacheToken, PriceRule};

/// 元(f64)→ 微元(i64),四舍五入。仅用于 API/费率边界入库。
pub fn yuan_to_micro(yuan: f64) -> i64 {
    (yuan * 1_000_000.0).round() as i64
}

/// 微元(i64)→ 元(f64)。微元整除 1e6 在 f64 下精确,无需再修约。
/// 生产调用面已迁 crates/aikun-admin(impl 各自持有同口径助手);本函数
/// 由本模块单测与 yuan_to_micro 成对保留(store 收编时统一)。
#[allow(dead_code)]
pub fn micro_to_yuan(micro: i64) -> f64 {
    micro as f64 / 1_000_000.0
}

/// 由用量与价格折算费用,返回整数微元(DB cost/balance 的存储口径)。
/// prompt_tokens 为未命中缓存的输入 token,cached_tokens 按 cached_price
/// 单独计价,cached_price 为 None 时按输入价计。浮点只用于中间折算,
/// 结果一次性 round 到微元,长期累加无浮点漂移。
pub fn compute_cost(
    prompt_tokens: i32,
    cached_tokens: i32,
    completion_tokens: i32,
    prices: (f64, f64, Option<f64>),
) -> i64 {
    let yuan = prompt_tokens as f64 / 1_000_000.0 * prices.0
        + cached_tokens as f64 / 1_000_000.0 * prices.2.unwrap_or(prices.0)
        + completion_tokens as f64 / 1_000_000.0 * prices.1;
    yuan_to_micro(yuan)
}

// ---------- 价格表缓存(roadmap 阶段 3;结构不变,读侧 async) ----------
//
// 全量规则进程内缓存:计费热路径每请求一次内存匹配。失效检测与渠道
// 拓扑快照同构:管理端价格写世代(aikun-admin 写路径成功后递增)+
// 存储层表令牌(count + max updated_at 聚合,捕获夹具/脚本直写)。
// 回源失败不缓存失败态(下一笔重试)。

struct PriceTable {
    state: RwLock<PriceState>,
}

struct PriceState {
    /// 缓存属主(存储实例序号):不同库(单测的多实例)即使表令牌相同
    /// 也不共享命中。
    owner: u64,
    epoch: u64,
    token: Option<CacheToken>,
    rules: Arc<Vec<PriceRule>>,
}

static PRICE_TABLE: OnceLock<PriceTable> = OnceLock::new();

fn price_table() -> &'static PriceTable {
    PRICE_TABLE.get_or_init(|| PriceTable {
        state: RwLock::new(PriceState {
            owner: 0,
            epoch: 0,
            token: None,
            rules: Arc::new(Vec::new()),
        }),
    })
}

/// 测试钩子:清空价格缓存。(阶段 S 切换后暂无调用点——缓存失效的
/// 库内行为由 aikun-store 的 surreal 单测覆盖;钩子为下一批恢复缓存
/// 对照测试保留。)
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn reset_price_table_for_test() {
    let mut st = price_table()
        .state
        .write()
        .unwrap_or_else(|e| e.into_inner());
    st.owner = 0;
    st.epoch = 0;
    st.token = None;
    st.rules = Arc::new(Vec::new());
}

/// 查模型价格(计费热路径):先比对 (存储实例, 世代, 表令牌),一致走
/// 内存匹配;变化则回源重载全量规则(单次往返)。令牌点查与规则拉取
/// 都是 Surreal 查询,天然可 await——不再需要 blocking 线程中转。
pub async fn find_price_cached(
    billing: &dyn BillingStore,
    store_id: u64,
    model: &str,
) -> Option<(f64, f64, Option<f64>)> {
    let table = price_table();
    let epoch = aikun_admin::admin_billing::price_table_epoch();
    let token = match billing.price_table_token().await {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!("price table token probe failed: {e}");
            None
        }
    };
    {
        let st = table.state.read().unwrap_or_else(|e| e.into_inner());
        if st.owner == store_id && st.epoch == epoch && st.token == token && token.is_some() {
            return aikun_store::pricing::find_in_rules(&st.rules, model);
        }
    }
    match billing.price_rules().await {
        Ok(rules) => {
            let mut st = table.state.write().unwrap_or_else(|e| e.into_inner());
            st.owner = store_id;
            st.epoch = epoch;
            st.token = token;
            let match_result = aikun_store::pricing::find_in_rules(&rules, model);
            st.rules = Arc::new(rules);
            match_result
        }
        Err(e) => {
            // 回源失败:不缓存失败态,下一笔重试(按无价格计费)。
            tracing::warn!("price table reload failed, billing at 0 for this request: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_calculation() {
        // 5 未缓存输入 + 2 缓存 + 3 输出 @ (10, 30, 缓存 5)/1M
        // = (5×10 + 2×5 + 3×30)/1M = 0.00015 元 = 150 微元
        assert_eq!(compute_cost(5, 2, 3, (10.0, 30.0, Some(5.0))), 150);
        // cached_price 为 NULL 时缓存 token 按输入价计:
        // (5×10 + 2×10 + 3×30)/1M = 0.00016 元 = 160 微元
        assert_eq!(compute_cost(5, 2, 3, (10.0, 30.0, None)), 160);
    }

    #[test]
    fn yuan_micro_conversion_roundtrip() {
        assert_eq!(yuan_to_micro(0.00015), 150);
        assert_eq!(yuan_to_micro(100.0), 100_000_000);
        assert_eq!(yuan_to_micro(-0.5), -500_000);
        // 亚微元零头四舍五入到微元
        assert_eq!(yuan_to_micro(0.0000004), 0);
        assert_eq!(yuan_to_micro(0.0000006), 1);
        assert_eq!(micro_to_yuan(150), 0.00015);
        assert_eq!(micro_to_yuan(-500_000), -0.5);
        // 微元口径在 f64 下精确,往返无损
        for m in [0, 1, 150, 999_999, 100_000_000, -123_456] {
            assert_eq!(yuan_to_micro(micro_to_yuan(m)), m);
        }
    }
}
pub mod ledger;
