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
