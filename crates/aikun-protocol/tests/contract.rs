//! 协议契约测试(refactor-plan 9.1):黄金样本比对完整输出,而非个别字段。
//!
//! 样本分三层(见 9.1 参照体系):
//! - 输入(request / upstream_response):手写,覆盖 tool_calls 与 usage(含缓存拆分);
//! - 期望输出(golden):由当前实现生成——`UPDATE_FIXTURES=1 cargo test -p aikun-protocol`
//!   重写;基线即契约,转换行为有意变更时必须显式更新 golden 并记 CHANGELOG;
//! - reference/:OpenAI 官方 spec 快照(MIT),形状下界参照,不参与断言。
//!
//! 挥发性字段(top-level id / created 由实现注入 uuid/时间戳)比对前统一摘除。
//! 流式样本属阶段 2(见 9.1 验收),此处先立非流式两方向。

use aikun_protocol::{Protocol, convert_request, convert_response};
use serde_json::Value;

fn load(dir: &str, name: &str) -> Value {
    let path = format!("tests/fixtures/{dir}/{name}");
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
        .unwrap()
}

/// 摘除 top-level 的 id / created(实现注入 uuid / 当前时间,不可作黄金断言)。
fn mask_volatile(mut v: Value) -> Value {
    if let Value::Object(map) = &mut v {
        map.remove("id");
        map.remove("created");
    }
    v
}

fn check_pair(dir: &str, from: Protocol, to: Protocol, client_model: &str) {
    let request = load(dir, "request.json");
    let upstream = load(dir, "upstream_response.json");

    let converted_request = convert_request(&request, from, to);
    let converted_response = convert_response(&upstream, from, to, client_model);

    let update = std::env::var("UPDATE_FIXTURES").as_deref() == Ok("1");
    for (name, value) in [
        ("expected_request.json", converted_request),
        ("expected_response.json", converted_response),
    ] {
        let path = format!("tests/fixtures/{dir}/{name}");
        if update {
            std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap() + "\n").unwrap();
            continue;
        }
        let golden = load(dir, name);
        assert_eq!(
            mask_volatile(golden),
            mask_volatile(value),
            "{dir}/{name} 与转换输出不一致(有意变更须 UPDATE_FIXTURES=1 重建并记 CHANGELOG)"
        );
    }
}

#[test]
fn openai_to_anthropic_nonstream() {
    check_pair(
        "openai-to-anthropic",
        Protocol::OpenAi,
        Protocol::Anthropic,
        "gpt-4o",
    );
}

#[test]
fn anthropic_to_openai_nonstream() {
    check_pair(
        "anthropic-to-openai",
        Protocol::Anthropic,
        Protocol::OpenAi,
        "claude-3-5-sonnet",
    );
}
