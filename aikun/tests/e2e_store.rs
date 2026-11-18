//! Store 批 e2e(plan 步骤 8 / roadmap 阶段 3):缓存与 DB 一致的防御
//! 测试 + 日额度计数器口径。
//!
//! * **渠道拓扑快照**(channel/registry):管理端 PATCH 渠道后,下一请求
//!   必须立即看到新配置(roadmap §四.5)——这里用换上游地址验证:改完
//!   地址的下一个 /v1 请求必须打到新 mock。
//! * **价格表缓存**(billing):管理端改价后,下一笔计费立即用新价
//!   (旧价计费 = 缓存失效缺失的典型症状)。
//! * **日用量计数器**(api_key_usage_daily):配额 429 由计数器判定
//!   (与记账同事务累加);e2e 以「quota=24 时 3×8 token 放行、第 4 次
//!   429」+「日志 total_tokens=24」锚定计数器口径与明细一致(计数器
//!   的库内直查已随检查通道改造由 store 单测承担,
//!   docs/surreal-migration.md §七)。

mod common;

use std::time::Duration;

use common::{KeyLimits, MockUpstream, ProviderSeed, TestApp};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(5);

async fn chat(app: &TestApp, key: &str) -> reqwest::Response {
    app.client()
        .post(format!("{}/v1/chat/completions", app.base))
        .bearer_auth(key)
        .json(&json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
}

/// 管理端 PATCH 渠道(换上游地址)→ 下一请求必须走新地址。
/// 这是对「缓存与 DB 不一致」的核心防御(roadmap §四.5):若快照未在
/// 写路径后失效,请求会继续打旧 mock,断言失败。
#[tokio::test]
async fn admin_provider_patch_takes_effect_on_next_request() {
    let mock_old = MockUpstream::start().await;
    let mock_new = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    let p = app
        .seed_provider(&ProviderSeed {
            name: "cache",
            base_url: &mock_old.base,
            ..ProviderSeed::default()
        })
        .await;
    let client = app.client();

    // 先发一串请求让快照落地(命中旧上游)
    for _ in 0..3 {
        let resp = chat(&app, &user.api_key).await;
        assert_eq!(resp.status(), 200);
    }
    assert!(
        mock_old
            .requests()
            .iter()
            .any(|r| r.path == "/v1/chat/completions"),
        "baseline requests must hit the old upstream"
    );
    assert!(mock_new.requests().is_empty());

    // 管理端 PATCH:换上游地址(经 /api/providers,写路径成功后失效快照)
    let jwt = app.admin_token().await;
    let patched = client
        .patch(format!("{}/api/providers/{p}", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"openai_base_url": mock_new.base}))
        .send()
        .await
        .unwrap();
    assert_eq!(patched.status(), 200, "{:?}", patched.text().await);

    // 下一请求必须看到新配置:打到新 mock,且不再打旧 mock
    let before_old = mock_old.requests().len();
    let resp = chat(&app, &user.api_key).await;
    assert_eq!(resp.status(), 200);
    assert!(
        mock_new
            .requests()
            .iter()
            .any(|r| r.path == "/v1/chat/completions"),
        "next request after admin PATCH must hit the new upstream (stale snapshot?)"
    );
    assert_eq!(
        mock_old.requests().len(),
        before_old,
        "old upstream must not receive traffic after the PATCH"
    );
}

/// 管理端改价 → 下一笔计费立即用新价(价格表缓存失效)。
#[tokio::test]
async fn admin_price_update_takes_effect_on_next_billing() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    app.seed_provider(&ProviderSeed {
        name: "price",
        base_url: &mock.base,
        ..ProviderSeed::default()
    })
    .await;
    // 初始价:10 元/1M 输入、30 元/1M 输出(mock usage 5/3,缓存 2)
    let price_id = app.seed_price("gpt-4", 10.0, 30.0, None).await;
    let client = app.client();
    let jwt = app.admin_token().await;
    let stats_path = format!("/api/logs/stats?user_id={}", user.id);

    // 第一笔按初始价计费:mock usage 5 prompt(含 2 缓存)+ 3 completion,
    // 无 cached_price 时缓存按输入价计 → (3+2)×10 + 3×30 = 140 微元
    chat(&app, &user.api_key).await;
    app.wait_admin(&stats_path, WAIT, |s| {
        (s["total_cost"].as_f64().unwrap_or(0.0) - 0.00014).abs() < 1e-12
    })
    .await;

    // 管理端改价(经 /api/prices,写路径成功后失效价格缓存):
    // 输入 20、输出 40 → 下一笔 = (3+2)×20 + 3×40 = 220 微元
    let updated = client
        .patch(format!("{}/api/prices/{price_id}", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"prompt_price": 20.0, "completion_price": 40.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(updated.status(), 200, "{:?}", updated.text().await);

    chat(&app, &user.api_key).await;
    let s = app
        .wait_admin(&stats_path, WAIT, |s| {
            (s["total_cost"].as_f64().unwrap_or(0.0) - 0.00036).abs() < 1e-12
        })
        .await;
    assert!(
        (s["total_cost"].as_f64().unwrap() - (0.00014 + 0.00022)).abs() < 1e-12,
        "next billing must use the new price"
    );
}

/// 日用量计数器口径:quota=24 tokens 时,3×8 token 请求全部放行、
/// 第 4 次被 429(配额判定读计数器);日志明细 total_tokens=24 与之
/// 一致——计数器与明细同事务累加的行为锚定。
#[tokio::test]
async fn daily_usage_counter_blocks_at_quota() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app
        .create_user_with_key(
            "e2e",
            "admin",
            KeyLimits {
                quota_daily_tokens: 24,
                ..KeyLimits::default()
            },
        )
        .await;
    app.seed_provider(&ProviderSeed {
        name: "quota",
        base_url: &mock.base,
        ..ProviderSeed::default()
    })
    .await;
    // /v1 余额预检要求 balance > 0
    app.adjust_balance(&user.id, 1.0).await;

    for _ in 0..3 {
        let resp = chat(&app, &user.api_key).await;
        assert_eq!(resp.status(), 200);
    }
    // 记账落库后计数器应为 24(3 × 8 tokens)
    app.wait_admin(&format!("/api/logs/stats?user_id={}", user.id), WAIT, |s| {
        s["total_tokens"].as_i64() == Some(24)
    })
    .await;
    // 第 4 次:24/24 已耗尽 → 429 rate_limit_error(计数器口径)
    let resp = chat(&app, &user.api_key).await;
    assert_eq!(resp.status(), 429, "quota (24) already consumed");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    // 上游只收到 3 次真实请求
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        3
    );
}
