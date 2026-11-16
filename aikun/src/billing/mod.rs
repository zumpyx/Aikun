//! 计费:模型价格匹配与费用计算。
//! 价格存 model_prices 表,单位为每 1M tokens 的价格(元)。
//! 匹配规则:精确命中优先;否则取以 `*` 结尾的通配条目中最长的前缀;
//! 单独的 `*` 是兜底条目,匹配一切但优先级最低。
//! 无匹配返回 None,调用方按 cost=0 处理(不动用户余额)。
//!
//! 金额口径:DB 一律存整数微元(1 元 = 1,000,000 微元,与前端 fmtCost
//! 的 6 位小数精度一致),浮点只存在于价格费率(元/1M tokens)与对外 API
//! (元)两侧边界。单请求费用常远小于 1 分,按分取整会让大量请求免费,
//! 故取微元;整数累加/扣减无浮点误差。转换只走 yuan_to_micro/micro_to_yuan。

use std::sync::{Arc, OnceLock, RwLock};

use rusqlite::Connection;

use crate::db::DbPool;
use aikun_store::{CacheToken, CacheTokens, PriceBook};

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

/// 查模型价格,返回 (prompt_price, completion_price, cached_price)(每 1M tokens)。
/// cached_price 为 NULL 时缓存 token 按 prompt_price 计(见 compute_cost)。
/// 查询/解码失败打 warn 并返回 None(按无价格处理),不再静默吞错。
pub fn find_price(conn: &Connection, model: &str) -> Option<(f64, f64, Option<f64>)> {
    // 精确命中优先;QueryReturnedNoRows 是正常的"无精确匹配",不算错误。
    let exact: Option<(f64, f64, Option<f64>)> = match conn.query_row(
        "SELECT prompt_price, completion_price, cached_price FROM model_prices WHERE model = ?1",
        rusqlite::params![model],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ) {
        Ok(v) => Some(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => {
            tracing::warn!(
                "find_price: exact price query failed for model '{}': {}",
                model,
                e
            );
            return None;
        }
    };
    if exact.is_some() {
        return exact;
    }

    // 通配:取最长前缀(最具体)的 * 条目
    let mut best: Option<(usize, f64, f64, Option<f64>)> = None;
    let mut stmt = match conn
        .prepare("SELECT model, prompt_price, completion_price, cached_price FROM model_prices WHERE model LIKE '%*'")
    {
        Ok(stmt) => stmt,
        Err(e) => {
            tracing::warn!("find_price: wildcard price query prepare failed: {}", e);
            return None;
        }
    };
    let rows = match stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, Option<f64>>(3)?,
        ))
    }) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("find_price: wildcard price query failed: {}", e);
            return None;
        }
    };
    for r in rows {
        let r = match r {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("find_price: failed to decode price row: {}", e);
                continue;
            }
        };
        let prefix = r.0.trim_end_matches('*');
        // 空前缀(条目为 *)是兜底价:匹配一切,但优先级最低(长度 0)
        if prefix.is_empty() || model.starts_with(prefix) {
            let len = prefix.len();
            if best.as_ref().is_none_or(|(l, _, _, _)| len > *l) {
                best = Some((len, r.1, r.2, r.3));
            }
        }
    }
    best.map(|(_, p, c, k)| (p, c, k))
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

// 内置默认价格播种已移入仓储层(`aikun_store::pricing`,随 db 层吸收,
// plan 步骤 8):db 层不再反向调用 billing,层次倒挂消除。

// ---------- 价格表缓存(roadmap 阶段 3) ----------
//
// 全量规则进程内缓存:计费热路径每请求一次内存匹配(替代原来的精确 +
// 通配两次 SQL)。失效检测与渠道拓扑快照同构:管理端价格写世代
// (aikun-admin 写路径成功后递增)+ 存储层表令牌(行数/rowid/updated_at
// 聚合,捕获夹具/脚本直写)。重载失败回退 find_price 直查(旧路径)。

struct PriceTable {
    state: RwLock<PriceState>,
}

