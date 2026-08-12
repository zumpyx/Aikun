mod common;

use std::time::Duration;

use common::{
    seed_api_key, seed_api_key_with_limits, seed_provider, wait_until, MockUpstream, ProviderSeed,
    TestApp,
};

const WAIT: Duration = Duration::from_secs(10);

fn seed_openai_provider(app: &TestApp, mock: &MockUpstream, id: &str) {
    seed_provider(
        &app.db(),
        &ProviderSeed {
            id,
            name: id,
            protocol: "openai",
            base_url: &mock.base,
            models: "[\"gpt-4\"]",
            priority: 10,
            weight: 10.0,
            health_status: "healthy",
            error_rate: 0.0,
        },
    );
}

async fn chat_completions(app: &TestApp) -> reqwest::Response {
    let req = app
        .client()
        .post(format!("{}/v1/chat/completions", app.base))
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}]
        }));
    app.auth(req).send().await.unwrap()
}

#[tokio::test]
async fn openai_chat_success_end_to_end() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");

    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "OK");

    assert!(mock
        .requests()
        .iter()
        .any(|r| r.path == "/v1/chat/completions"));

    wait_until("request log recorded", WAIT, || {
        let (n, tokens): (i64, i64) = app
            .db()
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(total_tokens), 0) FROM request_logs WHERE success = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        n >= 1 && tokens == 8
    })
    .await;
}

#[tokio::test]
async fn anthropic_client_converted_to_openai_upstream() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");

    let req = app
        .client()
        .post(format!("{}/v1/messages", app.base))
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": "gpt-4",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }));
    let resp = app.auth(req).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["content"][0]["text"], "OK");

    let reqs = mock.requests();
    let upstream_body = reqs
        .iter()
        .find(|r| r.path == "/v1/chat/completions")
        .expect("request must reach openai upstream");
    let v: serde_json::Value = serde_json::from_str(&upstream_body.body).unwrap();
    assert_eq!(v["model"], "gpt-4");
    assert!(
        v.get("max_tokens").is_some() || v.get("max_completion_tokens").is_some(),
        "anthropic request must be converted to openai shape, got {v}"
    );
}

#[tokio::test]
async fn failover_to_second_channel_after_upstream_500() {
    let bad_mock = MockUpstream::start().await;
    bad_mock.push_json(500, serde_json::json!({"error": "internal"}));
    let good_mock = MockUpstream::start().await;

    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    // bad channel: always wins the weighted selection on the first attempt
    seed_provider(
        &app.db(),
        &ProviderSeed {
            id: "p-bad",
            name: "p-bad",
            protocol: "openai",
            base_url: &bad_mock.base,
            models: "[\"gpt-4\"]",
            priority: 100,
            weight: 1000.0,
            health_status: "healthy",
            error_rate: 0.0,
        },
    );
    // good channel: error_rate=1.0 ⇒ score 0, only picked after bad is excluded
    seed_provider(
        &app.db(),
        &ProviderSeed {
            id: "p-good",
            name: "p-good",
            protocol: "openai",
            base_url: &good_mock.base,
            models: "[\"gpt-4\"]",
            priority: 10,
            weight: 10.0,
            health_status: "unknown",
            error_rate: 1.0,
        },
    );

    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200, "failover must deliver the good upstream");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "OK");

    let count_chat = |m: &MockUpstream| {
        m.requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count()
    };
    assert_eq!(
        count_chat(&bad_mock),
        1,
        "failing channel must be tried exactly once (no retry on 5xx)"
    );
    assert_eq!(count_chat(&good_mock), 1, "good channel must serve the retry");

    wait_until("failed attempt logged", WAIT, || {
        let n: i64 = app
            .db()
            .query_row(
                "SELECT COUNT(*) FROM request_logs WHERE status_code = 500",
                [],
                |r| r.get(0),
            )
            .unwrap();
        n >= 1
    })
    .await;
}

#[tokio::test]
async fn envelope_200_counts_as_failure_not_success() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");
    // 唯一渠道在所有候选被排除后会被重试(max_retries=3),三次尝试都给信封错误。
    for _ in 0..3 {
        mock.push_json(
            200,
            serde_json::json!({"base_resp": {"status_code": 1002, "status_msg": "rate limited"}}),
        );
    }

    let resp = chat_completions(&app).await;
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(
        status, 502,
        "envelope error must not surface as success; body={text}; upstream_requests={:?}",
        mock.requests()
    );

    wait_until("failure logged and health updated", WAIT, || {
        let (logged, failures): (i64, i64) = app
            .db()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM request_logs WHERE status_code = 502),
                        (SELECT consecutive_failures FROM providers WHERE id = 'p1')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        logged >= 1 && failures >= 1
    })
    .await;
}

#[tokio::test]
async fn empty_sse_stream_is_failure() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");
    mock.push_sse("");

    let req = app
        .client()
        .post(format!("{}/v1/chat/completions", app.base))
        .json(&serde_json::json!({
            "model": "gpt-4",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }));
    let resp = app.auth(req).send().await.unwrap();
    assert_eq!(resp.status(), 200, "stream already committed to 200");
    let text = resp.text().await.unwrap();
    assert!(text.contains("[DONE]"), "stream must terminate: {text}");
    assert!(text.contains("error"), "stream must report the failure: {text}");

    wait_until("stream failure logged", WAIT, || {
        let (logged, failures): (i64, i64) = app
            .db()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM request_logs WHERE status_code = 502),
                        (SELECT consecutive_failures FROM providers WHERE id = 'p1')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        logged >= 1 && failures >= 1
    })
    .await;
}

#[tokio::test]
async fn v1_requires_api_key() {
    let app = TestApp::spawn().await;
    let resp = app
        .client()
        .post(format!("{}/v1/chat/completions", app.base))
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await;
    // connect error would be an Err; any HTTP response is fine, must be 401
    assert_eq!(resp.unwrap().status(), 401);
}

#[tokio::test]
async fn upstream_401_auto_disables_channel() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");
    // max_retries is 3, all attempts hit the same channel (it's the only one)
    for _ in 0..3 {
        mock.push_json(401, serde_json::json!({"error": {"message": "bad key"}}));
    }

    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 502);

    wait_until("channel auto-disabled after 401s", WAIT, || {
        let active: i64 = app
            .db()
            .query_row("SELECT is_active FROM providers WHERE id = 'p1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        active == 0
    })
    .await;
}

#[tokio::test]
async fn api_key_rate_limit_returns_429() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key_with_limits(&app.db(), 1, 0); // 1 req/min
    seed_openai_provider(&app, &mock, "p1");

    let first = chat_completions(&app).await;
    assert_eq!(first.status(), 200, "first request within the limit");

    let second = chat_completions(&app).await;
    assert_eq!(second.status(), 429, "second request in the same minute");
    let body: serde_json::Value = second.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");

    // 被限流的请求不应打到上游
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count(),
        1
    );
}

