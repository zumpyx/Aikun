//! Store 批 e2e(plan 步骤 8 / roadmap 阶段 3):缓存与 DB 一致的防御
//! 测试 + 日额度计数器口径。
//!
//! * **渠道拓扑快照**(channel/registry):管理端 PATCH 渠道后,下一请求
//!   必须立即看到新配置(roadmap §四.5)——这里用换上游地址验证:改完
//!   地址的下一个 /v1 请求必须打到新 mock。
//! * **价格表缓存**(billing):管理端改价后,下一笔计费立即用新价
//!   (旧价计费 = 缓存失效缺失的典型症状)。
//! * **日用量计数器**(api_key_usage_daily):与记账同事务累加,配额
//!   429 语义与旧 request_logs SUM 口径一致(e2e.rs 的
//!   api_key_daily_quota_returns_429 覆盖主路径,这里覆盖计数器直查口径)。

mod common;

use common::{MockUpstream, TestApp, seed_api_key, seed_provider};
use serde_json::json;

/// 管理端 PATCH 渠道(换上游地址)→ 下一请求必须走新地址。
/// 这是对「缓存与 DB 不一致」的核心防御(roadmap §四.5):若快照未在
/// 写路径后失效,请求会继续打旧 mock,断言失败。
#[tokio::test]
async fn admin_provider_patch_takes_effect_on_next_request() {
    let mock_old = MockUpstream::start().await;
    let mock_new = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_provider(
        &app.db(),
        &common::ProviderSeed {
            id: "p-cache",
            name: "cache",
            protocol: "openai",
            base_url: &mock_old.base,
            models: "[\"gpt-4\"]",
            priority: 0,
            weight: 1.0,
            health_status: "healthy",
            error_rate: 0.0,
        },
    );
    let client = app.client();

    // 先发一串请求让快照落地(命中旧上游)
    for _ in 0..3 {
        let resp = client
            .post(format!("{}/v1/chat/completions", app.base))
            .bearer_auth(common::TEST_KEY)
            .json(&json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap();
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
    let jwt = common::admin_jwt();
    let patched = client
        .patch(format!("{}/api/providers/p-cache", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"openai_base_url": mock_new.base}))
        .send()
        .await
        .unwrap();
    assert_eq!(patched.status(), 200, "{:?}", patched.text().await);

    // 下一请求必须看到新配置:打到新 mock,且不再打旧 mock
    let before_old = mock_old.requests().len();
    let resp = client
        .post(format!("{}/v1/chat/completions", app.base))
        .bearer_auth(common::TEST_KEY)
        .json(&json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
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
    seed_api_key(&app.db());
    seed_provider(
        &app.db(),
        &common::ProviderSeed {
            id: "p-price",
            name: "price",
            protocol: "openai",
            base_url: &mock.base,
            models: "[\"gpt-4\"]",
            priority: 0,
            weight: 1.0,
            health_status: "healthy",
            error_rate: 0.0,
        },
    );
    // 初始价:10 元/1M 输入、30 元/1M 输出(mock usage 5/3,缓存 2)
    app.db()
        .execute(
            "INSERT INTO model_prices (id, model, prompt_price, completion_price)
             VALUES ('mp-1', 'gpt-4', 10.0, 30.0)",
            [],
        )
        .unwrap();
    let client = app.client();

    let chat = || async {
        let resp = client
            .post(format!("{}/v1/chat/completions", app.base))
            .bearer_auth(common::TEST_KEY)
            .json(&json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    };
    let total_cost = || -> i64 {
        app.db()
            .query_row("SELECT COALESCE(SUM(cost), 0) FROM request_logs", [], |r| {
                r.get(0)
            })
            .unwrap()
    };

    // 第一笔按初始价计费:mock usage 5 prompt(含 2 缓存)+ 3 completion,
    // 无 cached_price 时缓存按输入价计 → (3+2)×10 + 3×30 = 140 微元
    chat().await;
    common::wait_until(
        "first billing lands",
        std::time::Duration::from_secs(5),
        || total_cost() > 0,
    )
    .await;
    assert_eq!(total_cost(), 140);

    // 管理端改价(经 /api/prices,写路径成功后失效价格缓存):
    // 输入 20、输出 40 → 下一笔 = (3+2)×20 + 3×40 = 220 微元
    let jwt = common::admin_jwt();
    let prices: serde_json::Value = client
        .get(format!("{}/api/prices", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let price_id = prices["items"][0]["id"].as_str().unwrap().to_string();
    let updated = client
        .patch(format!("{}/api/prices/{price_id}", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"prompt_price": 20.0, "completion_price": 40.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(updated.status(), 200, "{:?}", updated.text().await);

    chat().await;
    common::wait_until(
        "second billing lands",
        std::time::Duration::from_secs(5),
        || total_cost() >= 360,
    )
    .await;
    assert_eq!(
        total_cost(),
        140 + 220,
        "next billing must use the new price"
    );
}

/// 日用量计数器与明细一致:计数器 = 当日 Σ total_tokens(旧 SUM 口径),
/// 由记账事务维护。
#[tokio::test]
async fn daily_usage_counter_matches_request_log_sum() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_provider(
        &app.db(),
        &common::ProviderSeed {
            id: "p-quota",
            name: "quota",
            protocol: "openai",
            base_url: &mock.base,
            models: "[\"gpt-4\"]",
            priority: 0,
            weight: 1.0,
            health_status: "healthy",
            error_rate: 0.0,
        },
    );
    let client = app.client();
    for _ in 0..3 {
        let resp = client
            .post(format!("{}/v1/chat/completions", app.base))
            .bearer_auth(common::TEST_KEY)
            .json(&json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
    // 记账是同步 await(request_log_sync),但指标侧 fire-and-forget 无关;
    // 计数器在记账事务内,响应返回时已落库。
    common::wait_until("counter settles", std::time::Duration::from_secs(5), || {
        app.db()
            .query_row(
                "SELECT COALESCE(SUM(tokens_spent), 0) FROM api_key_usage_daily",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            == 24
    })
    .await;
    let (counter, log_sum): (i64, i64) = app
        .db()
        .query_row(
            "SELECT (SELECT COALESCE(SUM(tokens_spent), 0) FROM api_key_usage_daily),
                    (SELECT COALESCE(SUM(total_tokens), 0) FROM request_logs
                     WHERE api_key_id = 'k-e2e'
                       AND substr(created_at, 1, 10) = date('now'))",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(counter, log_sum);
    assert_eq!(counter, 24, "3 × 8 tokens");
}
