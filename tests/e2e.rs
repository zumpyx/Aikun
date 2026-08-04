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
    // 余额 100 元;gpt-4 价格:输入 10 元/1M、输出 30 元/1M
    app.db()
        .execute("UPDATE users SET balance = 100.0 WHERE id = 'u-e2e'", [])
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
    // 按输入价计 → cost = (3+2)×10/1M + 3×30/1M = 0.00014
    wait_until("cost recorded and balance deducted", WAIT, || {
        let (cost, balance): (f64, f64) = app
            .db()
            .query_row(
                "SELECT (SELECT COALESCE(SUM(cost), 0) FROM request_logs WHERE user_id = 'u-e2e'),
                        (SELECT balance FROM users WHERE id = 'u-e2e')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        (cost - 0.00014).abs() < 1e-9 && (balance - (100.0 - 0.00014)).abs() < 1e-9
    })
    .await;
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
    // cost = 3×10/1M + 2×1/1M + 3×30/1M = 0.000122
    wait_until("cached tokens billed at cached price", WAIT, || {
        let (cost, cached): (f64, i64) = app
            .db()
            .query_row(
                "SELECT COALESCE(SUM(cost), 0), COALESCE(SUM(cached_tokens), 0)
                 FROM request_logs WHERE user_id = 'u-e2e'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        cached == 2 && (cost - 0.000122).abs() < 1e-9
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
        .execute("UPDATE users SET balance = 100.0 WHERE id = 'u-e2e'", [])
        .unwrap();

    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200);

    wait_until("request logged with zero cost", WAIT, || {
        let (n, cost, balance): (i64, f64, f64) = app
            .db()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM request_logs WHERE user_id = 'u-e2e' AND success = 1),
                        (SELECT COALESCE(SUM(cost), 0) FROM request_logs WHERE user_id = 'u-e2e'),
                        (SELECT balance FROM users WHERE id = 'u-e2e')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        n >= 1 && cost == 0.0 && balance == 100.0
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

    // 请求命中通配价格:cost = (3+2)×1/1M + 3×2/1M = 0.000011
    // (mock 5 个输入含 2 缓存,无 cached_price 时缓存按输入价计)
    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200);
    wait_until("wildcard price applied", WAIT, || {
        let cost: f64 = app
            .db()
            .query_row(
                "SELECT COALESCE(SUM(cost), 0) FROM request_logs WHERE user_id = 'u-e2e'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        (cost - 0.000011).abs() < 1e-9
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
        .execute("UPDATE users SET balance = 100.0 WHERE id = 'u-e2e'", [])
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
        let cost: f64 = app
            .db()
            .query_row(
                "SELECT COALESCE(SUM(cost), 0) FROM request_logs WHERE user_id = 'u-e2e'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        (cost - 0.00014).abs() < 1e-9
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
