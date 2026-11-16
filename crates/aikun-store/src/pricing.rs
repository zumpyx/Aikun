//! 价格表:默认价格播种 + 内存匹配(迁自二进制 `aikun/src/billing/mod.rs`
//! 的播种部分,plan 步骤 8——db 层不再反向调用 billing,层次倒挂消除;
//! 计费折算 `compute_cost` 仍在二进制 billing)。
//!
//! 匹配规则与 SQLite 侧 `billing::find_price` 逐值一致:精确命中优先;
//! 否则取以 `*` 结尾的通配条目中最长前缀;单独的 `*` 是兜底条目。

use rusqlite::Connection;

use crate::{PriceBook, PriceRule, StoreError};

/// 内置默认价格快照(src/default_prices.json):源自 LiteLLM 的
/// model_prices_and_context_window.json(官方刊例价),筛选主流 chat
/// 模型,单位为 USD / 1M tokens。属售价基准,非渠道成本价。
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
            tracing::error!(
                "seed_default_prices: embedded price snapshot is invalid: {}",
                e
            );
            return Ok(0);
        }
    };
    let n = prices.len();
    let mut stmt = conn.prepare(
        "INSERT INTO model_prices (id, model, prompt_price, completion_price, cached_price) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (model, prompt, completion, cached) in prices {
        stmt.execute(rusqlite::params![
            uuid::Uuid::new_v4().to_string(),
            model,
            prompt,
            completion,
            cached
        ])?;
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

/// 在内存价格表里查模型价格,返回 (prompt, completion, cached)
/// (每 1M tokens)。cached 为 None 时缓存 token 按 prompt 价计。
/// 口径与 SQLite 侧 `billing::find_price` 一致(二进制有逐值对照测试)。
pub fn find_in_rules(rules: &[PriceRule], model: &str) -> Option<(f64, f64, Option<f64>)> {
    // 精确命中优先
    if let Some(r) = rules.iter().find(|r| r.model == model) {
        return Some((r.prompt_price, r.completion_price, r.cached_price));
    }
    // 通配:取最长前缀(最具体)的 * 条目;空前缀(条目为 *)是兜底价,
    // 匹配一切但优先级最低(长度 0)。
    rules
        .iter()
        .filter_map(|r| {
            let prefix = r.model.trim_end_matches('*');
            (prefix.is_empty() || model.starts_with(prefix)).then_some((prefix.len(), r))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, r)| (r.prompt_price, r.completion_price, r.cached_price))
}

impl PriceBook for crate::pool::DbPool {
    fn price_rules(&self) -> Result<Vec<PriceRule>, StoreError> {
        let conn = self.read().lock().map_err(|_| StoreError::Unavailable)?;
        let mut stmt = conn
            .prepare("SELECT model, prompt_price, completion_price, cached_price FROM model_prices")
            .map_err(|e| StoreError::Failed(e.to_string()))?;
        let rules = stmt
            .query_map([], |row| {
                Ok(PriceRule {
                    model: row.get(0)?,
                    prompt_price: row.get(1)?,
                    completion_price: row.get(2)?,
                    cached_price: row.get(3)?,
                })
            })
            .map_err(|e| StoreError::Failed(e.to_string()))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rules)
    }

    fn seed_default_prices_if_empty(&self) -> Result<usize, StoreError> {
        let conn = self.conn.lock().map_err(|_| StoreError::Unavailable)?;
        seed_default_prices(&conn).map_err(|e| StoreError::Failed(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Vec<PriceRule> {
        [
            ("gpt-4", 10.0, 30.0, None),
            ("gpt-*", 1.0, 2.0, Some(0.5)),
            ("gpt-4o-*", 3.0, 6.0, None),
            ("claude-*", 15.0, 75.0, None),
            ("*", 3.0, 7.0, None),
        ]
        .into_iter()
        .map(|(m, p, c, k)| PriceRule {
            model: m.to_string(),
            prompt_price: p,
            completion_price: c,
            cached_price: k,
        })
        .collect()
    }

    #[test]
    fn in_memory_matching_matches_sql_semantics() {
        let rs = rules();
        // 精确命中优先
        assert_eq!(find_in_rules(&rs, "gpt-4"), Some((10.0, 30.0, None)));
        // 最长通配前缀
        assert_eq!(find_in_rules(&rs, "gpt-4o-mini"), Some((3.0, 6.0, None)));
        assert_eq!(
            find_in_rules(&rs, "gpt-3.5-turbo"),
            Some((1.0, 2.0, Some(0.5)))
        );
        assert_eq!(
            find_in_rules(&rs, "claude-opus-4"),
            Some((15.0, 75.0, None))
        );
        // 兜底与边界输入
        assert_eq!(find_in_rules(&rs, "gp"), Some((3.0, 7.0, None)));
        assert_eq!(find_in_rules(&rs, ""), Some((3.0, 7.0, None)));
        assert_eq!(find_in_rules(&rs, "llama-3"), Some((3.0, 7.0, None)));
        // 无任何匹配(只有具体条目时)
        assert_eq!(
            find_in_rules(
                &[PriceRule {
                    model: "gpt-4".into(),
                    prompt_price: 1.0,
                    completion_price: 2.0,
                    cached_price: None,
                }],
                "other"
            ),
            None
        );
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
        assert!(
            n > 100,
            "expected a substantial default price set, got {}",
            n
        );
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
        let fallback = conn
            .query_row(
                "SELECT prompt_price, completion_price, cached_price FROM model_prices WHERE model = '*'",
                [],
                |row| Ok((row.get::<_, f64>(0)?, row.get::<_, f64>(1)?, row.get::<_, Option<f64>>(2)?)),
            )
            .unwrap();
        assert_eq!(fallback, (3.0, 7.0, None));
        // 非空表:跳过,不覆盖手工数据
        conn.execute("DELETE FROM model_prices WHERE model != 'gpt-4o'", [])
            .unwrap();
        conn.execute(
            "UPDATE model_prices SET prompt_price = 1.0 WHERE model = 'gpt-4o'",
            [],
        )
        .unwrap();
        assert_eq!(seed_default_prices(&conn).unwrap(), 0);
        let kept: f64 = conn
            .query_row(
                "SELECT prompt_price FROM model_prices WHERE model = 'gpt-4o'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1.0);
    }

    #[test]
    fn price_book_capability_loads_rules() {
        let pool =
            crate::pool::DbPool::open(":memory:", crate::crypto::KeyCipher::from_secret("t"))
                .unwrap();
        // 池初始化已播种默认价格
        let rules = PriceBook::price_rules(&pool).unwrap();
        assert!(!rules.is_empty());
        assert!(rules.iter().any(|r| r.model == "*"));
    }
}
