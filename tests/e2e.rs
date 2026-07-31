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

    // mock usage: 5 prompt + 3 completion → cost = 5×10/1M + 3×30/1M = 0.00014
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

    // 请求命中通配价格:cost = 5×1/1M + 3×2/1M = 0.000011
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
