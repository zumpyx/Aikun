//! 计费:模型价格匹配与费用计算。
//! 价格存 model_prices 表,单位为每 1M tokens 的价格(元)。
//! 匹配规则:精确命中优先;否则取以 `*` 结尾的通配条目中最长的前缀。
//! 无匹配返回 None,调用方按 cost=0 处理(不动用户余额)。
//!
//! 金额用 REAL(浮点):每请求费用极小,累计误差只影响展示分位;
//! 网关非银行系统,可接受此取舍。

use rusqlite::Connection;

/// 查模型价格,返回 (prompt_price, completion_price)(每 1M tokens)。
/// 查询/解码失败打 warn 并返回 None(按无价格处理),不再静默吞错。
pub fn find_price(conn: &Connection, model: &str) -> Option<(f64, f64)> {
    // 精确命中优先;QueryReturnedNoRows 是正常的"无精确匹配",不算错误。
    let exact: Option<(f64, f64)> = match conn.query_row(
        "SELECT prompt_price, completion_price FROM model_prices WHERE model = ?1",
        rusqlite::params![model],
        |row| Ok((row.get(0)?, row.get(1)?)),
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
    let mut best: Option<(usize, f64, f64)> = None;
    let mut stmt = match conn
        .prepare("SELECT model, prompt_price, completion_price FROM model_prices WHERE model LIKE '%*'")
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
        if !prefix.is_empty() && model.starts_with(prefix) {
            let len = prefix.len();
            if best.as_ref().is_none_or(|(l, _, _)| len > *l) {
                best = Some((len, r.1, r.2));
            }
        }
    }
    best.map(|(_, p, c)| (p, c))
}

/// 由用量与价格折算费用(元)。
pub fn compute_cost(prompt_tokens: i32, completion_tokens: i32, prices: (f64, f64)) -> f64 {
    prompt_tokens as f64 / 1_000_000.0 * prices.0
        + completion_tokens as f64 / 1_000_000.0 * prices.1
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
                completion_price REAL NOT NULL DEFAULT 0
            );
            INSERT INTO model_prices VALUES
                ('1', 'gpt-4', 10.0, 30.0),
                ('2', 'gpt-*', 1.0, 2.0),
                ('3', 'gpt-4o-*', 3.0, 6.0),
                ('4', 'claude-*', 15.0, 75.0);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn exact_match_wins_over_wildcard() {
        let conn = setup();
        assert_eq!(find_price(&conn, "gpt-4"), Some((10.0, 30.0)));
    }

    #[test]
    fn longest_wildcard_prefix_wins() {
        let conn = setup();
        // gpt-4o-mini 同时命中 gpt-* 与 gpt-4o-*,取更长前缀
        assert_eq!(find_price(&conn, "gpt-4o-mini"), Some((3.0, 6.0)));
        assert_eq!(find_price(&conn, "gpt-3.5-turbo"), Some((1.0, 2.0)));
        assert_eq!(find_price(&conn, "claude-opus-4"), Some((15.0, 75.0)));
    }

    #[test]
    fn no_match_returns_none() {
        let conn = setup();
        assert_eq!(find_price(&conn, "llama-3"), None);
        assert_eq!(find_price(&conn, ""), None);
    }

    #[test]
    fn cost_calculation() {
        // 5 prompt + 3 completion tokens @ (10, 30)/1M = 0.00014 元
        let cost = compute_cost(5, 3, (10.0, 30.0));
        assert!((cost - 0.00014).abs() < 1e-12);
    }
}
