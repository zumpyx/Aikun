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

use rusqlite::Connection;

/// 元(f64)→ 微元(i64),四舍五入。仅用于 API/费率边界入库。
pub fn yuan_to_micro(yuan: f64) -> i64 {
    (yuan * 1_000_000.0).round() as i64
}

/// 微元(i64)→ 元(f64)。微元整除 1e6 在 f64 下精确,无需再修约。
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
            tracing::warn!("find_price: exact price query failed for model '{}': {}", model, e);
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

/// 内置默认价格快照(src/default_prices.json):
/// 源自 LiteLLM 的 model_prices_and_context_window.json(官方刊例价),
/// 筛选主流 chat 模型,单位为 USD / 1M tokens。属售价基准,非渠道成本价。
const DEFAULT_PRICES_JSON: &str = include_str!("default_prices.json");

/// 快照换算汇率(USD → CNY)。价格表以元计费,seed 时一次性折算;
/// 汇率漂移不会回溯,管理员可在价格表页面手工改价。
const USD_TO_CNY_RATE: f64 = 7.2;

/// 兜底价(元/1M tokens):LiteLLM 快照未收录的模型按此计费,
/// 以 * 通配条目入库,优先级最低,可在「计费」页修改或删除
/// (删除后无匹配模型回到按 0 元记账)。
const FALLBACK_PRICE: (f64, f64) = (3.0, 7.0);

/// 仅在价格表为空时导入内置默认价格(用户已有任何条目即跳过,
/// 不覆盖手工维护的数据,也不会复活被删除的内置条目)。
pub fn seed_default_prices(conn: &Connection) -> Result<usize, rusqlite::Error> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM model_prices", [], |row| row.get(0))?;
    if count > 0 {
        return Ok(0);
    }
    let prices: Vec<(String, f64, f64, Option<f64>)> = match serde_json::from_str::<
        std::collections::BTreeMap<String, (f64, f64, Option<f64>)>,
    >(DEFAULT_PRICES_JSON)
    {
        Ok(map) => map
            .into_iter()
            .map(|(model, (p, c, k))| {
                (
                    model,
                    // 保留 4 位小数:低价模型折算后仍有有效精度
                    (p * USD_TO_CNY_RATE * 1e4).round() / 1e4,
                    (c * USD_TO_CNY_RATE * 1e4).round() / 1e4,
                    // 缓存读价可空:快照缺省(null)存 NULL,按输入价计费
                    k.map(|v| (v * USD_TO_CNY_RATE * 1e4).round() / 1e4),
                )
            })
            .collect(),
        Err(e) => {
            // 快照文件随二进制编译内嵌,解析失败是构建期问题,不该在运行期发生
            tracing::error!("seed_default_prices: embedded price snapshot is invalid: {}", e);
            return Ok(0);
        }
    };
    let n = prices.len();
    let mut stmt = conn.prepare(
        "INSERT INTO model_prices (id, model, prompt_price, completion_price, cached_price) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (model, prompt, completion, cached) in prices {
        stmt.execute(rusqlite::params![uuid::Uuid::new_v4().to_string(), model, prompt, completion, cached])?;
    }
    // 兜底价直接以元入库(不经汇率换算);cached_price 存 NULL,缓存按输入价 3 元计
    stmt.execute(rusqlite::params![
        uuid::Uuid::new_v4().to_string(),
        "*",
        FALLBACK_PRICE.0,
        FALLBACK_PRICE.1,
        Option::<f64>::None
    ])?;
    Ok(n + 1)
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
        assert_eq!(find_price(&conn, "gpt-3.5-turbo"), Some((1.0, 2.0, Some(0.5))));
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
        assert_eq!(find_price(&conn, "gpt-3.5-turbo"), Some((1.0, 2.0, Some(0.5))));
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

    #[test]
    fn seed_imports_defaults_only_into_empty_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE model_prices (
                id TEXT PRIMARY KEY, model TEXT NOT NULL UNIQUE,
                prompt_price REAL NOT NULL DEFAULT 0,
                completion_price REAL NOT NULL DEFAULT 0,
                cached_price REAL
            );",
        )
        .unwrap();
        // 空表:全量导入
        let n = seed_default_prices(&conn).unwrap();
        assert!(n > 100, "expected a substantial default price set, got {}", n);
        let gpt4o: (f64, f64, Option<f64>) = conn
            .query_row(
                "SELECT prompt_price, completion_price, cached_price FROM model_prices WHERE model = 'gpt-4o'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        // gpt-4o 刊例价 $2.5/$10 × 7.2 = 18/72 元;缓存读价 $1.25 × 7.2 = 9 元
        assert_eq!(gpt4o, (18.0, 72.0, Some(9.0)));
        // 兜底条目:* → 3/7 元,cached_price 为 NULL,未知模型按此计费
        assert_eq!(find_price(&conn, "some-unknown-model"), Some((3.0, 7.0, None)));
        // 非空表:跳过,不覆盖手工数据
        conn.execute("DELETE FROM model_prices WHERE model != 'gpt-4o'", [])
            .unwrap();
        conn.execute("UPDATE model_prices SET prompt_price = 1.0 WHERE model = 'gpt-4o'", [])
            .unwrap();
        assert_eq!(seed_default_prices(&conn).unwrap(), 0);
        let kept: f64 = conn
            .query_row("SELECT prompt_price FROM model_prices WHERE model = 'gpt-4o'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(kept, 1.0);
    }
}