#[tokio::test]
async fn api_key_daily_quota_returns_429() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key_with_limits(&app.db(), 0, 5); // 5 tokens/day,mock 每次消耗 8
    seed_openai_provider(&app, &mock, "p1");

    let first = chat_completions(&app).await;
    assert_eq!(first.status(), 200, "first request within the quota");

    // 记账是异步的:等 8 个 token 落库后再发第二个请求
    wait_until("first request accounted", WAIT, || {
        let used: i64 = app
            .db()
            .query_row(
                "SELECT COALESCE(SUM(total_tokens), 0) FROM request_logs WHERE api_key_id = 'k-e2e'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        used >= 8
    })
    .await;

    let second = chat_completions(&app).await;
    assert_eq!(second.status(), 429, "quota (5) already consumed (8)");
    let body: serde_json::Value = second.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn provider_keys_encrypted_at_rest_and_still_usable() {
    let mock = MockUpstream::start().await;
    let mut app = TestApp::spawn().await;
    seed_api_key(&app.db());
    // 启动后插入明文 key(seed_provider 写 'upstream-key')
    seed_openai_provider(&app, &mock, "p1");

    // 重启触发启动迁移:存量明文 → enc:v1: 密文
    app.restart().await;
    let stored: String = app
        .db()
        .query_row("SELECT api_key FROM providers WHERE id = 'p1'", [], |r| r.get(0))
        .unwrap();
    assert!(
        stored.starts_with("enc:v1:"),
        "stored key must be encrypted at rest, got {stored}"
    );
    assert!(!stored.contains("upstream-key"));

    // 加密落库后代理链路仍正常,上游收到解密后的明文 Bearer
    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200);
    let reqs = mock.requests();
    let chat = reqs
        .iter()
        .find(|r| r.path == "/v1/chat/completions")
        .expect("request must reach upstream");
    assert_eq!(chat.authorization.as_deref(), Some("Bearer upstream-key"));
}

#[tokio::test]
async fn successful_request_deducts_user_balance() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");
    // 余额 100 元(库内口径:整数微元,100 元 = 100_000_000);
    // gpt-4 价格:输入 10 元/1M、输出 30 元/1M
    app.db()
        .execute("UPDATE users SET balance = 100_000_000 WHERE id = 'u-e2e'", [])
        .unwrap();
    app.db()
        .execute(
            "INSERT INTO model_prices (id, model, prompt_price, completion_price)
             VALUES ('mp1', 'gpt-4', 10.0, 30.0)",
            [],
        )
        .unwrap();

    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200);

    // mock usage: 5 prompt(含 2 缓存)+ 3 completion;无 cached_price 时缓存
    // 按输入价计 → cost = (3+2)×10/1M + 3×30/1M = 0.00014 元 = 140 微元
    wait_until("cost recorded and balance deducted", WAIT, || {
        let (cost, balance): (i64, i64) = app
            .db()
            .query_row(
                "SELECT (SELECT COALESCE(SUM(cost), 0) FROM request_logs WHERE user_id = 'u-e2e'),
                        (SELECT balance FROM users WHERE id = 'u-e2e')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        cost == 140 && balance == 100_000_000 - 140
    })
    .await;
}

#[tokio::test]
async fn insufficient_balance_rejected_with_402() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");

    // 余额 0(种子默认带 1 元,显式清零)→ 入口预检 402,协议形状错误 insufficient_quota
    app.db()
        .execute("UPDATE users SET balance = 0 WHERE id = 'u-e2e'", [])
        .unwrap();
    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 402);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "insufficient_quota");

    // 充值(1 微元)后恢复 200
    app.db()
        .execute("UPDATE users SET balance = 1 WHERE id = 'u-e2e'", [])
        .unwrap();
    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn cached_tokens_billed_at_cached_price() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");
    // gpt-4 价格:输入 10、输出 30、缓存 1 元/1M
    app.db()
        .execute(
            "INSERT INTO model_prices (id, model, prompt_price, completion_price, cached_price)
             VALUES ('mp1', 'gpt-4', 10.0, 30.0, 1.0)",
            [],
        )
        .unwrap();

    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200);

    // mock usage: 5 prompt 中 2 个命中缓存 → 未缓存 3、缓存 2、输出 3
    // cost = 3×10/1M + 2×1/1M + 3×30/1M = 0.000122 元 = 122 微元
    wait_until("cached tokens billed at cached price", WAIT, || {
        let (cost, cached): (i64, i64) = app
            .db()
            .query_row(
                "SELECT COALESCE(SUM(cost), 0), COALESCE(SUM(cached_tokens), 0)
                 FROM request_logs WHERE user_id = 'u-e2e'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        cached == 2 && cost == 122
    })
    .await;
}

#[tokio::test]
async fn no_price_match_leaves_balance_unchanged() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");
    app.db()
        .execute("UPDATE users SET balance = 100_000_000 WHERE id = 'u-e2e'", [])
        .unwrap();

    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200);

    wait_until("request logged with zero cost", WAIT, || {
        let (n, cost, balance): (i64, i64, i64) = app
            .db()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM request_logs WHERE user_id = 'u-e2e' AND success = 1),
                        (SELECT COALESCE(SUM(cost), 0) FROM request_logs WHERE user_id = 'u-e2e'),
                        (SELECT balance FROM users WHERE id = 'u-e2e')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        n >= 1 && cost == 0 && balance == 100_000_000
    })
    .await;
}

