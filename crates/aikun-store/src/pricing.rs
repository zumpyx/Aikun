//! 价格表:默认价格快照 + 内存匹配(迁自二进制 `aikun/src/billing/mod.rs`
//! 的播种部分,plan 步骤 8;计费折算 `compute_cost` 在二进制 billing)。
//!
//! 匹配规则与 SQLite 侧 `billing::find_price` 逐值一致:精确命中优先;
//! 否则取以 `*` 结尾的通配条目中最长前缀;单独的 `*` 是兜底条目。
//!
//! 阶段 S:播种入口在 Surreal 侧(`surreal/billing_store.rs` 的
//! `seed_default_prices_if_empty`),快照与汇率折算统一收口到本文件的
//! [`default_price_rules`],消除双处定义(原 TODO 已结)。

use crate::PriceRule;

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

/// 内置默认价格条目(快照折算 + `*` 兜底条目;cached 为 None = 按
/// 输入价计)。播种入口只在**价格表为空**时消费(不覆盖手工数据)。
/// 快照解析失败(构建期问题)返回空集并留痕。
pub fn default_price_rules() -> Vec<PriceRule> {
    let mut rules: Vec<PriceRule> = match serde_json::from_str::<
        std::collections::BTreeMap<String, (f64, f64, Option<f64>)>,
    >(DEFAULT_PRICES_JSON)
    {
        Ok(map) => map
            .into_iter()
            .map(|(model, (p, c, k))| PriceRule {
                model,
                // 保留 4 位小数:低价模型折算后仍有有效精度
                prompt_price: (p * USD_TO_CNY_RATE * 1e4).round() / 1e4,
                completion_price: (c * USD_TO_CNY_RATE * 1e4).round() / 1e4,
                // 缓存读价可空:快照缺省(null)存 NONE,按输入价计费
                cached_price: k.map(|v| (v * USD_TO_CNY_RATE * 1e4).round() / 1e4),
            })
            .collect(),
        Err(e) => {
            // 快照文件随二进制编译内嵌,解析失败是构建期问题,不该在运行期发生
            tracing::error!("default_price_rules: embedded price snapshot is invalid: {e}");
            return Vec::new();
        }
    };
    // 兜底价直接以元入库(不经汇率换算);cached 为 None,缓存按输入价计
    rules.push(PriceRule {
        model: "*".to_string(),
        prompt_price: FALLBACK_PRICE.0,
        completion_price: FALLBACK_PRICE.1,
        cached_price: None,
    });
    rules
}

/// 在内存价格表里查模型价格,返回 (prompt, completion, cached)
/// (每 1M tokens)。cached 为 None 时缓存 token 按 prompt 价计。
/// 口径与 SQLite 侧 `billing::find_price` 一致(有逐值对照测试)。
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

    /// 默认快照的折算口径:gpt-4o 刊例价 $2.5/$10 × 7.2 = 18/72 元,
    /// 缓存读价 $1.25 × 7.2 = 9 元;`*` 兜底 3/7、cached 为 None。
    #[test]
    fn default_rules_conversion_and_fallback() {
        let rules = default_price_rules();
        assert!(
            rules.len() > 100,
            "expected a substantial default price set, got {}",
            rules.len()
        );
        let gpt4o = rules.iter().find(|r| r.model == "gpt-4o").unwrap();
        assert_eq!(
            (
                gpt4o.prompt_price,
                gpt4o.completion_price,
                gpt4o.cached_price
            ),
            (18.0, 72.0, Some(9.0))
        );
        let fallback = rules.iter().find(|r| r.model == "*").unwrap();
        assert_eq!(fallback.prompt_price, 3.0);
        assert_eq!(fallback.completion_price, 7.0);
        assert_eq!(fallback.cached_price, None);
    }
}
