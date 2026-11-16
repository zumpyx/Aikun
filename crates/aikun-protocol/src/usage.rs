//! usage 提取与缓存 token 拆分口径(三协议统一)。

use serde_json::Value;

#[allow(unused_imports)]
use crate::*;

/// 上游 token 计数不可信:负数按 0,超出 i32 范围的值做饱和转换,
/// 替代裸 `as i32`(后者会回绕成负数,污染计费与统计)。
pub fn clamp_token_count(v: Option<i64>) -> i32 {
    v.unwrap_or(0).clamp(0, i32::MAX as i64) as i32
}

/// Extract (prompt_uncached, completion, total, cached) token usage from a
/// response in either protocol (OpenAI `prompt_tokens`/`completion_tokens` or
/// Anthropic `input_tokens`/`output_tokens`)。
/// 缓存拆分:OpenAI 的 prompt_tokens 含命中缓存的部分,按
/// prompt_tokens_details.cached_tokens 拆出单独计价;Anthropic 的
/// input_tokens 本就不含缓存,cache_read_input_tokens 记为缓存,
/// cache_creation_input_tokens(写缓存)按输入价并入未缓存输入(近似)。
/// 上游数值不可信:负数按 0 处理,超出 i32 范围的值做饱和转换,避免污染计费与统计。
pub fn extract_usage_any(resp: &Value) -> (i32, i32, i32, i32) {
    let clamp = clamp_token_count;
    let u = &resp["usage"];
    let completion = clamp(
        u["completion_tokens"]
            .as_i64()
            .or_else(|| u["output_tokens"].as_i64()),
    );
    if !u["prompt_tokens"].is_null() {
        // OpenAI:prompt_tokens 含缓存,拆出后不得为负(上游数据不可信)
        let prompt = clamp(u["prompt_tokens"].as_i64());
        let cached = clamp(u["prompt_tokens_details"]["cached_tokens"].as_i64());
        let prompt_uncached = (prompt - cached).max(0);
        let total = u["total_tokens"]
            .as_i64()
            .map(|t| clamp(Some(t)))
            .unwrap_or(prompt.saturating_add(completion));
        (prompt_uncached, completion, total, cached)
    } else if !u["input_tokens"].is_null()
        && (!u["input_tokens_details"].is_null() || !u["total_tokens"].is_null())
    {
        // Responses API:input_tokens 同样含缓存,按 input_tokens_details
        // .cached_tokens 拆出。特征判定先于 Anthropic 分支:Anthropic 的
        // usage 没有 total_tokens/input_tokens_details,不会误入此分支。
        // input_tokens 必须存在:非标 OpenAI 缺 prompt_tokens 但带
        // total_tokens 时,不能凭 total 误入此分支把输入读成 0。
        let prompt = clamp(u["input_tokens"].as_i64());
        let cached = clamp(u["input_tokens_details"]["cached_tokens"].as_i64());
        let prompt_uncached = (prompt - cached).max(0);
        let total = u["total_tokens"]
            .as_i64()
            .map(|t| clamp(Some(t)))
            .unwrap_or(prompt.saturating_add(completion));
        (prompt_uncached, completion, total, cached)
    } else {
        // Anthropic:input_tokens 不含缓存;cache_read 按缓存价,
        // cache_creation 按输入价并入未缓存输入(近似)
        let cached = clamp(u["cache_read_input_tokens"].as_i64());
        let prompt_uncached = clamp(u["input_tokens"].as_i64())
            .saturating_add(clamp(u["cache_creation_input_tokens"].as_i64()));
        let total = prompt_uncached
            .saturating_add(cached)
            .saturating_add(completion);
        (prompt_uncached, completion, total, cached)
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::test_util::{out_types, sse, sse_typed};
    // ---- extract_usage_any ----

    #[test]
    fn usage_extraction_both_protocols() {
        let oa = json!({"usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}});
        assert_eq!(extract_usage_any(&oa), (1, 2, 3, 0));
        let an = json!({"usage": {"input_tokens": 4, "output_tokens": 5}});
        assert_eq!(extract_usage_any(&an), (4, 5, 9, 0)); // total derived
        let none = json!({});
        assert_eq!(extract_usage_any(&none), (0, 0, 0, 0));
    }

    #[test]
    fn usage_extraction_splits_cached_tokens() {
        // OpenAI:prompt_tokens 含缓存,按 prompt_tokens_details.cached_tokens 拆出
        let oa = json!({"usage": {
            "prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12,
            "prompt_tokens_details": {"cached_tokens": 4}
        }});
        assert_eq!(extract_usage_any(&oa), (6, 2, 12, 4));
        // cached 超过 prompt_tokens 时未缓存部分饱和为 0(上游数据不可信)
        let oa_bad = json!({"usage": {
            "prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3,
            "prompt_tokens_details": {"cached_tokens": 5}
        }});
        assert_eq!(extract_usage_any(&oa_bad), (0, 2, 3, 5));
        // Anthropic:cache_read 按缓存价,cache_creation 按输入价并入未缓存输入;
        // total = 未缓存输入 + 缓存 + 输出
        let an = json!({"usage": {
            "input_tokens": 4, "output_tokens": 5,
            "cache_read_input_tokens": 7, "cache_creation_input_tokens": 3
        }});
        assert_eq!(extract_usage_any(&an), (7, 5, 19, 7));
        // 负数缓存字段按 0 钳制
        let an_neg = json!({"usage": {"input_tokens": 4, "output_tokens": 5, "cache_read_input_tokens": -2}});
        assert_eq!(extract_usage_any(&an_neg), (4, 5, 9, 0));
    }

    #[test]
    fn extract_usage_responses_shape_splits_cache() {
        // Responses:input_tokens 含缓存,按 input_tokens_details 拆出
        let resp = json!({"usage": {"input_tokens": 10, "output_tokens": 4, "total_tokens": 14,
                                    "input_tokens_details": {"cached_tokens": 6}}});
        assert_eq!(extract_usage_any(&resp), (4, 4, 14, 6));
        // Anthropic 形状不受影响:input_tokens 不含缓存,cache_read 单独计价
        let an = json!({"usage": {"input_tokens": 10, "output_tokens": 4, "cache_read_input_tokens": 6}});
        assert_eq!(extract_usage_any(&an), (10, 4, 20, 6));
    }

    #[test]
    fn usage_extraction_nonstandard_openai_not_misread_as_responses() {
        // 非标 OpenAI:缺 prompt_tokens/input_tokens 但带 total_tokens,
        // 不得误入 Responses 分支把输入读成 0 并照单全收 total;
        // 按 Anthropic 分支兜底(输入未知按 0,total 由分项推导)。
        let weird = json!({"usage": {"completion_tokens": 5, "total_tokens": 15}});
        assert_eq!(extract_usage_any(&weird), (0, 5, 5, 0));
        let only_total = json!({"usage": {"total_tokens": 10}});
        assert_eq!(extract_usage_any(&only_total), (0, 0, 0, 0));
        // 带 input_tokens 的 Responses 形状不受影响
        let responses =
            json!({"usage": {"input_tokens": 7, "output_tokens": 3, "total_tokens": 10}});
        assert_eq!(extract_usage_any(&responses), (7, 3, 10, 0));
    }
}