#[tokio::test]
async fn admin_prices_crud_and_wildcard_billing() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");
    let jwt = common::admin_jwt();

    // 创建通配价格 gpt-*(1, 2)/1M
    let resp = app
        .client()
        .post(format!("{}/api/admin/prices", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"model": "gpt-*", "prompt_price": 1.0, "completion_price": 2.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let price: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(price["model"], "gpt-*");
    // 未传 cached_price:序列化输出 null(缓存按输入价计)
    assert!(price["cached_price"].is_null());

    // 重复创建同一 model → 409
    let resp = app
        .client()
        .post(format!("{}/api/admin/prices", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"model": "gpt-*"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    // 请求命中通配价格:cost = (3+2)×1/1M + 3×2/1M = 0.000011 元 = 11 微元
    // (mock 5 个输入含 2 缓存,无 cached_price 时缓存按输入价计)
    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200);
    wait_until("wildcard price applied", WAIT, || {
        let cost: i64 = app
            .db()
            .query_row(
                "SELECT COALESCE(SUM(cost), 0) FROM request_logs WHERE user_id = 'u-e2e'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        cost == 11
    })
    .await;

    // 列表 → 更新 → 删除
    let resp = app
        .client()
        .get(format!("{}/api/admin/prices", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let prices: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(prices.as_array().unwrap().len(), 1);

    // in_use=1:只返回渠道模型(gpt-4)会命中的条目;other-* 无渠道模型命中被滤掉
    let resp = app
        .client()
        .post(format!("{}/api/admin/prices", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"model": "other-*", "prompt_price": 1.0, "completion_price": 1.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let other_id = resp.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = app
        .client()
        .get(format!("{}/api/admin/prices?in_use=1", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let in_use: serde_json::Value = resp.json().await.unwrap();
    let in_use = in_use.as_array().unwrap();
    assert_eq!(in_use.len(), 1);
    assert_eq!(in_use[0]["model"], "gpt-*");
    let resp = app
        .client()
        .get(format!("{}/api/admin/prices", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.json::<serde_json::Value>().await.unwrap().as_array().unwrap().len(), 2);
    let resp = app
        .client()
        .delete(format!("{}/api/admin/prices/{}", app.base, other_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let id = price["id"].as_str().unwrap();
    let resp = app
        .client()
        .patch(format!("{}/api/admin/prices/{}", app.base, id))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"prompt_price": 5.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<serde_json::Value>().await.unwrap()["prompt_price"], 5.0);

    // cached_price 双层语义:显式 null 清回 NULL(缓存按输入价),缺省不改动
    let resp = app
        .client()
        .patch(format!("{}/api/admin/prices/{}", app.base, id))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"cached_price": 1.5}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<serde_json::Value>().await.unwrap()["cached_price"], 1.5);
    let resp = app
        .client()
        .patch(format!("{}/api/admin/prices/{}", app.base, id))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"cached_price": null}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.json::<serde_json::Value>().await.unwrap();
    assert!(body["cached_price"].is_null(), "显式 null 应清空缓存价: {}", body);
    // 缺省字段不影响已有值(prompt_price 仍是 5.0)
    assert_eq!(body["prompt_price"], 5.0);

    let resp = app
        .client()
        .delete(format!("{}/api/admin/prices/{}", app.base, id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn default_prices_seeded_when_table_empty_on_startup() {
    // spawn 已清空价格表(见 tests/common);重启等价于空表启动 → 重新 seed
    let mut app = TestApp::spawn().await;
    app.restart().await;
    let (n, gpt4o_prompt): (i64, f64) = app
        .db()
        .query_row(
            "SELECT COUNT(*),
                    (SELECT prompt_price FROM model_prices WHERE model = 'gpt-4o')
             FROM model_prices",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(n > 100, "expected seeded default prices, got {}", n);
    // gpt-4o 刊例价 $2.5/1M × 汇率 7.2 = 18 元
    assert_eq!(gpt4o_prompt, 18.0);
}

#[tokio::test]
async fn wallet_returns_own_balance_and_daily_costs() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");
    app.db()
        .execute("UPDATE users SET balance = 100_000_000 WHERE id = 'u-e2e'", [])
        .unwrap();
    app.db()
        .execute(
            "INSERT INTO model_prices (id, model, prompt_price, completion_price)
             VALUES ('mp1', 'gpt-4', 10.0, 30.0)",
            [],
        )
        .unwrap();

    // 未登录 → 401
    let resp = app
        .client()
        .get(format!("{}/api/wallet", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200);
    wait_until("cost recorded", WAIT, || {
        let cost: i64 = app
            .db()
            .query_row(
                "SELECT COALESCE(SUM(cost), 0) FROM request_logs WHERE user_id = 'u-e2e'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        cost == 140
    })
    .await;

    // 普通用户角色也能看自己的钱包(接口只按 claims.sub 取数)
    let jwt = common::sign_jwt("u-e2e", "e2e", "user");
    let resp = app
        .client()
        .get(format!("{}/api/wallet", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let w: serde_json::Value = resp.json().await.unwrap();
    // 费用:3 未缓存×10 + 2 缓存×10(无缓存价按输入价)+ 3 输出×30 = 0.00014
    assert!((w["balance"].as_f64().unwrap() - (100.0 - 0.00014)).abs() < 1e-6);
    assert!((w["today"]["cost"].as_f64().unwrap() - 0.00014).abs() < 1e-9);
    assert!((w["month"]["cost"].as_f64().unwrap() - 0.00014).abs() < 1e-9);
    assert_eq!(w["daily"].as_array().unwrap().len(), 30);
    let today_cost = w["daily"].as_array().unwrap().last().unwrap()["cost"]
        .as_f64()
        .unwrap();
    assert!((today_cost - 0.00014).abs() < 1e-9);
    let models = w["top_models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["model"], "gpt-4");
}

#[tokio::test]
async fn admin_adjust_balance_records_transaction() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    let jwt = common::admin_jwt();
    // 从零余额起算(种子默认带 1 元)
    app.db()
        .execute("UPDATE users SET balance = 0 WHERE id = 'u-e2e'", [])
        .unwrap();

    // 充值 100
    let resp = app
        .client()
        .post(format!("{}/api/admin/users/u-e2e/balance", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"amount": 100.0, "note": "首充"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["balance"], 100.0);
    assert_eq!(body["kind"], "recharge");

    // 扣减 30 → 余额 70,kind=adjust
    let resp = app
        .client()
        .post(format!("{}/api/admin/users/u-e2e/balance", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"amount": -30.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<serde_json::Value>().await.unwrap()["balance"], 70.0);

    // 流水有两条,最新的在前
    let resp = app
        .client()
        .get(format!("{}/api/admin/billing/transactions", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let txs: serde_json::Value = resp.json().await.unwrap();
    let arr = txs.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["amount"], -30.0);
    assert_eq!(arr[0]["balance_after"], 70.0);
    assert_eq!(arr[0]["kind"], "adjust");
    assert_eq!(arr[1]["amount"], 100.0);
    assert_eq!(arr[1]["kind"], "recharge");
    assert_eq!(arr[1]["note"], "首充");

    // 非法金额 → 400;不存在的用户 → 404
    let resp = app
        .client()
        .post(format!("{}/api/admin/users/u-e2e/balance", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"amount": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = app
        .client()
        .post(format!("{}/api/admin/users/nobody/balance", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"amount": 1.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// 渠道批量添加:3 个 key 建 3 个渠道(自动编号 -01..-03),协议/加密落库;
/// 全部非法时 400。
#[tokio::test]
async fn batch_create_providers() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    let jwt = common::admin_jwt();

    let resp = app
        .client()
        .post(format!("{}/api/admin/providers/batch", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({
            "name": "batch",
            "openai_base_url": "http://127.0.0.1:1",
            "models": ["gpt-4"],
            "protocols": ["openai"],
            "api_keys": ["k1", "k2", "k3"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["created"], 3);
    assert_eq!(body["failed"].as_array().unwrap().len(), 0);
    let providers = body["providers"].as_array().unwrap();
    let names: Vec<&str> = providers.iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["batch-01", "batch-02", "batch-03"]);

    // 落库核对:协议字段与加密 key(enc:v1: 前缀,逐 key 密文不同)
    let rows: Vec<(String, String, String)> = app
        .db()
        .prepare("SELECT name, api_key, default_protocol FROM providers WHERE name LIKE 'batch-%' ORDER BY name")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    assert_eq!(rows.len(), 3);
    let ciphers: std::collections::HashSet<&str> = rows.iter().map(|(_, k, _)| k.as_str()).collect();
    for (_, key, proto) in &rows {
        assert_eq!(proto, "openai");
        assert!(key.starts_with("enc:v1:"), "key must be encrypted at rest");
    }
    assert_eq!(ciphers.len(), 3, "每个 key 应独立加密");

    // 空 keys / 超上限 / 缺地址 → 400
    for body in [
        serde_json::json!({"name": "x", "openai_base_url": "http://a", "models": [], "api_keys": []}),
        serde_json::json!({"name": "x", "openai_base_url": "http://a", "models": [], "api_keys": vec!["k"; 101]}),
        serde_json::json!({"name": "x", "models": [], "api_keys": ["k1"], "protocols": ["openai"]}),
    ] {
        let resp = app
            .client()
            .post(format!("{}/api/admin/providers/batch", app.base))
            .bearer_auth(&jwt)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "body: {}", body);
    }
}

/// 渠道 key 重复检测:库存已有同 key(哈希比对)→ 单建 409;批量中与库存
/// 重复及批内重复的归入失败组,不影响其余 key 创建。
#[tokio::test]
async fn duplicate_provider_key_rejected() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    let jwt = common::admin_jwt();

    let payload = serde_json::json!({
        "name": "dup",
        "openai_base_url": "http://127.0.0.1:1",
        "models": ["gpt-4"],
        "protocols": ["openai"],
        "api_key": "same-key"
    });
    let resp = app
        .client()
        .post(format!("{}/api/admin/providers", app.base))
        .bearer_auth(&jwt)
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    // 同 key 再建(不同名)→ 409 duplicate_key
    let mut p2 = payload.clone();
    p2["name"] = serde_json::json!("dup2");
    let resp = app
        .client()
        .post(format!("{}/api/admin/providers", app.base))
        .bearer_auth(&jwt)
        .json(&p2)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "duplicate_key");
    assert!(body["message"].as_str().unwrap().contains("dup"));

    // 批量:same-key 与库存重复、第二个 new-1 批内重复 → 失败组;new-1 创建成功
    let resp = app
        .client()
        .post(format!("{}/api/admin/providers/batch", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({
            "name": "dedup",
            "openai_base_url": "http://127.0.0.1:1",
            "models": ["gpt-4"],
            "protocols": ["openai"],
            "api_keys": ["same-key", "new-1", "new-1"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["created"], 1);
    let failed = body["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 2);
    assert!(failed.iter().all(|f| f["error"] == "duplicate_key"));

    // 编辑渠道换成已占用的 key → 409
    let dup_id: String = app
        .db()
        .query_row("SELECT id FROM providers WHERE name = 'dup'", [], |r| r.get(0))
        .unwrap();
    let resp = app
        .client()
        .patch(format!("{}/api/admin/providers/{}", app.base, dup_id))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"api_key": "new-1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
}

/// Key 并发限制:max_concurrent=1 时,慢上游请求在途期间第二个请求 429;
/// 在途请求结束后槽位归还,后续请求恢复 200。
#[tokio::test]
async fn api_key_concurrent_limit_enforced() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");
    app.db()
        .execute("UPDATE api_keys SET max_concurrent = 1 WHERE id = 'k-e2e'", [])
        .unwrap();

    // 慢上游(1.5s):第一个请求在途期间第二个并发请求 → 429 rate_limit_error
    mock.push_delayed_json(std::time::Duration::from_millis(1500), 200, common::openai_completion("slow"));
    // 注意:reqwest future 是惰性的,必须 spawn 才会真正发出第一个请求
    let first = tokio::spawn({
        let client = app.client();
        let base = app.base.clone();
        async move {
            client
                .post(format!("{}/v1/chat/completions", base))
                .header("Authorization", format!("Bearer {}", common::TEST_KEY))
                .json(&serde_json::json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "hi"}]
                }))
                .send()
                .await
                .unwrap()
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let second = chat_completions(&app).await;
    assert_eq!(second.status(), 429);
    let body: serde_json::Value = second.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert_eq!(first.await.unwrap().status(), 200);

    // 槽位已归还:立即再发一个请求 → 200
    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200);
}

/// 兑换码全流程:生成 → 兑换(余额+流水) → 重复兑换 400 → 禁用后 400 → 过期 400。
#[tokio::test]
async fn redemption_code_full_flow() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    let jwt = common::admin_jwt();
    app.db()
        .execute("UPDATE users SET balance = 0 WHERE id = 'u-e2e'", [])
        .unwrap();

    // 批量生成 3 张 10 元码 + 1 张已过期码
    let resp = app
        .client()
        .post(format!("{}/api/admin/redemption-codes", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"count": 3, "amount": 10.0, "batch": "b1", "note": "活动"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["amount"], 10.0);
    let codes: Vec<String> = body["codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    assert_eq!(codes.len(), 3);
    assert!(codes.iter().all(|c| {
        c.len() == 22 && c.starts_with("AK-") && c.matches('-').count() == 4
    }));
    let resp = app
        .client()
        .post(format!("{}/api/admin/redemption-codes", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"count": 1, "amount": 5.0, "batch": "b1", "expires_at": "2020-01-01"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let expired_code = resp.json::<serde_json::Value>().await.unwrap()["codes"][0]
        .as_str()
        .unwrap()
        .to_string();

    let user_jwt = common::sign_jwt("u-e2e", "e2e", "user");
    let redeem = |code: &str| {
        let app = &app;
        let user_jwt = &user_jwt;
        let code = code.to_string();
        async move {
            app.client()
                .post(format!("{}/api/wallet/redeem", app.base))
                .bearer_auth(user_jwt)
                .json(&serde_json::json!({"code": code}))
                .send()
                .await
                .unwrap()
        }
    };

    // 兑换第一张:余额 0 → 10,流水记"兑换码充值"
    let resp = redeem(&codes[0]).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["amount"], 10.0);
    assert_eq!(body["balance"], 10.0);

    // 重复兑换同一张 → 400
    let resp = redeem(&codes[0]).await;
    assert_eq!(resp.status(), 400);
    // 不存在的码 → 400;过期码 → 400(口径一致,不细分防枚举)
    let resp = redeem("AK-AAAA-BBBB-CCCC-DDDD").await;
    assert_eq!(resp.status(), 400);
    let resp = redeem(&expired_code).await;
    assert_eq!(resp.status(), 400);

    // 流水有一条 10 元充值
    let resp = app
        .client()
        .get(format!("{}/api/admin/billing/transactions", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    let txs: serde_json::Value = resp.json().await.unwrap();
    let arr = txs.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["amount"], 10.0);
    assert_eq!(arr[0]["kind"], "recharge");
    assert_eq!(arr[0]["note"], "兑换码充值");

    // 管理端列表:4 张码(3 正常 + 1 过期未用),第一张已使用;明文/哈希不出库
    let resp = app
        .client()
        .get(format!("{}/api/admin/redemption-codes?batch=b1", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let list: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(list["total"], 4);
    let arr = list["items"].as_array().unwrap();
    assert_eq!(arr.len(), 4);
    assert!(arr.iter().all(|c| {
        let masked = c["code_masked"].as_str().unwrap();
        masked.len() == 22 && masked.starts_with("AK-****-****-****-")
    }));
    assert!(arr.iter().all(|c| c.get("code_hash").is_none() && c.get("code").is_none()));
    let used = arr.iter().find(|c| c["status"] == "used").unwrap();
    assert_eq!(used["used_by"], "e2e");
    // 过期未用:status 仍为 unused,expired 计算字段为 true
    let expired_unused = arr.iter().find(|c| c["status"] == "unused" && !c["expires_at"].is_null()).unwrap();
    assert_eq!(expired_unused["expired"], true);
    assert!(arr.iter().filter(|c| c["expired"] == true).count() == 1);

    // expired 筛选:只返回过期未用的那 1 张
    let resp = app
        .client()
        .get(format!("{}/api/admin/redemption-codes?batch=b1&status=expired", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let filtered: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(filtered["total"], 1);
    assert_eq!(filtered["items"][0]["expired"], true);

    // 分页:limit=2 应得两页
    let resp = app
        .client()
        .get(format!("{}/api/admin/redemption-codes?batch=b1&limit=2&offset=2", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let page2: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(page2["total"], 4);
    assert_eq!(page2["items"].as_array().unwrap().len(), 2);

    // 禁用一张未过期且未使用的 → 兑换 400;重复禁用 → 409
    let unused_id = arr
        .iter()
        .find(|c| c["status"] == "unused" && c["expires_at"].is_null())
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = app
        .client()
        .post(format!("{}/api/admin/redemption-codes/{}/disable", app.base, unused_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = app
        .client()
        .post(format!("{}/api/admin/redemption-codes/{}/disable", app.base, unused_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    // 被禁用的是 codes[1] 或 codes[2]:找出未用且未被禁用的另一张确认仍可用
    let resp = redeem(&codes[1]).await;
    let disabled_first = resp.status() == 400;
    if !disabled_first {
        assert_eq!(resp.status(), 200);
        let resp = redeem(&codes[2]).await;
        assert_eq!(resp.status(), 400);
    } else {
        let resp = redeem(&codes[2]).await;
        assert_eq!(resp.status(), 200);
    }

    // 非法参数:count 超限 / 负面值 / 空批次 → 400
    for body in [
        serde_json::json!({"count": 501, "amount": 1.0, "batch": "x"}),
        serde_json::json!({"count": 1, "amount": -1.0, "batch": "x"}),
        serde_json::json!({"count": 1, "amount": 1.0, "batch": ""}),
        serde_json::json!({"count": 1, "amount": 1.0, "batch": "x", "expires_at": "2026-01-01T00:00:00Z"}),
    ] {
        let resp = app
            .client()
            .post(format!("{}/api/admin/redemption-codes", app.base))
            .bearer_auth(&jwt)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "body: {}", body);
    }
}

/// 兑换防爆破:同一用户 10 分钟内失败 5 次 → 429。
#[tokio::test]
async fn redemption_bruteforce_rate_limited() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    let user_jwt = common::sign_jwt("u-e2e", "e2e", "user");
    for i in 0..5 {
        let resp = app
            .client()
            .post(format!("{}/api/wallet/redeem", app.base))
            .bearer_auth(&user_jwt)
            .json(&serde_json::json!({"code": format!("AK-XXXX-XXXX-XXXX-{:04}", i)}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }
    let resp = app
        .client()
        .post(format!("{}/api/wallet/redeem", app.base))
        .bearer_auth(&user_jwt)
        .json(&serde_json::json!({"code": "AK-XXXX-XXXX-XXXX-9999"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
}

/// 3.4:admin 跨用户管理 API key(list ?user_id= / PATCH / DELETE),
/// 普通用户操作他人 key 一律 404。
#[tokio::test]
async fn admin_manages_other_users_api_keys() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db()); // u-e2e(admin) + k-e2e
    // 普通用户 bob 及其 key
    app.db()
        .execute(
            "INSERT INTO users (id, username, password_hash, role) VALUES ('u-bob', 'bob', 'h', 'user')",
            [],
        )
        .unwrap();
    app.db()
        .execute(
            "INSERT INTO api_keys (id, user_id, key, key_suffix, name) VALUES ('k-bob', 'u-bob', ?1, 'wxyz', 'bob-key')",
            [common::sha256_hex("sk-bob")],
        )
        .unwrap();

    let admin = common::admin_jwt();
    let bob = common::sign_jwt("u-bob", "bob", "user");

    // admin 带 ?user_id= 列出 bob 的 key;返回只有掩码,无明文。
    let resp = app
        .client()
        .get(format!("{}/api/api-keys?user_id=u-bob", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let keys: serde_json::Value = resp.json().await.unwrap();
    let arr = keys.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "k-bob");
    assert_eq!(arr[0]["key"], "sk-****wxyz");

    // admin 不带 user_id 仍只看自己的 key。
    let resp = app
        .client()
        .get(format!("{}/api/api-keys", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let keys: serde_json::Value = resp.json().await.unwrap();
    let arr = keys.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "k-e2e");

    // admin 禁用 bob 的 key。
    let resp = app
        .client()
        .patch(format!("{}/api/api-keys/k-bob", app.base))
        .bearer_auth(&admin)
        .json(&serde_json::json!({"is_active": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let active: i64 = app
        .db()
        .query_row("SELECT is_active FROM api_keys WHERE id = 'k-bob'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(active, 0);

    // bob 对 admin 的 key 无权操作:404(不暴露存在性)。
    let resp = app
        .client()
        .patch(format!("{}/api/api-keys/k-e2e", app.base))
        .bearer_auth(&bob)
        .json(&serde_json::json!({"is_active": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = app
        .client()
        .delete(format!("{}/api/api-keys/k-e2e", app.base))
        .bearer_auth(&bob)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // admin 删除 bob 的 key。
    let resp = app
        .client()
        .delete(format!("{}/api/api-keys/k-bob", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let n: i64 = app
        .db()
        .query_row("SELECT COUNT(*) FROM api_keys WHERE id = 'k-bob'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 0);
}

// ---------------------------------------------------------------------------
// OpenAI Responses API (/v1/responses)
// ---------------------------------------------------------------------------

fn responses_object(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "resp-mock",
        "object": "response",
        "created_at": 1_700_000_000,
        "status": "completed",
        "model": "real-upstream-model",
        "output": [{
            "id": "msg-mock",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}]
        }],
        "usage": {
            "input_tokens": 10,
            "input_tokens_details": {"cached_tokens": 6},
            "output_tokens": 4,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": 14
        }
    })
}

fn seed_responses_provider(app: &TestApp, mock: &MockUpstream, id: &str) {
    seed_responses_provider_full(app, mock, id, 10, 10.0, "healthy", 0.0);
}

/// provider_type 受 CHECK 约束只能是 openai/anthropic/azure/custom;
/// responses 原生渠道 = openai 类型 + protocols 声明 ["responses"]。
fn seed_responses_provider_full(
    app: &TestApp,
    mock: &MockUpstream,
    id: &str,
    priority: i32,
    weight: f64,
    health_status: &str,
    error_rate: f64,
) {
    seed_provider(
        &app.db(),
        &ProviderSeed {
            id,
            name: id,
            protocol: "openai",
            base_url: &mock.base,
            models: "[\"gpt-4\"]",
            priority,
            weight,
            health_status,
            error_rate,
        },
    );
    app.db()
        .execute(
            "UPDATE providers SET protocols = '[\"responses\"]' WHERE id = ?1",
            rusqlite::params![id],
        )
        .unwrap();
}

async fn post_responses(app: &TestApp, body: serde_json::Value) -> reqwest::Response {
    let req = app
        .client()
        .post(format!("{}/v1/responses", app.base))
        .json(&body);
    app.auth(req).send().await.unwrap()
}

/// 等待一条 success=1 的 responses 记账并返回 (prompt, completion, total, cached)。
async fn wait_billing(app: &TestApp) -> (i64, i64, i64, i64) {
    let mut out = (0, 0, 0, 0);
    wait_until("responses request log recorded", WAIT, || {
        let row: Option<(i64, i64, i64, i64)> = app
            .db()
            .query_row(
                "SELECT prompt_tokens, completion_tokens, total_tokens, cached_tokens
                 FROM request_logs WHERE success = 1 AND request_type = 'responses'
                 ORDER BY created_at DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok();
        if let Some(v) = row {
            out = v;
        }
        row.is_some()
    })
    .await;
    out
}

#[tokio::test]
async fn responses_passthrough_non_stream() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_responses_provider(&app, &mock, "p1");
    mock.push_json(200, responses_object("native ok"));

    let resp = post_responses(
        &app,
        serde_json::json!({
            "model": "gpt-4",
            "store": false,
            "input": [{"role": "user", "content": "hi"}]
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "response");
    assert_eq!(body["output"][0]["content"][0]["text"], "native ok");
    // 模型名统一回写为客户端请求名,不透传上游真实模型名
    assert_eq!(body["model"], "gpt-4");

    // 原生透传:body 原样到达 /v1/responses,不注入 chat 的 stream_options
    let reqs = mock.requests();
    let up = reqs
        .iter()
        .find(|r| r.path == "/v1/responses")
        .expect("request must reach responses upstream");
    let v: serde_json::Value = serde_json::from_str(&up.body).unwrap();
    assert_eq!(v["store"], false);
    assert!(v.get("stream_options").is_none());
    assert!(v.get("messages").is_none(), "pass-through must not convert: {v}");

    // input_tokens=10 含 6 缓存:记账拆分为 4 未缓存 + 6 缓存
    assert_eq!(wait_billing(&app).await, (4, 4, 14, 6));
}

#[tokio::test]
async fn responses_passthrough_stream() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_responses_provider(&app, &mock, "p1");
    mock.push_sse(concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"model\":\"real-upstream-model\",\"status\":\"in_progress\"}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m1\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hello\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"model\":\"real-upstream-model\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":10,\"input_tokens_details\":{\"cached_tokens\":6},\"output_tokens\":4,\"total_tokens\":14}}}\n\n",
    ));

    let resp = post_responses(
        &app,
        serde_json::json!({
            "model": "gpt-4",
            "stream": true,
            "input": "hi"
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    // 事件名保留透传,模型名回写
    assert!(text.contains("event: response.created"), "{text}");
    assert!(text.contains("response.output_text.delta"), "{text}");
    assert!(text.contains("response.completed"), "{text}");
    assert!(text.contains("\"gpt-4\""), "{text}");
    assert!(!text.contains("real-upstream-model"), "{text}");

    assert_eq!(wait_billing(&app).await, (4, 4, 14, 6));
}

#[tokio::test]
async fn responses_converted_to_openai_non_stream() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");
    // 模型映射:上游收到映射名,客户端仍看到请求名
    app.db()
        .execute(
            "UPDATE providers SET model_mapping = '{\"gpt-4\":\"mapped-gpt-4\"}' WHERE id = 'p1'",
            [],
        )
        .unwrap();
    // 脚本耗尽后 mock 默认回 openai_completion("OK")(usage 5/3/8,缓存 2)

    let resp = post_responses(
        &app,
        serde_json::json!({
            "model": "gpt-4",
            "instructions": "be terse",
            "input": [
                {"role": "user", "content": "hi"},
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "done"}
            ]
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "response");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["model"], "gpt-4");
    assert_eq!(body["output"][0]["content"][0]["text"], "OK");
    assert!(body["id"].as_str().unwrap().starts_with("resp_"));

    // 上游收到转换后的 chat/completions 请求
    let reqs = mock.requests();
    let up = reqs
        .iter()
        .find(|r| r.path == "/v1/chat/completions")
        .expect("request must be converted to chat/completions");
    let v: serde_json::Value = serde_json::from_str(&up.body).unwrap();
    assert_eq!(v["model"], "mapped-gpt-4");
    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(msgs[0], serde_json::json!({"role": "system", "content": "be terse"}));
    assert_eq!(msgs[1], serde_json::json!({"role": "user", "content": "hi"}));
    assert_eq!(msgs[2]["tool_calls"][0]["id"], "c1");
    assert_eq!(msgs[3]["role"], "tool");

    assert_eq!(wait_billing(&app).await, (3, 3, 8, 2));
}

#[tokio::test]
async fn responses_converted_to_openai_stream() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");
    mock.push_sse(concat!(
        "data: {\"id\":\"c1\",\"model\":\"mapped\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"model\":\"mapped\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"model\":\"mapped\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"model\":\"mapped\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"id\":\"c1\",\"model\":\"mapped\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":14,\"prompt_tokens_details\":{\"cached_tokens\":6}}}\n\n",
        "data: [DONE]\n\n",
    ));

    let resp = post_responses(
        &app,
        serde_json::json!({
            "model": "gpt-4",
            "stream": true,
            "input": "hi"
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("event: response.created"), "{text}");
    assert!(text.contains("event: response.output_text.delta"), "{text}");
    assert!(text.contains("event: response.completed"), "{text}");
    assert!(text.contains("\"Hello\""), "{text}");
    // Responses 流没有 [DONE] 终止帧
    assert!(!text.contains("[DONE]"), "{text}");

    // 网关强制上游上报 usage(防免费白嫖)
    let reqs = mock.requests();
    let up = reqs
        .iter()
        .find(|r| r.path == "/v1/chat/completions")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&up.body).unwrap();
    assert_eq!(v["stream_options"], serde_json::json!({"include_usage": true}));

    assert_eq!(wait_billing(&app).await, (4, 4, 14, 6));
}

#[tokio::test]
async fn responses_converted_to_anthropic_non_stream() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_provider(
        &app.db(),
        &ProviderSeed {
            id: "p1",
            name: "p1",
            protocol: "anthropic",
            base_url: &mock.base,
            models: "[\"gpt-4\"]",
            priority: 10,
            weight: 10.0,
            health_status: "healthy",
            error_rate: 0.0,
        },
    );
    mock.push_json(
        200,
        serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-x",
            "content": [{"type": "text", "text": "anthropic hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 2}
        }),
    );

    let resp = post_responses(
        &app,
        serde_json::json!({
            "model": "gpt-4",
            "instructions": "sys",
            "input": "hi"
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "response");
    assert_eq!(body["output"][0]["content"][0]["text"], "anthropic hi");
    assert_eq!(body["model"], "gpt-4");

    // responses→openai→anthropic 组合转换到达 /v1/messages
    let reqs = mock.requests();
    let up = reqs
        .iter()
        .find(|r| r.path == "/v1/messages")
        .expect("request must be converted to anthropic messages");
    let v: serde_json::Value = serde_json::from_str(&up.body).unwrap();
    assert_eq!(v["system"], "sys");
    assert_eq!(
        v["messages"],
        serde_json::json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}])
    );

    assert_eq!(wait_billing(&app).await, (3, 2, 5, 0));
}

#[tokio::test]
async fn responses_converted_to_anthropic_stream() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_provider(
        &app.db(),
        &ProviderSeed {
            id: "p1",
            name: "p1",
            protocol: "anthropic",
            base_url: &mock.base,
            models: "[\"gpt-4\"]",
            priority: 10,
            weight: 10.0,
            health_status: "healthy",
            error_rate: 0.0,
        },
    );
    mock.push_sse(concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-x\",\"content\":[],\"usage\":{\"input_tokens\":9,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    ));

    let resp = post_responses(
        &app,
        serde_json::json!({
            "model": "gpt-4",
            "stream": true,
            "input": "hi"
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("event: response.created"), "{text}");
    assert!(text.contains("event: response.output_text.delta"), "{text}");
    assert!(text.contains("event: response.completed"), "{text}");
    assert!(!text.contains("[DONE]"), "{text}");

    assert_eq!(wait_billing(&app).await, (9, 3, 12, 0));
}

#[tokio::test]
async fn responses_failover_to_second_channel() {
    let bad_mock = MockUpstream::start().await;
    bad_mock.push_json(500, serde_json::json!({"error": {"message": "internal"}}));
    let good_mock = MockUpstream::start().await;
    good_mock.push_json(200, responses_object("failover ok"));

    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_responses_provider_full(&app, &bad_mock, "p-bad", 100, 1000.0, "healthy", 0.0);
    seed_responses_provider_full(&app, &good_mock, "p-good", 10, 10.0, "unknown", 1.0);

    let resp = post_responses(&app, serde_json::json!({"model": "gpt-4", "input": "hi"})).await;
    assert_eq!(resp.status(), 200, "failover must deliver the good upstream");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["output"][0]["content"][0]["text"], "failover ok");

    let count = |m: &MockUpstream| {
        m.requests()
            .iter()
            .filter(|r| r.path == "/v1/responses")
            .count()
    };
    assert_eq!(count(&bad_mock), 1);
    assert_eq!(count(&good_mock), 1);
}

#[tokio::test]
async fn responses_empty_sse_stream_is_failure() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");
    mock.push_sse("");

    let resp = post_responses(
        &app,
        serde_json::json!({"model": "gpt-4", "stream": true, "input": "hi"}),
    )
    .await;
    assert_eq!(resp.status(), 200, "stream already committed to 200");
    let text = resp.text().await.unwrap();
    // responses 协议:error 事件即终止,无 [DONE],也不得产出 response.completed
    assert!(text.contains("event: error"), "{text}");
    assert!(!text.contains("[DONE]"), "{text}");
    assert!(!text.contains("response.completed"), "{text}");

    wait_until("responses stream failure logged", WAIT, || {
        let (logged, failures): (i64, i64) = app
            .db()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM request_logs WHERE status_code = 502),
                        (SELECT consecutive_failures FROM providers WHERE id = 'p1')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        logged >= 1 && failures >= 1
    })
    .await;
}

#[tokio::test]
async fn admin_provider_responses_protocol_validation() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    let admin = common::admin_jwt();
    let base = serde_json::json!({
        "name": "p",
        "api_key": "k",
        "models": ["gpt-4"],
        "openai_base_url": "http://127.0.0.1:1"
    });
    let create = |body: serde_json::Value| {
        let mut b = base.clone();
        b.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
        app.client()
            .post(format!("{}/api/admin/providers", app.base))
            .bearer_auth(&admin)
            .json(&b)
    };

    // responses 不能单独成渠道(默认协议/上游地址语义依附 openai|anthropic)
    let r = create(serde_json::json!({"protocols": ["responses"]}))
        .send().await.unwrap();
    assert_eq!(r.status(), 400);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["error"], "invalid_protocols");

    // responses 不能作为 default_protocol
    let r = create(serde_json::json!({
        "protocols": ["openai", "responses"],
        "default_protocol": "responses"
    }))
    .send().await.unwrap();
    assert_eq!(r.status(), 400);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["error"], "invalid_default_protocol");

    // 合法组合:省略默认时落到第一个可作默认的协议(openai 而非 responses)
    let r = create(serde_json::json!({"protocols": ["responses", "openai"]}))
        .send().await.unwrap();
    assert_eq!(r.status(), 201);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["default_protocol"], "openai");
    assert_eq!(
        v["protocols"],
        serde_json::json!(["responses", "openai"])
    );

    // 勾选 responses 但缺 openai 地址 → 400(responses 复用 openai 地址)
    let r = create(serde_json::json!({
        "protocols": ["anthropic", "responses"],
        "anthropic_base_url": "http://127.0.0.1:1",
        "openai_base_url": ""
    }))
    .send().await.unwrap();
    assert_eq!(r.status(), 400);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["error"], "invalid_base_url");
}

/// 通用端点透传:embeddings 字节级转发——模型映射生效、凭证改写、
/// usage 计费、request_type 落库;非白名单路径 404。
#[tokio::test]
async fn passthrough_embeddings_end_to_end() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    common::seed_provider(
        &app.db(),
        &common::ProviderSeed {
            id: "p-emb",
            name: "emb",
            protocol: "openai",
            base_url: &mock.base,
            models: "[\"emb-test-001\"]",
            priority: 0,
            weight: 1.0,
            health_status: "healthy",
            error_rate: 0.0,
        },
    );
    app.db()
        .execute(
            "UPDATE providers SET model_mapping = '{\"emb-test-001\":\"upstream-emb-001\"}' WHERE id = 'p-emb'",
            [],
        )
        .unwrap();
    // 输入 10 元/1M(spawn 已清空默认价格表,此为唯一价格行)
    app.db()
        .execute(
            "INSERT INTO model_prices (id, model, prompt_price, completion_price) VALUES ('mp-emb', 'emb-test-001', 10.0, 0.0)",
            [],
        )
        .unwrap();

    let emb_resp = serde_json::json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}],
        "model": "upstream-emb-001",
        "usage": {"prompt_tokens": 1000, "total_tokens": 1000}
    });
    mock.push_json(200, emb_resp.clone());

    let resp = app
        .auth(app.client().post(format!("{}/v1/embeddings", app.base)))
        .json(&serde_json::json!({"model": "emb-test-001", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // 响应体原样回传
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body, emb_resp);

    // 转发核对:路径、凭证改写、模型映射已应用、其余字段原样
    let reqs = mock.requests();
    let fwd = reqs.iter().find(|r| r.path.ends_with("/embeddings")).unwrap();
    assert_eq!(fwd.authorization.as_deref(), Some("Bearer upstream-key"));
    let fwd_body: serde_json::Value = serde_json::from_str(&fwd.body).unwrap();
    assert_eq!(fwd_body["model"], "upstream-emb-001");
    assert_eq!(fwd_body["input"], "hi");

    // 计费:1000 输入 token @10 元/1M = 0.01 元 = 10000 微元
    let (req_type, cost): (String, i64) = app
        .db()
        .query_row(
            "SELECT request_type, cost FROM request_logs WHERE model = 'emb-test-001'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(req_type, "embeddings");
    assert_eq!(cost, 10_000);
    let balance: i64 = app
        .db()
        .query_row("SELECT balance FROM users WHERE id = 'u-e2e'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(balance, 1_000_000 - 10_000);

    // 非白名单路径 → 404(不转发、不消耗脚本)
    let resp = app
        .auth(app.client().post(format!("{}/v1/files", app.base)))
        .json(&serde_json::json!({"purpose": "fine-tune"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert!(mock.requests().iter().all(|r| !r.path.ends_with("/files")));
}

/// 透传故障转移:第一个渠道 500 后换第二个渠道成功;上游 401 对
/// 客户端掩码为 502(不泄露渠道凭证失败)。
#[tokio::test]
async fn passthrough_failover_and_credential_masking() {
    let mock1 = MockUpstream::start().await;
    let mock2 = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    // p1 高优先级高权重:首轮加权选路必然先选它(同既有 failover 用例口径)
    common::seed_provider(
        &app.db(),
        &common::ProviderSeed {
            id: "p1",
            name: "emb-a",
            protocol: "openai",
            base_url: &mock1.base,
            models: "[\"emb-test-001\"]",
            priority: 100,
            weight: 1000.0,
            health_status: "healthy",
            error_rate: 0.0,
        },
    );
    // p2 error_rate=1.0 ⇒ 评分 0,只在 p1 被排除后才被选中
    common::seed_provider(
        &app.db(),
        &common::ProviderSeed {
            id: "p2",
            name: "emb-b",
            protocol: "openai",
            base_url: &mock2.base,
            models: "[\"emb-test-001\"]",
            priority: 10,
            weight: 10.0,
            health_status: "unknown",
            error_rate: 1.0,
        },
    );
    let emb_resp = serde_json::json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [0.5]}],
        "model": "emb-test-001",
        "usage": {"prompt_tokens": 10, "total_tokens": 10}
    });
    mock1.push_json(500, serde_json::json!({"error": {"message": "boom"}}));
    mock2.push_json(200, emb_resp.clone());

    let resp = app
        .auth(app.client().post(format!("{}/v1/embeddings", app.base)))
        .json(&serde_json::json!({"model": "emb-test-001", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body, emb_resp);
    // 两个渠道各被打一次(500 后故障转移)
    assert_eq!(mock1.requests().iter().filter(|r| r.path.ends_with("/embeddings")).count(), 1);
    assert_eq!(mock2.requests().iter().filter(|r| r.path.ends_with("/embeddings")).count(), 1);

    // 全部渠道 401 → 客户端收 502 upstream_error(渠道凭证失败不外泄)。
    // 循环内候选耗尽会重选同一渠道,多推几个 401 防脚本耗尽回默认 200。
    for _ in 0..4 {
        mock1.push_json(401, serde_json::json!({"error": {"message": "invalid key sk-secret"}}));
        mock2.push_json(401, serde_json::json!({"error": {"message": "invalid key sk-secret"}}));
    }
    let resp = app
        .auth(app.client().post(format!("{}/v1/embeddings", app.base)))
        .json(&serde_json::json!({"model": "emb-test-001", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "upstream_error");
    assert!(!body["error"]["message"].as_str().unwrap().contains("sk-secret"));
}
