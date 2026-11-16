mod common;

use std::time::Duration;

use common::{
    MockUpstream, ProviderSeed, TestApp, seed_api_key, seed_api_key_with_limits, seed_provider,
    wait_until,
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

    assert!(
        mock.requests()
            .iter()
            .any(|r| r.path == "/v1/chat/completions")
    );

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
    assert_eq!(
        resp.status(),
        200,
        "failover must deliver the good upstream"
    );
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
    assert_eq!(
        count_chat(&good_mock),
        1,
        "good channel must serve the retry"
    );

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
        status,
        502,
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
    assert!(
        text.contains("error"),
        "stream must report the failure: {text}"
    );

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
        .query_row("SELECT api_key FROM providers WHERE id = 'p1'", [], |r| {
            r.get(0)
        })
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
        .execute(
            "UPDATE users SET balance = 100_000_000 WHERE id = 'u-e2e'",
            [],
        )
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
        .execute(
            "UPDATE users SET balance = 100_000_000 WHERE id = 'u-e2e'",
            [],
        )
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
async fn api_key_concurrent_limit_enforced() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");
    app.db()
        .execute(
            "UPDATE api_keys SET max_concurrent = 1 WHERE id = 'k-e2e'",
            [],
        )
        .unwrap();

    // 慢上游(1.5s):第一个请求在途期间第二个并发请求 → 429 rate_limit_error
    mock.push_delayed_json(
        std::time::Duration::from_millis(1500),
        200,
        common::openai_completion("slow"),
    );
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
    assert!(
        v.get("messages").is_none(),
        "pass-through must not convert: {v}"
    );

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
    assert_eq!(
        msgs[0],
        serde_json::json!({"role": "system", "content": "be terse"})
    );
    assert_eq!(
        msgs[1],
        serde_json::json!({"role": "user", "content": "hi"})
    );
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
    assert_eq!(
        v["stream_options"],
        serde_json::json!({"include_usage": true})
    );

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
    assert_eq!(
        resp.status(),
        200,
        "failover must deliver the good upstream"
    );
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
    // 0.5:渠道管理面已迁 /api/providers(spec-first,problem+json code)
    let create = |body: serde_json::Value| {
        let mut b = base.clone();
        b.as_object_mut()
            .unwrap()
            .extend(body.as_object().unwrap().clone());
        app.client()
            .post(format!("{}/api/providers", app.base))
            .bearer_auth(&admin)
            .json(&b)
    };

    // responses 不能单独成渠道(默认协议/上游地址语义依附 openai|anthropic)
    let r = create(serde_json::json!({"protocols": ["responses"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["code"], "invalid_protocols");

    // responses 不能作为 default_protocol:schema enum 只有 openai/anthropic,
    // 生成层直接拦成 422(0.5 实测行为;spec 的 400 示例对该取值不可达)
    let r = create(serde_json::json!({
        "protocols": ["openai", "responses"],
        "default_protocol": "responses"
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 422);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["code"], "request_validation_failed");

    // impl 侧联动校验仍可达:default_protocol 合法枚举值但未勾选
    // (anthropic 不在 protocols 里)→ 400 invalid_default_protocol
    let r = create(serde_json::json!({
        "protocols": ["openai", "responses"],
        "default_protocol": "anthropic"
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 400);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["code"], "invalid_default_protocol");

    // 合法组合:省略默认时落到第一个可作默认的协议(openai 而非 responses)
    let r = create(serde_json::json!({"protocols": ["responses", "openai"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["default_protocol"], "openai");
    assert_eq!(v["protocols"], serde_json::json!(["responses", "openai"]));

    // 勾选 responses 但缺 openai 地址 → 400(responses 复用 openai 地址)
    let r = create(serde_json::json!({
        "protocols": ["anthropic", "responses"],
        "anthropic_base_url": "http://127.0.0.1:1",
        "openai_base_url": ""
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 400);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["code"], "invalid_base_url");
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
    let fwd = reqs
        .iter()
        .find(|r| r.path.ends_with("/embeddings"))
        .unwrap();
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
        .query_row("SELECT balance FROM users WHERE id = 'u-e2e'", [], |r| {
            r.get(0)
        })
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
    assert_eq!(
        mock1
            .requests()
            .iter()
            .filter(|r| r.path.ends_with("/embeddings"))
            .count(),
        1
    );
    assert_eq!(
        mock2
            .requests()
            .iter()
            .filter(|r| r.path.ends_with("/embeddings"))
            .count(),
        1
    );

    // 全部渠道 401 → 客户端收 502 upstream_error(渠道凭证失败不外泄)。
    // 循环内候选耗尽会重选同一渠道,多推几个 401 防脚本耗尽回默认 200。
    for _ in 0..4 {
        mock1.push_json(
            401,
            serde_json::json!({"error": {"message": "invalid key sk-secret"}}),
        );
        mock2.push_json(
            401,
            serde_json::json!({"error": {"message": "invalid key sk-secret"}}),
        );
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
    assert!(
        !body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("sk-secret")
    );
}

fn chat_count(mock: &MockUpstream) -> usize {
    mock.requests()
        .iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .count()
}

/// 低分渠道被钉住时必须压过高权重渠道;它返回 500 后改钉到接手的渠道,
/// 下一次请求不再回头。
#[tokio::test]
async fn channel_affinity_sticks_then_repins_after_failure() {
    let low = MockUpstream::start().await;
    let high = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_provider(
        &app.db(),
        &ProviderSeed {
            id: "p-low",
            name: "p-low",
            protocol: "openai",
            base_url: &low.base,
            models: "[\"gpt-4\"]",
            priority: 1,
            weight: 1.0,
            health_status: "degraded",
            error_rate: 1.0,
        },
    );
    seed_provider(
        &app.db(),
        &ProviderSeed {
            id: "p-high",
            name: "p-high",
            protocol: "openai",
            base_url: &high.base,
            models: "[\"gpt-4\"]",
            priority: 100,
            weight: 1000.0,
            health_status: "healthy",
            error_rate: 0.0,
        },
    );
    app.db()
        .execute(
            "INSERT INTO channel_affinity (api_key_id, model, provider_id)
             VALUES ('k-e2e', 'gpt-4', 'p-low')",
            [],
        )
        .unwrap();

    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(chat_count(&low), 1, "pinned channel must serve the request");
    assert_eq!(
        chat_count(&high),
        0,
        "a healthy higher-weight channel must not steal a live pin"
    );

    low.push_json(500, serde_json::json!({"error": "down"}));
    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200, "failure of the pin must fail over");
    assert_eq!(chat_count(&low), 2);
    assert_eq!(chat_count(&high), 1);

    wait_until("affinity repinned to the channel that succeeded", WAIT, || {
        app.db()
            .query_row(
                "SELECT provider_id FROM channel_affinity WHERE api_key_id = 'k-e2e' AND model = 'gpt-4'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .as_deref()
            == Some("p-high")
    })
    .await;

    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        chat_count(&low),
        2,
        "the failed channel must stay unused after repin"
    );
    assert_eq!(chat_count(&high), 2);
}

#[tokio::test]
async fn cross_protocol_keeps_reasoning_and_cache_control() {
    let mock = MockUpstream::start().await;
    mock.push_json(
        200,
        serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "model": "gpt-4",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "ans",
                    "reasoning_content": "think",
                    "reasoning_signature": "sig"
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
        }),
    );
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
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "because", "signature": "sig"},
                    {"type": "text", "text": "earlier"}
                ]}
            ]
        }));
    let resp = app.auth(req).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["type"], "thinking");
    assert_eq!(body["content"][0]["thinking"], "think");
    assert_eq!(body["content"][0]["signature"], "sig");
    assert_eq!(body["content"][1]["text"], "ans");

    let upstream = mock
        .requests()
        .into_iter()
        .find(|r| r.path == "/v1/chat/completions")
        .expect("converted request must reach the openai channel");
    let sent: serde_json::Value = serde_json::from_str(&upstream.body).unwrap();
    let msgs = sent["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["content"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(msgs[1]["reasoning_content"], "because");
    assert_eq!(msgs[1]["reasoning_signature"], "sig");
}