struct PriceState {
    /// 缓存属主(池实例序号):不同库(单测的多个 :memory: 池)即使表
    /// 令牌字符串相同也不共享命中。
    owner: u64,
    epoch: u64,
    token: Option<CacheToken>,
    rules: Arc<Vec<aikun_store::PriceRule>>,
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

/// 测试钩子:清空价格缓存。
#[cfg(test)]
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

/// 查模型价格(计费热路径):先比对 (池, 世代, 表令牌),一致走内存匹配;
/// 变化则回源重载全量规则。表令牌点查持读锁,调用方须在 blocking 线程
/// 上下文(与 find_price 同)。重载失败回退 find_price 直查。
pub fn find_price_cached(pool: &DbPool, model: &str) -> Option<(f64, f64, Option<f64>)> {
    let table = price_table();
    let epoch = aikun_admin::admin_billing::price_table_epoch();
    let token = match pool.price_table_token() {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!("price table token probe failed: {}", e);
            None
        }
    };
    {
        let st = table.state.read().unwrap_or_else(|e| e.into_inner());
        if st.owner == pool.id && st.epoch == epoch && st.token == token && token.is_some() {
            return aikun_store::pricing::find_in_rules(&st.rules, model);
        }
    }
    match pool.price_rules() {
        Ok(rules) => {
            let mut st = table.state.write().unwrap_or_else(|e| e.into_inner());
            st.owner = pool.id;
            st.epoch = epoch;
            st.token = token;
            let match_result = aikun_store::pricing::find_in_rules(&rules, model);
            st.rules = Arc::new(rules);
            match_result
        }
        Err(e) => {
            // 回源失败:回退旧路径(直查),不缓存失败态。
            tracing::warn!(
                "price table reload failed, falling back to direct query: {}",
                e
            );
            pool.read()
                .lock()
                .ok()
                .and_then(|conn| find_price(&conn, model))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE model_prices (
                id TEXT PRIMARY KEY, model TEXT NOT NULL UNIQUE,
                prompt_price REAL NOT NULL DEFAULT 0,
                completion_price REAL NOT NULL DEFAULT 0,
                cached_price REAL
            );
            INSERT INTO model_prices VALUES
                ('1', 'gpt-4', 10.0, 30.0, NULL),
                ('2', 'gpt-*', 1.0, 2.0, 0.5),
                ('3', 'gpt-4o-*', 3.0, 6.0, NULL),
                ('4', 'claude-*', 15.0, 75.0, NULL),
                ('5', '*', 3.0, 7.0, NULL);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn exact_match_wins_over_wildcard() {
        let conn = setup();
        assert_eq!(find_price(&conn, "gpt-4"), Some((10.0, 30.0, None)));
    }

    #[test]
    fn longest_wildcard_prefix_wins() {
        let conn = setup();
        // gpt-4o-mini 同时命中 gpt-* 与 gpt-4o-*,取更长前缀
        assert_eq!(find_price(&conn, "gpt-4o-mini"), Some((3.0, 6.0, None)));
        assert_eq!(
            find_price(&conn, "gpt-3.5-turbo"),
            Some((1.0, 2.0, Some(0.5)))
        );
        assert_eq!(find_price(&conn, "claude-opus-4"), Some((15.0, 75.0, None)));
    }

    #[test]
    fn fallback_also_matches_edge_inputs() {
        let conn = setup();
        // 不以任何通配前缀开头的输入只命中兜底
        assert_eq!(find_price(&conn, "gp"), Some((3.0, 7.0, None)));
        assert_eq!(find_price(&conn, ""), Some((3.0, 7.0, None)));
    }

    #[test]
    fn bare_star_is_lowest_priority_fallback() {
        let conn = setup();
        // 未知模型命中兜底价
        assert_eq!(find_price(&conn, "llama-3"), Some((3.0, 7.0, None)));
        // 有更具体的通配/精确条目时兜底不生效
        assert_eq!(
            find_price(&conn, "gpt-3.5-turbo"),
            Some((1.0, 2.0, Some(0.5)))
        );
        assert_eq!(find_price(&conn, "gpt-4"), Some((10.0, 30.0, None)));
    }

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

    // ---- 价格表缓存 ----

    fn cache_pool() -> crate::db::DbPool {
        let pool = crate::db::DbPool::new(&crate::config::AppConfig {
            database_url: ":memory:".into(),
            ..Default::default()
        })
        .unwrap();
        // 池初始化播种的默认价格清空,保证用例独立(夹具口径)
        pool.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM model_prices", [])
            .unwrap();
        pool
    }

    /// 缓存匹配与 SQL 直查(find_price)逐值一致;夹具直写 INSERT 经表
    /// 令牌失效,下一笔计费立即可见。
    #[test]
    fn cached_matching_matches_sql_and_invalidates_on_direct_write() {
        reset_price_table_for_test();
        let pool = cache_pool();
        pool.conn
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO model_prices (id, model, prompt_price, completion_price, cached_price) VALUES
                    ('1', 'gpt-4', 10.0, 30.0, NULL),
                    ('2', 'gpt-*', 1.0, 2.0, 0.5),
                    ('3', 'gpt-4o-*', 3.0, 6.0, NULL),
                    ('4', '*', 3.0, 7.0, NULL);",
            )
            .unwrap();

        // 先取 SQL 期望值并释放连接(:memory: 下读写是同一把连接锁,
        // 缓存路径内部还要取读者连接,不能并持)
        let expected: Vec<_> = {
            let conn = pool.read().lock().unwrap();
            ["gpt-4", "gpt-4o-mini", "gpt-3.5-turbo", "llama-3", "gp", ""]
                .iter()
                .map(|m| find_price(&conn, m))
                .collect()
        };
        for (m, want) in ["gpt-4", "gpt-4o-mini", "gpt-3.5-turbo", "llama-3", "gp", ""]
            .iter()
            .zip(expected)
        {
            assert_eq!(
                &find_price_cached(&pool, m),
                &want,
                "cache/SQL mismatch for {m:?}"
            );
        }

        // 直写新价(夹具/脚本路径):表令牌捕获 ⇒ 立即可见
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO model_prices (id, model, prompt_price, completion_price)
                 VALUES ('5', 'new-model', 8.0, 9.0)",
                [],
            )
            .unwrap();
        assert_eq!(
            find_price_cached(&pool, "new-model"),
            Some((8.0, 9.0, None))
        );
    }

    /// 管理端价格写世代:UPDATE 不改行数、updated_at 同秒也可能不变,
    /// 世代计数器兜底(与渠道拓扑同构)。
    #[test]
    fn price_epoch_bump_invalidates_cache() {
        reset_price_table_for_test();
        let pool = cache_pool();
        pool.conn
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO model_prices (id, model, prompt_price, completion_price) VALUES
                    ('p1', 'm1', 1.0, 2.0);",
            )
            .unwrap();
        assert_eq!(find_price_cached(&pool, "m1"), Some((1.0, 2.0, None)));

        // 同秒内改价且 updated_at 撞值:令牌可能不变,世代必须兜底
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE model_prices SET prompt_price = 5.0 WHERE model = 'm1'",
                [],
            )
            .unwrap();
        aikun_admin::admin_billing::bump_price_table_epoch();
        assert_eq!(find_price_cached(&pool, "m1"), Some((5.0, 2.0, None)));
    }
}
pub mod ledger;