// ---------- 渠道适配器(channel_kind) ----------

#[tokio::test]
async fn channel_kinds_listed_validated_and_routed() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    let jwt = common::admin_jwt();

    // 注册表对管理端可见:generic 排第一,占位适配器带 experimental 标记
    // (0.5:/api/admin/channel-kinds → /api/channel-kinds,{items,total} 信封)
    let kinds: serde_json::Value = app
        .client()
        .get(format!("{}/api/channel-kinds", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list = kinds["items"].as_array().unwrap();
    assert_eq!(list[0]["kind"], "generic");
    assert!(
        list.iter()
            .any(|k| k["kind"] == "qoder" && k["experimental"] == true)
    );
    assert!(list.iter().any(|k| k["kind"] == "workbuddy"));

    let payload = |kind: &str| {
        serde_json::json!({
            "name": format!("ck-{}", kind),
            "openai_base_url": mock.base,
            "api_key": format!("sk-{}", kind),
            "models": ["gpt-4"],
            "channel_kind": kind,
        })
    };
    // 未注册的类型 → 400(problem+json code)
    let resp = app
        .client()
        .post(format!("{}/api/providers", app.base))
        .bearer_auth(&jwt)
        .json(&payload("no-such-kind"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "invalid_channel_kind");

    // 合法类型 → 201,回显 channel_kind
    let resp = app
        .client()
        .post(format!("{}/api/providers", app.base))
        .bearer_auth(&jwt)
        .json(&payload("workbuddy"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let created: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(created["channel_kind"], "workbuddy");
    let id = created["id"].as_str().unwrap().to_string();
    app.db()
        .execute(
            "UPDATE providers SET health_status = 'healthy' WHERE id = ?1",
            [&id],
        )
        .unwrap();

    // 经适配器路由的请求照常成功
    let resp = chat_completions(&app).await;
    assert_eq!(resp.status(), 200);
    assert!(
        mock.requests()
            .iter()
            .any(|r| r.path == "/v1/chat/completions")
    );

    // PATCH 改类型同样受注册表校验(0.5 路径;成功返回更新后资源)
    let resp = app
        .client()
        .patch(format!("{}/api/providers/{}", app.base, id))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"channel_kind": "bogus"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = app
        .client()
        .patch(format!("{}/api/providers/{}", app.base, id))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"channel_kind": "qoder"}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let kind: String = app
        .db()
        .query_row(
            "SELECT channel_kind FROM providers WHERE id = ?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kind, "qoder");
}

// ---------- 响应缓存 ----------

async fn chat_with(
    app: &TestApp,
    temperature: f64,
    extra_header: Option<(&str, &str)>,
) -> reqwest::Response {
    let mut req = app
        .client()
        .post(format!("{}/v1/chat/completions", app.base))
        .json(&serde_json::json!({
            "model": "gpt-4",
            "temperature": temperature,
            "messages": [{"role": "user", "content": "cache me"}]
        }));
    if let Some((k, v)) = extra_header {
        req = req.header(k, v);
    }
    app.auth(req).send().await.unwrap()
}

fn upstream_chat_calls(mock: &MockUpstream) -> usize {
    mock.requests()
        .iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .count()
}

#[tokio::test]
async fn cache_off_by_default_every_request_hits_upstream() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");
    for _ in 0..2 {
        let resp = chat_with(&app, 0.0, None).await;
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().get("x-aikun-cache").is_none());
    }
    assert_eq!(upstream_chat_calls(&mock), 2);
}

#[tokio::test]
async fn deterministic_cache_hit_skips_upstream_and_still_logs() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn_with_env(&[("AIKUN_CACHE_MODE", "deterministic")]).await;
    seed_api_key(&app.db());
    seed_openai_provider(&app, &mock, "p1");

    let first = chat_with(&app, 0.0, None).await;
    assert_eq!(first.status(), 200);
    assert_eq!(first.headers()["x-aikun-cache"], "MISS");
    let first_body: serde_json::Value = first.json().await.unwrap();

    // 响应返回前已写入缓存:同一请求紧接着必然命中
    let second = chat_with(&app, 0.0, None).await;
    assert_eq!(second.status(), 200);
    assert_eq!(second.headers()["x-aikun-cache"], "HIT");
    let hit_body: serde_json::Value = second.json().await.unwrap();
    assert_eq!(hit_body, first_body);
    assert_eq!(
        upstream_chat_calls(&mock),
        1,
        "cache hit must not reach upstream"
    );

    // temperature != 0 不缓存;Cache-Control: no-cache 跳过缓存
    let warm = chat_with(&app, 0.7, None).await;
    assert!(warm.headers().get("x-aikun-cache").is_none());
    let bypass = chat_with(&app, 0.0, Some(("cache-control", "no-cache"))).await;
    assert!(bypass.headers().get("x-aikun-cache").is_none());
    assert_eq!(upstream_chat_calls(&mock), 3);

    // 命中也写一条成功日志:provider_id 为空、用量照记
    wait_until("cache hit logged", WAIT, || {
        app.db()
            .query_row(
                "SELECT COUNT(*) FROM request_logs WHERE success = 1 AND provider_id IS NULL AND total_tokens = 8",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            >= 1
    })
    .await;
}

// ---------- 仪表盘统计(usage-stats / system-stats;0.5 新路径) ----------

/// 仪表盘聚合(迁 crates/aikun-admin::admin_stats,集成批实现):
/// usage-stats 的 today/week/month 计数、30 天补零日序列与 top_models;
/// system-stats 的全系统计数;守护同管理面(匿名 401 / 非 admin 403)。
#[tokio::test]
async fn dashboard_stats_aggregations_new_paths() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    seed_api_key(&app.db()); // u-e2e(admin)+ k-e2e
    seed_openai_provider(&app, &mock, "p1");
    let now = chrono::Utc::now();
    // 今日两条成功(不同模型)+ 昨日一条失败;31 天前一条(窗外不计)
    let day = |n: i64| now - chrono::Duration::days(n);
    for (id, model, success, tokens, age) in [
        ("s1", "gpt-4", true, 100i64, 0i64),
        ("s2", "gpt-4o", true, 50, 0),
        ("s3", "gpt-4", false, 10, 1),
        ("s0", "gpt-4", true, 999, 31),
    ] {
        app.db()
            .execute(
                "INSERT INTO request_logs (id, user_id, provider_id, model, request_type,
                        prompt_tokens, completion_tokens, total_tokens, cached_tokens,
                        latency_ms, status_code, success, error_message, cost, created_at)
                 VALUES (?1, 'u-e2e', 'p1', ?2, 'chat', 0, 0, ?3, 0, 42, 200, ?4, NULL, 0, ?5)",
                rusqlite::params![id, model, tokens, i32::from(success), day(age).to_rfc3339()],
            )
            .unwrap();
    }

    let client = app.client();
    let jwt = common::admin_jwt();

    // 匿名 → 401;非 admin(库内真实 user 角色)→ 403
    let anon = client
        .get(format!("{}/api/usage-stats", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
    app.db()
        .execute(
            "INSERT INTO users (id, username, password_hash, role) VALUES ('u-plain', 'plain', 'h', 'user')",
            [],
        )
        .unwrap();
    let forbidden = client
        .get(format!("{}/api/usage-stats", app.base))
        .bearer_auth(common::sign_jwt("u-plain", "plain", "user"))
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), 403);

    // usage-stats:today = 今日 2 条 150 tokens;week/month 含昨日失败共
    // 3 条 160 tokens;31 天前那条在窗外。daily 恒 30 天;top_models 首
    // 位是请求数并列时的 gpt-4(2 次);窗口内全部成功请求的聚合不受
    // 失败行影响口径(与旧实现同 SQL)。
    let r = client
        .get(format!("{}/api/usage-stats", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let u: serde_json::Value = r.json().await.unwrap();
    assert_eq!(u["today"]["requests"], 2, "{u}");
    assert_eq!(u["today"]["tokens"], 150);
    assert_eq!(u["week"]["requests"], 3);
    assert_eq!(u["month"]["requests"], 3);
    assert_eq!(u["month"]["tokens"], 160);
    assert_eq!(u["daily"].as_array().unwrap().len(), 30, "缺日补零");
    let today_str = now.date_naive().format("%Y-%m-%d").to_string();
    let today_bar = u["daily"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["date"].as_str() == Some(today_str.as_str()))
        .unwrap();
    assert_eq!(today_bar["requests"], 2);
    let gpt4 = u["top_models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"].as_str() == Some("gpt-4"))
        .unwrap();
    assert_eq!(gpt4["requests"], 2);

    // system-stats:api_keys=1(k-e2e)、users≥3(默认 admin+u-e2e+u-plain)、
    // providers=1、requests_today=2
    let r = client
        .get(format!("{}/api/system/stats", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let s: serde_json::Value = r.json().await.unwrap();
    assert_eq!(s["api_keys"], 1, "{s}");
    assert!(s["users"].as_i64().unwrap() >= 3);
    assert_eq!(s["providers"], 1);
    assert_eq!(s["requests_today"], 2);
}
