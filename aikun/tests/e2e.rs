mod common;

use std::time::Duration;

use common::{KeyLimits, MockUpstream, ProviderSeed, TestApp, wait_success_log};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(10);

/// OpenAI 渠道种子(经管理面 API;health 起始 unknown,选路可用)。
async fn seed_openai_provider(app: &TestApp, mock: &MockUpstream, name: &str) -> String {
    app.seed_provider(&ProviderSeed {
        name,
        base_url: &mock.base,
        ..ProviderSeed::default()
    })
    .await
}

async fn chat_completions(app: &TestApp, key: &str) -> reqwest::Response {
    let req = app
        .client()
        .post(format!("{}/v1/chat/completions", app.base))
        .json(&json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}]
        }));
    req.bearer_auth(key).send().await.unwrap()
}

/// 某用户日志页里存在 status_code == 给定值的行(failover/失败类断言)。
fn has_log_with_status(page: &Value, status: i64) -> bool {
    page["items"].as_array().is_some_and(|items| {
        items
            .iter()
            .any(|l| l["status_code"].as_i64() == Some(status))
    })
}

/// 渠道 error_rate 已被失败尝试抬升(EMA 记账落库)。
fn error_rate_lifted(p: &Value) -> bool {
    p["error_rate"].as_f64().unwrap_or(0.0) > 0.0
}

/// 自定义 usage 的 OpenAI 补全(仪表盘用例需要受控 token 数)。
fn completion_with_usage(prompt: i64, completion: i64) -> Value {
    json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "OK"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion
        }
    })
}

#[tokio::test]
async fn openai_chat_success_end_to_end() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    seed_openai_provider(&app, &mock, "p1").await;

    let resp = chat_completions(&app, &user.api_key).await;
    assert_eq!(resp.status(), 200);
    // 9.6:所有 /v1 出口带 X-Request-Id(UUID v4 形状;与 span、
    // request_logs.request_id 同值——request_id 落库由 store 单测锁定)
    let request_id = resp
        .headers()
        .get("x-request-id")
        .expect("X-Request-Id header")
        .to_str()
        .unwrap()
        .to_string();
    let parts: Vec<&str> = request_id.split('-').collect();
    assert_eq!(
        parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
        vec![8, 4, 4, 4, 12],
        "UUID v4 shape: {request_id}"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "OK");

    assert!(
        mock.requests()
            .iter()
            .any(|r| r.path == "/v1/chat/completions")
    );

    // 记账断言(管理面日志通道):成功行 total_tokens=8(mock usage 5+3)
    let log = wait_success_log(&app, &user.id, WAIT).await;
    assert_eq!(log["total_tokens"], 8, "{log}");
    assert_eq!(log["request_type"], "openai");
}

#[tokio::test]
async fn anthropic_client_converted_to_openai_upstream() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    seed_openai_provider(&app, &mock, "p1").await;

    let req = app
        .client()
        .post(format!("{}/v1/messages", app.base))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "gpt-4",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }));
    let resp = req.bearer_auth(&user.api_key).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["content"][0]["text"], "OK");

    let reqs = mock.requests();
    let upstream_body = reqs
        .iter()
        .find(|r| r.path == "/v1/chat/completions")
        .expect("request must reach openai upstream");
    let v: Value = serde_json::from_str(&upstream_body.body).unwrap();
    assert_eq!(v["model"], "gpt-4");
    assert!(
        v.get("max_tokens").is_some() || v.get("max_completion_tokens").is_some(),
        "anthropic request must be converted to openai shape, got {v}"
    );
}

/// 故障转移的确定性铺排:坏渠道先建、好渠道建后立即停用旁观;坏渠道
/// 600ms 延迟回 500,请求在途期间激活好渠道 → 重试必达好渠道,且坏
/// 渠道恰好尝试一次。(旧直连夹具靠 error_rate=1.0 把好渠道评分压零;
/// 管理面 API 不暴露 error_rate,用「停用→在途激活」得到同一确定性,
/// 被断言的语义不变:5xx 后不重试原渠道、故障转移到下一渠道。)
async fn park_provider(app: &TestApp, id: &str) {
    let resp = app
        .client()
        .patch(format!("{}/api/providers/{id}", app.base))
        .bearer_auth(app.admin_token().await)
        .json(&json!({"is_active": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
}

async fn activate_provider(app: &TestApp, id: &str) {
    let resp = app
        .client()
        .patch(format!("{}/api/providers/{id}", app.base))
        .bearer_auth(app.admin_token().await)
        .json(&json!({"is_active": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
}

#[tokio::test]
async fn failover_to_second_channel_after_upstream_500() {
    let bad_mock = MockUpstream::start().await;
    let good_mock = MockUpstream::start().await;

    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    let _bad_id = seed_openai_provider(&app, &bad_mock, "p-bad").await;
    let good_id = seed_openai_provider(&app, &good_mock, "p-good").await;
    park_provider(&app, &good_id).await;
    bad_mock.push_delayed_json(
        Duration::from_millis(600),
        500,
        json!({"error": "internal"}),
    );

    let client = app.client();
    let base = app.base.clone();
    let key = user.api_key.clone();
    let resp = tokio::spawn(async move {
        client
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth(key)
            .json(&json!({
                "model": "gpt-4",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap()
    });
    // 请求在途(坏渠道 600ms 延迟窗口内)激活好渠道
    tokio::time::sleep(Duration::from_millis(200)).await;
    activate_provider(&app, &good_id).await;
    let resp = resp.await.unwrap();
    assert_eq!(
        resp.status(),
        200,
        "failover must deliver the good upstream"
    );
    let body: Value = resp.json().await.unwrap();
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

    // 失败尝试落一条 500 日志(旧直连 SELECT 的管理面通道等价)
    app.wait_admin(
        &format!("/api/logs?user_id={}&limit=200", user.id),
        WAIT,
        |page| has_log_with_status(page, 500),
    )
    .await;
}

#[tokio::test]
async fn envelope_200_counts_as_failure_not_success() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    let pid = seed_openai_provider(&app, &mock, "p1").await;
    // 唯一渠道在所有候选被排除后会被重试(max_retries=3),三次尝试都给信封错误。
    for _ in 0..3 {
        mock.push_json(
            200,
            json!({"base_resp": {"status_code": 1002, "status_msg": "rate limited"}}),
        );
    }

    let resp = chat_completions(&app, &user.api_key).await;
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(
        status,
        502,
        "envelope error must not surface as success; body={text}; upstream_requests={:?}",
        mock.requests()
    );

    // 失败落日志(502)且渠道 EMA 记账(error_rate 抬升、健康降级)
    app.wait_admin(
        &format!("/api/logs?user_id={}&limit=200", user.id),
        WAIT,
        |page| has_log_with_status(page, 502),
    )
    .await;
    let p = app
        .wait_admin(&format!("/api/providers/{pid}"), WAIT, error_rate_lifted)
        .await;
    assert_ne!(p["health_status"], "unknown", "健康态应降级:{p}");
}

#[tokio::test]
async fn empty_sse_stream_is_failure() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    let pid = seed_openai_provider(&app, &mock, "p1").await;
    mock.push_sse("");

    let req = app
        .client()
        .post(format!("{}/v1/chat/completions", app.base))
        .json(&json!({
            "model": "gpt-4",
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }));
    let resp = req.bearer_auth(&user.api_key).send().await.unwrap();
    assert_eq!(resp.status(), 200, "stream already committed to 200");
    let text = resp.text().await.unwrap();
    assert!(text.contains("[DONE]"), "stream must terminate: {text}");
    assert!(
        text.contains("error"),
        "stream must report the failure: {text}"
    );

    app.wait_admin(
        &format!("/api/logs?user_id={}&limit=200", user.id),
        WAIT,
        |page| has_log_with_status(page, 502),
    )
    .await;
    app.wait_admin(&format!("/api/providers/{pid}"), WAIT, error_rate_lifted)
        .await;
}

#[tokio::test]
async fn v1_requires_api_key() {
    let app = TestApp::spawn().await;
    let resp = app
        .client()
        .post(format!("{}/v1/chat/completions", app.base))
        .json(&json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await;
    // connect error would be an Err; any HTTP response is fine, must be 401
    assert_eq!(resp.unwrap().status(), 401);
}

/// /v1 生成化(2026-11-18)后的新行为锁定:非法 JSON 经生成校验链产生
/// problem+json,装配层中间件改写为 OpenAI 厂商错误形状(手写时代是
/// axum 默认 text/plain 400,见 CHANGELOG)。
#[tokio::test]
async fn malformed_json_returns_vendor_error_shape() {
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    let resp = app
        .client()
        .post(format!("{}/v1/chat/completions", app.base))
        .header("content-type", "application/json")
        .body("{not valid json")
        .bearer_auth(&user.api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "{body}"
    );
}

/// /v1 生成化(2026-11-18)后的透传回归:混合宽松 schema 的核心字段之外,
/// 顶层扩展字段(OpenAI 兼容生态的典型形态)必须经生成层的
/// #[serde(flatten)] 原样到达上游——漏写 additionalProperties 的层会静默
/// 丢字段,此用例是防回归锚点。
#[tokio::test]
async fn openai_top_level_extension_fields_survive_generated_layer() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    seed_openai_provider(&app, &mock, "p1").await;

    let resp = app
        .client()
        .post(format!("{}/v1/chat/completions", app.base))
        .json(&json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "high",
            "enable_thinking": {"type": "enabled"}
        }))
        .bearer_auth(&user.api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let sent = mock
        .requests()
        .iter()
        .find(|r| r.path == "/v1/chat/completions")
        .map(|r| serde_json::from_str::<Value>(&r.body).unwrap())
        .expect("upstream request recorded");
    assert_eq!(sent["reasoning_effort"], "high", "{sent}");
    assert_eq!(sent["enable_thinking"]["type"], "enabled", "{sent}");
    assert_eq!(sent["model"], "gpt-4", "{sent}");
    assert_eq!(sent["messages"][0]["content"], "hi", "{sent}");
}

#[tokio::test]
async fn upstream_401_auto_disables_channel() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    let pid = seed_openai_provider(&app, &mock, "p1").await;
    // max_retries is 3, all attempts hit the same channel (it's the only one)
    for _ in 0..3 {
        mock.push_json(401, json!({"error": {"message": "bad key"}}));
    }

    let resp = chat_completions(&app, &user.api_key).await;
    assert_eq!(resp.status(), 502);

    // 自动禁用:is_active=false + disabled_reason 落库(管理面回读)
    let p = app
        .wait_admin(&format!("/api/providers/{pid}"), WAIT, |p| {
            p["is_active"] == json!(false)
        })
        .await;
    assert!(
        !p["disabled_reason"].as_str().unwrap_or("").is_empty(),
        "禁用原因应落库:{p}"
    );
}

#[tokio::test]
async fn api_key_rate_limit_returns_429() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_with_limits(1, 0).await; // 1 req/min
    app.adjust_balance(&user.id, 1.0).await;
    seed_openai_provider(&app, &mock, "p1").await;

    let first = chat_completions(&app, &user.api_key).await;
    assert_eq!(first.status(), 200, "first request within the limit");

    let second = chat_completions(&app, &user.api_key).await;
    assert_eq!(second.status(), 429, "second request in the same minute");
    let body: Value = second.json().await.unwrap();
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
    let user = app.seed_user_with_limits(0, 5).await; // 5 tokens/day,mock 每次消耗 8
    app.adjust_balance(&user.id, 1.0).await;
    seed_openai_provider(&app, &mock, "p1").await;

    let first = chat_completions(&app, &user.api_key).await;
    assert_eq!(first.status(), 200, "first request within the quota");

    // 记账是异步的:等 8 个 token 落日志后再发第二个请求
    app.wait_admin(&format!("/api/logs/stats?user_id={}", user.id), WAIT, |s| {
        s["total_tokens"].as_i64() == Some(8)
    })
    .await;

    let second = chat_completions(&app, &user.api_key).await;
    assert_eq!(second.status(), 429, "quota (5) already consumed (8)");
    let body: Value = second.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
}

/// 渠道 key 静态加密落库 + 跨重启可用:管理面创建即加密(enc:v1: 形态
/// 断言在 aikun-store surreal 单测,docs/surreal-migration.md §七第 2 层);
/// e2e 锚定「密文跨重启可解密使用」——重启后代理链路仍把解密后的明文
/// Bearer 送到上游,且 api_key 永不出参。
#[tokio::test]
async fn provider_keys_encrypted_at_rest_and_still_usable() {
    let mock = MockUpstream::start().await;
    let mut app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    let pid = app
        .seed_provider(&ProviderSeed {
            name: "p1",
            base_url: &mock.base,
            api_key: "upstream-key",
            ..ProviderSeed::default()
        })
        .await;
    let created = app.provider(&pid).await;
    assert!(
        created.get("api_key").is_none(),
        "api_key 永不出参:{created}"
    );

    // 重启:密文随数据目录持久化,解密链路(JWT secret 派生密钥)不变
    app.restart().await;

    // 加密落库后代理链路仍正常,上游收到解密后的明文 Bearer
    let resp = chat_completions(&app, &user.api_key).await;
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
    let user = app.seed_user().await;
    seed_openai_provider(&app, &mock, "p1").await;
    // 余额 100 元(管理面调账;库内口径:整数微元,100 元 = 100_000_000);
    // gpt-4 价格:输入 10 元/1M、输出 30 元/1M
    assert_eq!(app.adjust_balance(&user.id, 100.0).await, 100.0);
    app.seed_price("gpt-4", 10.0, 30.0, None).await;

    let resp = chat_completions(&app, &user.api_key).await;
    assert_eq!(resp.status(), 200);

    // mock usage: 5 prompt(含 2 缓存)+ 3 completion;无 cached_price 时缓存
    // 按输入价计 → cost = (3+2)×10/1M + 3×30/1M = 0.00014 元 = 140 微元
    app.wait_admin(&format!("/api/logs/stats?user_id={}", user.id), WAIT, |s| {
        (s["total_cost"].as_f64().unwrap_or(0.0) - 0.00014).abs() < 1e-12
    })
    .await;
    // 扣费与记账同事务:cost 落库后余额必然同步扣减
    assert!((app.user_balance(&user.id).await - 99.99986).abs() < 1e-9);
}

#[tokio::test]
async fn insufficient_balance_rejected_with_402() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user().await; // 新建用户余额 0
    seed_openai_provider(&app, &mock, "p1").await;

    // 余额 0 → 入口预检 402,协议形状错误 insufficient_quota
    let resp = chat_completions(&app, &user.api_key).await;
    assert_eq!(resp.status(), 402);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "insufficient_quota");

    // 充值(1 微元)后恢复 200
    app.adjust_balance(&user.id, 0.000001).await;
    let resp = chat_completions(&app, &user.api_key).await;
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn cached_tokens_billed_at_cached_price() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    seed_openai_provider(&app, &mock, "p1").await;
    // gpt-4 价格:输入 10、输出 30、缓存 1 元/1M
    app.seed_price("gpt-4", 10.0, 30.0, Some(1.0)).await;

    let resp = chat_completions(&app, &user.api_key).await;
    assert_eq!(resp.status(), 200);

    // mock usage: 5 prompt 中 2 个命中缓存 → 未缓存 3、缓存 2、输出 3
    // cost = 3×10/1M + 2×1/1M + 3×30/1M = 0.000122 元 = 122 微元
    let page = app
        .wait_admin(
            &format!("/api/logs?user_id={}&limit=200", user.id),
            WAIT,
            |page| {
                page["items"].as_array().is_some_and(|items| {
                    items.iter().any(|l| {
                        l["success"] == json!(true) && l["cached_tokens"].as_i64() == Some(2)
                    })
                })
            },
        )
        .await;
    let log = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["success"] == json!(true))
        .unwrap();
    assert!(
        (log["cost"].as_f64().unwrap_or(0.0) - 0.000122).abs() < 1e-12,
        "{log}"
    );
}

#[tokio::test]
async fn no_price_match_leaves_balance_unchanged() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user().await;
    seed_openai_provider(&app, &mock, "p1").await;
    app.adjust_balance(&user.id, 100.0).await;

    let resp = chat_completions(&app, &user.api_key).await;
    assert_eq!(resp.status(), 200);

    app.wait_admin(&format!("/api/logs/stats?user_id={}", user.id), WAIT, |s| {
        s["total_requests"].as_i64().unwrap_or(0) >= 1
            && s["total_cost"].as_f64().unwrap_or(-1.0) == 0.0
    })
    .await;
    assert!((app.user_balance(&user.id).await - 100.0).abs() < 1e-9);
}

#[tokio::test]
async fn default_prices_seeded_when_table_empty_on_startup() {
    // spawn 已清空价格表(经管理面 API,见 tests/common);重启等价于
    // 空表启动 → 重新 seed → 经价格列表回读核对。
    let mut app = TestApp::spawn().await;
    app.restart().await;
    let page = app.admin_get("/api/prices?limit=200&offset=0").await;
    let total = page["total"].as_i64().unwrap();
    assert!(total > 100, "expected seeded default prices, got {}", total);
    // gpt-4o 可能在第二页(默认 210 条,页宽 200)
    let mut items = page["items"].as_array().cloned().unwrap_or_default();
    if items.len() < total as usize {
        let rest = app.admin_get("/api/prices?limit=200&offset=200").await;
        items.extend(rest["items"].as_array().cloned().unwrap_or_default());
    }
    let gpt4o = items
        .iter()
        .find(|p| p["model"] == json!("gpt-4o"))
        .expect("gpt-4o 应在默认价格内");
    // gpt-4o 刊例价 $2.5/1M × 汇率 7.2 = 18 元
    assert_eq!(gpt4o["prompt_price"], 18.0, "{gpt4o}");
}

#[tokio::test]
async fn api_key_concurrent_limit_enforced() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app
        .create_user_with_key(
            "e2e",
            "admin",
            KeyLimits {
                max_concurrent: 1,
                ..KeyLimits::default()
            },
        )
        .await;
    // /v1 余额预检要求 balance > 0
    app.adjust_balance(&user.id, 1.0).await;
    seed_openai_provider(&app, &mock, "p1").await;

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
        let key = user.api_key.clone();
        async move {
            client
                .post(format!("{base}/v1/chat/completions"))
                .bearer_auth(key)
                .json(&json!({
                    "model": "gpt-4",
                    "messages": [{"role": "user", "content": "hi"}]
                }))
                .send()
                .await
                .unwrap()
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let second = chat_completions(&app, &user.api_key).await;
    assert_eq!(second.status(), 429);
    let body: Value = second.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert_eq!(first.await.unwrap().status(), 200);

    // 槽位已归还:立即再发一个请求 → 200
    let resp = chat_completions(&app, &user.api_key).await;
    assert_eq!(resp.status(), 200);
}

// ---------------------------------------------------------------------------
// OpenAI Responses API (/v1/responses)
// ---------------------------------------------------------------------------

fn responses_object(text: &str) -> Value {
    json!({
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

/// responses 原生渠道 = openai 类型 + protocols ["responses","openai"]
/// (responses 不能单独成渠道,跨字段约束; negotiation 对 /v1/responses
/// 请求优先原生 responses)。
async fn seed_responses_provider(app: &TestApp, mock: &MockUpstream, name: &str) -> String {
    app.seed_provider(&ProviderSeed {
        name,
        base_url: &mock.base,
        protocol: "responses",
        ..ProviderSeed::default()
    })
    .await
}

async fn post_responses(app: &TestApp, key: &str, body: Value) -> reqwest::Response {
    let req = app
        .client()
        .post(format!("{}/v1/responses", app.base))
        .json(&body);
    req.bearer_auth(key).send().await.unwrap()
}

/// 等待一条 success=1 的 responses 记账并返回 (prompt, completion, total, cached)。
async fn wait_billing(app: &TestApp, user_id: &str) -> (i64, i64, i64, i64) {
    let page = app
        .wait_admin(
            &format!("/api/logs?user_id={user_id}&limit=200"),
            WAIT,
            |page| {
                page["items"].as_array().is_some_and(|items| {
                    items.iter().any(|l| {
                        l["success"] == json!(true)
                            && l["request_type"].as_str() == Some("responses")
                    })
                })
            },
        )
        .await;
    let l = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["success"] == json!(true) && l["request_type"].as_str() == Some("responses"))
        .unwrap();
    (
        l["prompt_tokens"].as_i64().unwrap_or(0),
        l["completion_tokens"].as_i64().unwrap_or(0),
        l["total_tokens"].as_i64().unwrap_or(0),
        l["cached_tokens"].as_i64().unwrap_or(0),
    )
}

#[tokio::test]
async fn responses_passthrough_non_stream() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    seed_responses_provider(&app, &mock, "p1").await;
    mock.push_json(200, responses_object("native ok"));

    let resp = post_responses(
        &app,
        &user.api_key,
        json!({
            "model": "gpt-4",
            "store": false,
            "input": [{"role": "user", "content": "hi"}]
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
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
    let v: Value = serde_json::from_str(&up.body).unwrap();
    assert_eq!(v["store"], false);
    assert!(v.get("stream_options").is_none());
    assert!(
        v.get("messages").is_none(),
        "pass-through must not convert: {v}"
    );

    // input_tokens=10 含 6 缓存:记账拆分为 4 未缓存 + 6 缓存
    assert_eq!(wait_billing(&app, &user.id).await, (4, 4, 14, 6));
}

#[tokio::test]
async fn responses_passthrough_stream() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    seed_responses_provider(&app, &mock, "p1").await;
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
        &user.api_key,
        json!({
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

    assert_eq!(wait_billing(&app, &user.id).await, (4, 4, 14, 6));
}

#[tokio::test]
async fn responses_converted_to_openai_non_stream() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    // 模型映射:上游收到映射名,客户端仍看到请求名
    app.seed_provider(&ProviderSeed {
        name: "p1",
        base_url: &mock.base,
        model_mapping: Some(json!({"gpt-4": "mapped-gpt-4"})),
        ..ProviderSeed::default()
    })
    .await;
    // 脚本耗尽后 mock 默认回 openai_completion("OK")(usage 5/3/8,缓存 2)

    let resp = post_responses(
        &app,
        &user.api_key,
        json!({
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
    let body: Value = resp.json().await.unwrap();
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
    let v: Value = serde_json::from_str(&up.body).unwrap();
    assert_eq!(v["model"], "mapped-gpt-4");
    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(msgs[0], json!({"role": "system", "content": "be terse"}));
    assert_eq!(msgs[1], json!({"role": "user", "content": "hi"}));
    assert_eq!(msgs[2]["tool_calls"][0]["id"], "c1");
    assert_eq!(msgs[3]["role"], "tool");

    assert_eq!(wait_billing(&app, &user.id).await, (3, 3, 8, 2));
}

#[tokio::test]
async fn responses_converted_to_openai_stream() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    seed_openai_provider(&app, &mock, "p1").await;
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
        &user.api_key,
        json!({
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
    let v: Value = serde_json::from_str(&up.body).unwrap();
    assert_eq!(v["stream_options"], json!({"include_usage": true}));

    assert_eq!(wait_billing(&app, &user.id).await, (4, 4, 14, 6));
}

async fn seed_anthropic_provider(app: &TestApp, mock: &MockUpstream, name: &str) -> String {
    app.seed_provider(&ProviderSeed {
        name,
        protocol: "anthropic",
        base_url: &mock.base,
        ..ProviderSeed::default()
    })
    .await
}

#[tokio::test]
async fn responses_converted_to_anthropic_non_stream() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    seed_anthropic_provider(&app, &mock, "p1").await;
    mock.push_json(
        200,
        json!({
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
        &user.api_key,
        json!({
            "model": "gpt-4",
            "instructions": "sys",
            "input": "hi"
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "response");
    assert_eq!(body["output"][0]["content"][0]["text"], "anthropic hi");
    assert_eq!(body["model"], "gpt-4");

    // responses→openai→anthropic 组合转换到达 /v1/messages
    let reqs = mock.requests();
    let up = reqs
        .iter()
        .find(|r| r.path == "/v1/messages")
        .expect("request must be converted to anthropic messages");
    let v: Value = serde_json::from_str(&up.body).unwrap();
    assert_eq!(v["system"], "sys");
    assert_eq!(
        v["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}])
    );

    assert_eq!(wait_billing(&app, &user.id).await, (3, 2, 5, 0));
}

#[tokio::test]
async fn responses_converted_to_anthropic_stream() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    seed_anthropic_provider(&app, &mock, "p1").await;
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
        &user.api_key,
        json!({
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

    assert_eq!(wait_billing(&app, &user.id).await, (9, 3, 12, 0));
}

#[tokio::test]
async fn responses_failover_to_second_channel() {
    let bad_mock = MockUpstream::start().await;
    let good_mock = MockUpstream::start().await;
    good_mock.push_json(200, responses_object("failover ok"));

    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    let _bad_id = seed_responses_provider(&app, &bad_mock, "p-bad").await;
    let good_id = seed_responses_provider(&app, &good_mock, "p-good").await;
    park_provider(&app, &good_id).await;
    bad_mock.push_delayed_json(
        Duration::from_millis(600),
        500,
        json!({"error": {"message": "internal"}}),
    );

    let client = app.client();
    let base = app.base.clone();
    let key = user.api_key.clone();
    let resp = tokio::spawn(async move {
        client
            .post(format!("{base}/v1/responses"))
            .bearer_auth(key)
            .json(&json!({"model": "gpt-4", "input": "hi"}))
            .send()
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    activate_provider(&app, &good_id).await;
    let resp = resp.await.unwrap();
    assert_eq!(
        resp.status(),
        200,
        "failover must deliver the good upstream"
    );
    let body: Value = resp.json().await.unwrap();
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
    let user = app.seed_user_funded().await;
    let pid = seed_openai_provider(&app, &mock, "p1").await;
    mock.push_sse("");

    let resp = post_responses(
        &app,
        &user.api_key,
        json!({"model": "gpt-4", "stream": true, "input": "hi"}),
    )
    .await;
    assert_eq!(resp.status(), 200, "stream already committed to 200");
    let text = resp.text().await.unwrap();
    // responses 协议:error 事件即终止,无 [DONE],也不得产出 response.completed
    assert!(text.contains("event: error"), "{text}");
    assert!(!text.contains("[DONE]"), "{text}");
    assert!(!text.contains("response.completed"), "{text}");

    app.wait_admin(
        &format!("/api/logs?user_id={}&limit=200", user.id),
        WAIT,
        |page| has_log_with_status(page, 502),
    )
    .await;
    app.wait_admin(&format!("/api/providers/{pid}"), WAIT, error_rate_lifted)
        .await;
}

#[tokio::test]
async fn admin_provider_responses_protocol_validation() {
    let app = TestApp::spawn().await;
    let admin = app.admin_token().await;
    let base = json!({
        "name": "p",
        "api_key": "k",
        "models": ["gpt-4"],
        "openai_base_url": "http://127.0.0.1:1"
    });
    // 0.5:渠道管理面已迁 /api/providers(spec-first,problem+json code)
    let create = |body: Value| {
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
    let r = create(json!({"protocols": ["responses"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["code"], "invalid_protocols");

    // responses 不能作为 default_protocol:schema enum 只有 openai/anthropic,
    // 生成层直接拦成 422(0.5 实测行为;spec 的 400 示例对该取值不可达)
    let r = create(json!({
        "protocols": ["openai", "responses"],
        "default_protocol": "responses"
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 422);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["code"], "request_validation_failed");

    // impl 侧联动校验仍可达:default_protocol 合法枚举值但未勾选
    // (anthropic 不在 protocols 里)→ 400 invalid_default_protocol
    let r = create(json!({
        "protocols": ["openai", "responses"],
        "default_protocol": "anthropic"
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["code"], "invalid_default_protocol");

    // 合法组合:省略默认时落到第一个可作默认的协议(openai 而非 responses)
    let r = create(json!({"protocols": ["responses", "openai"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["default_protocol"], "openai");
    assert_eq!(v["protocols"], json!(["responses", "openai"]));

    // 勾选 responses 但缺 openai 地址 → 400(responses 复用 openai 地址)
    let r = create(json!({
        "protocols": ["anthropic", "responses"],
        "anthropic_base_url": "http://127.0.0.1:1",
        "openai_base_url": ""
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 400);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["code"], "invalid_base_url");
}

/// 通用端点透传:embeddings 字节级转发——模型映射生效、凭证改写、
/// usage 计费、request_type 落库;非白名单路径 404。
#[tokio::test]
async fn passthrough_embeddings_end_to_end() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user().await;
    app.adjust_balance(&user.id, 1.0).await;
    app.seed_provider(&ProviderSeed {
        name: "p-emb",
        base_url: &mock.base,
        models: &["emb-test-001"],
        priority: 0,
        weight: 1.0,
        api_key: "upstream-key",
        model_mapping: Some(json!({"emb-test-001": "upstream-emb-001"})),
        ..ProviderSeed::default()
    })
    .await;
    // 输入 10 元/1M(spawn 已清空默认价格表,此为唯一价格行)
    app.seed_price("emb-test-001", 10.0, 0.0, None).await;

    let emb_resp = json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}],
        "model": "upstream-emb-001",
        "usage": {"prompt_tokens": 1000, "total_tokens": 1000}
    });
    mock.push_json(200, emb_resp.clone());

    let resp = app
        .client()
        .post(format!("{}/v1/embeddings", app.base))
        .bearer_auth(&user.api_key)
        .json(&json!({"model": "emb-test-001", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // 响应体原样回传
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, emb_resp);

    // 转发核对:路径、凭证改写、模型映射已应用、其余字段原样
    let reqs = mock.requests();
    let fwd = reqs
        .iter()
        .find(|r| r.path.ends_with("/embeddings"))
        .unwrap();
    assert_eq!(fwd.authorization.as_deref(), Some("Bearer upstream-key"));
    let fwd_body: Value = serde_json::from_str(&fwd.body).unwrap();
    assert_eq!(fwd_body["model"], "upstream-emb-001");
    assert_eq!(fwd_body["input"], "hi");

    // 计费:1000 输入 token @10 元/1M = 0.01 元 = 10000 微元
    // (passthrough 记账 fire-and-forget,轮询等待)
    app.wait_admin(
        &format!("/api/logs?user_id={}&limit=200", user.id),
        WAIT,
        |page| {
            page["items"].as_array().is_some_and(|items| {
                items.iter().any(|l| {
                    l["model"] == json!("emb-test-001")
                        && l["request_type"].as_str() == Some("embeddings")
                        && (l["cost"].as_f64().unwrap_or(0.0) - 0.01).abs() < 1e-12
                })
            })
        },
    )
    .await;
    app.wait_admin(&format!("/api/users/{}", user.id), WAIT, |u| {
        (u["balance"].as_f64().unwrap_or(0.0) - 0.99).abs() < 1e-9
    })
    .await;

    // 非白名单路径 → 404(不转发、不消耗脚本)
    let resp = app
        .client()
        .post(format!("{}/v1/files", app.base))
        .bearer_auth(&user.api_key)
        .json(&json!({"purpose": "fine-tune"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert!(mock.requests().iter().all(|r| !r.path.ends_with("/files")));
}

/// 透传故障转移:第一个渠道 500 后换第二个渠道成功(确定性铺排同
/// failover_to_second_channel:停用→在途激活);上游 401 对客户端掩码为
/// 502(不泄露渠道凭证失败)。
#[tokio::test]
async fn passthrough_failover_and_credential_masking() {
    let mock1 = MockUpstream::start().await;
    let mock2 = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    let _p1 = app
        .seed_provider(&ProviderSeed {
            name: "emb-a",
            base_url: &mock1.base,
            models: &["emb-test-001"],
            priority: 100,
            weight: 1000.0,
            ..ProviderSeed::default()
        })
        .await;
    let p2 = app
        .seed_provider(&ProviderSeed {
            name: "emb-b",
            base_url: &mock2.base,
            models: &["emb-test-001"],
            priority: 10,
            weight: 10.0,
            ..ProviderSeed::default()
        })
        .await;
    park_provider(&app, &p2).await;

    let emb_resp = json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [0.5]}],
        "model": "emb-test-001",
        "usage": {"prompt_tokens": 10, "total_tokens": 10}
    });
    mock1.push_delayed_json(
        Duration::from_millis(600),
        500,
        json!({"error": {"message": "boom"}}),
    );
    mock2.push_json(200, emb_resp.clone());

    let client = app.client();
    let base = app.base.clone();
    let key = user.api_key.clone();
    let resp = tokio::spawn(async move {
        client
            .post(format!("{base}/v1/embeddings"))
            .bearer_auth(key)
            .json(&json!({"model": "emb-test-001", "input": "hi"}))
            .send()
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    activate_provider(&app, &p2).await;
    let resp = resp.await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
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
        mock1.push_json(401, json!({"error": {"message": "invalid key sk-secret"}}));
        mock2.push_json(401, json!({"error": {"message": "invalid key sk-secret"}}));
    }
    let resp = app
        .client()
        .post(format!("{}/v1/embeddings", app.base))
        .bearer_auth(&user.api_key)
        .json(&json!({"model": "emb-test-001", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.unwrap();
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
///
/// 亲和钉住的建立走真实请求(旧直连夹具写 channel_affinity 行的 API
/// 等价):先只建低分渠道并成功一次(必然经它)→ 钉住;再建高权重渠道,
/// 验证钉住压过选路评分。钉住写库是 fire-and-forget 单写,成功请求
/// 返回后留 500ms 收敛窗口(断言可检出的偶发,非静默)。
#[tokio::test]
async fn channel_affinity_sticks_then_repins_after_failure() {
    let low = MockUpstream::start().await;
    let high = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    app.seed_provider(&ProviderSeed {
        name: "p-low",
        base_url: &low.base,
        priority: 1,
        weight: 1.0,
        ..ProviderSeed::default()
    })
    .await;

    // 唯一渠道成功一次 → (api_key, gpt-4) 钉到 p-low
    let resp = chat_completions(&app, &user.api_key).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(chat_count(&low), 1);
    // 钉住写收敛(fire-and-forget 单写)
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 高权重渠道加入后,钉住的低分渠道必须继续承接
    app.seed_provider(&ProviderSeed {
        name: "p-high",
        base_url: &high.base,
        priority: 100,
        weight: 1000.0,
        ..ProviderSeed::default()
    })
    .await;
    let resp = chat_completions(&app, &user.api_key).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(chat_count(&low), 2, "pinned channel must serve the request");
    assert_eq!(
        chat_count(&high),
        0,
        "a healthy higher-weight channel must not steal a live pin"
    );

    low.push_json(500, json!({"error": "down"}));
    let resp = chat_completions(&app, &user.api_key).await;
    assert_eq!(resp.status(), 200, "failure of the pin must fail over");
    assert_eq!(chat_count(&low), 3);
    assert_eq!(chat_count(&high), 1);
    // 重钉写收敛后再验证"不回头"
    tokio::time::sleep(Duration::from_millis(500)).await;

    let resp = chat_completions(&app, &user.api_key).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        chat_count(&low),
        3,
        "the failed channel must stay unused after repin"
    );
    assert_eq!(chat_count(&high), 2);
}

#[tokio::test]
async fn cross_protocol_keeps_reasoning_and_cache_control() {
    let mock = MockUpstream::start().await;
    mock.push_json(
        200,
        json!({
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
    let user = app.seed_user_funded().await;
    seed_openai_provider(&app, &mock, "p1").await;

    let req = app
        .client()
        .post(format!("{}/v1/messages", app.base))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
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
    let resp = req.bearer_auth(&user.api_key).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["type"], "thinking");
    assert_eq!(body["content"][0]["thinking"], "think");
    assert_eq!(body["content"][0]["signature"], "sig");
    assert_eq!(body["content"][1]["text"], "ans");

    let upstream = mock
        .requests()
        .into_iter()
        .find(|r| r.path == "/v1/chat/completions")
        .expect("converted request must reach the openai channel");
    let sent: Value = serde_json::from_str(&upstream.body).unwrap();
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
    let user = app.seed_user_funded().await;
    let jwt = app.admin_token().await;

    // 注册表对管理端可见:generic 排第一,占位适配器带 experimental 标记
    // (0.5:/api/admin/channel-kinds → /api/channel-kinds,{items,total} 信封)
    let kinds: Value = app.admin_get("/api/channel-kinds").await;
    let list = kinds["items"].as_array().unwrap();
    assert_eq!(list[0]["kind"], "generic");
    assert!(
        list.iter()
            .any(|k| k["kind"] == "qoder" && k["experimental"] == true)
    );
    assert!(list.iter().any(|k| k["kind"] == "workbuddy"));

    let payload = |kind: &str| {
        json!({
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
    let body: Value = resp.json().await.unwrap();
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
    let created: Value = resp.json().await.unwrap();
    assert_eq!(created["channel_kind"], "workbuddy");
    let id = created["id"].as_str().unwrap().to_string();

    // 经适配器路由的请求照常成功(health unknown 不排除出选路)
    let resp = chat_completions(&app, &user.api_key).await;
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
        .json(&json!({"channel_kind": "bogus"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = app
        .client()
        .patch(format!("{}/api/providers/{}", app.base, id))
        .bearer_auth(&jwt)
        .json(&json!({"channel_kind": "qoder"}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert_eq!(
        app.provider(&id).await["channel_kind"],
        json!("qoder"),
        "PATCH 后渠道类型应生效"
    );
}

// ---------- 响应缓存 ----------

async fn chat_with(
    app: &TestApp,
    key: &str,
    temperature: f64,
    extra_header: Option<(&str, &str)>,
) -> reqwest::Response {
    let mut req = app
        .client()
        .post(format!("{}/v1/chat/completions", app.base))
        .json(&json!({
            "model": "gpt-4",
            "temperature": temperature,
            "messages": [{"role": "user", "content": "cache me"}]
        }));
    if let Some((k, v)) = extra_header {
        req = req.header(k, v);
    }
    req.bearer_auth(key).send().await.unwrap()
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
    let user = app.seed_user_funded().await;
    seed_openai_provider(&app, &mock, "p1").await;
    for _ in 0..2 {
        let resp = chat_with(&app, &user.api_key, 0.0, None).await;
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().get("x-aikun-cache").is_none());
    }
    assert_eq!(upstream_chat_calls(&mock), 2);
}

#[tokio::test]
async fn deterministic_cache_hit_skips_upstream_and_still_logs() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn_with_env(&[("AIKUN_CACHE_MODE", "deterministic")]).await;
    let user = app.seed_user_funded().await;
    seed_openai_provider(&app, &mock, "p1").await;

    let first = chat_with(&app, &user.api_key, 0.0, None).await;
    assert_eq!(first.status(), 200);
    assert_eq!(first.headers()["x-aikun-cache"], "MISS");
    let first_body: Value = first.json().await.unwrap();

    // 响应返回前已写入缓存:同一请求紧接着必然命中
    let second = chat_with(&app, &user.api_key, 0.0, None).await;
    assert_eq!(second.status(), 200);
    assert_eq!(second.headers()["x-aikun-cache"], "HIT");
    let hit_body: Value = second.json().await.unwrap();
    assert_eq!(hit_body, first_body);
    assert_eq!(
        upstream_chat_calls(&mock),
        1,
        "cache hit must not reach upstream"
    );

    // temperature != 0 不缓存;Cache-Control: no-cache 跳过缓存
    let warm = chat_with(&app, &user.api_key, 0.7, None).await;
    assert!(warm.headers().get("x-aikun-cache").is_none());
    let bypass = chat_with(
        &app,
        &user.api_key,
        0.0,
        Some(("cache-control", "no-cache")),
    )
    .await;
    assert!(bypass.headers().get("x-aikun-cache").is_none());
    assert_eq!(upstream_chat_calls(&mock), 3);

    // 命中也写一条成功日志:provider_name 为空、用量照记
    app.wait_admin(
        &format!("/api/logs?user_id={}&limit=200", user.id),
        WAIT,
        |page| {
            page["items"].as_array().is_some_and(|items| {
                items.iter().any(|l| {
                    l["success"] == json!(true)
                        && l["provider_name"].is_null()
                        && l["total_tokens"] == 8
                })
            })
        },
    )
    .await;
}

// ---------- 仪表盘统计(usage-stats / system-stats;0.5 新路径) ----------

/// 仪表盘聚合:usage-stats 的 today/week/month 计数、30 天补零日序列与
/// top_models;system-stats 的全系统计数;守护同管理面(匿名 401 / 非
/// admin 403)。数据全部来自真实 /v1 请求(受控 usage 的 mock 上游);
/// 历史时间窗(昨日/31 天前)的库内形态检查随检查通道改造移至 store
/// 单测层(docs/surreal-migration.md §七)。
#[tokio::test]
async fn dashboard_stats_aggregations_new_paths() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user().await;
    app.adjust_balance(&user.id, 1.0).await;
    app.seed_provider(&ProviderSeed {
        name: "p1",
        base_url: &mock.base,
        models: &["gpt-4", "gpt-4o"],
        ..ProviderSeed::default()
    })
    .await;

    let chat_model = |model: &str| {
        let client = app.client();
        let base = app.base.clone();
        let key = user.api_key.clone();
        let model = model.to_string();
        async move {
            client
                .post(format!("{base}/v1/chat/completions"))
                .bearer_auth(key)
                .json(&json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "hi"}]
                }))
                .send()
                .await
                .unwrap()
        }
    };

    // 今日:两条成功(不同模型,受控 token)+ 一次失败(3 次尝试全 500)
    mock.push_json(200, completion_with_usage(100, 0));
    assert_eq!(chat_model("gpt-4").await.status(), 200);
    mock.push_json(200, completion_with_usage(50, 0));
    assert_eq!(chat_model("gpt-4o").await.status(), 200);
    for _ in 0..3 {
        mock.push_json(500, json!({"error": "internal"}));
    }
    // 上游 5xx 原状态透传(401/403 才掩码为 502)
    assert_eq!(chat_model("gpt-4").await.status(), 500);

    // 聚合收敛(失败行 fire-and-forget):今日 5 行(2 成 + 3 败)、150 tokens
    let now = chrono::Utc::now();
    app.wait_admin("/api/usage-stats", WAIT, |u| {
        u["today"]["requests"].as_i64() == Some(5)
    })
    .await;

    let client = app.client();

    // 匿名 → 401;非 admin(库内真实 user 行,登录签发)→ 403
    //(auth 中间件用库内角色覆写 token 声明,须用真实 user)
    let anon = client
        .get(format!("{}/api/usage-stats", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
    app.create_user("plain", "plain-password-1", "user").await;
    let plain_token = app.login("plain", "plain-password-1").await;
    let forbidden = client
        .get(format!("{}/api/usage-stats", app.base))
        .bearer_auth(plain_token)
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), 403);

    // usage-stats:today = 5 行 150 tokens(失败行 tokens=0);week/month 同窗;
    // daily 恒 30 天;top_models 首位 gpt-4(1 成 + 3 败 = 4 行)。
    let u = app.admin_get("/api/usage-stats").await;
    assert_eq!(u["today"]["requests"], 5, "{u}");
    assert_eq!(u["today"]["tokens"], 150);
    assert_eq!(u["week"]["requests"], 5);
    assert_eq!(u["month"]["requests"], 5);
    assert_eq!(u["month"]["tokens"], 150);
    assert_eq!(u["daily"].as_array().unwrap().len(), 30, "缺日补零");
    let today_str = now.date_naive().format("%Y-%m-%d").to_string();
    let today_bar = u["daily"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["date"].as_str() == Some(today_str.as_str()))
        .unwrap();
    assert_eq!(today_bar["requests"], 5);
    let gpt4 = u["top_models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"].as_str() == Some("gpt-4"))
        .unwrap();
    assert_eq!(gpt4["requests"], 4);

    // system-stats:api_keys=1、users=3(默认 admin + e2e + plain)、
    // providers=1、requests_today=5
    let s = app.admin_get("/api/system/stats").await;
    assert_eq!(s["api_keys"], 1, "{s}");
    assert_eq!(s["users"], 3);
    assert_eq!(s["providers"], 1);
    assert_eq!(s["requests_today"], 5);
}

/// 渠道熔断(9.5,阶段 3 尾巴):连续计入健康的失败达到阈值后,冷却期内
/// 选路跳过该渠道;冷却到期恢复可选(半开试探)。确定性构造:
/// - 坏渠道 A 权重碾压(weight 1000 vs 1),无熔断时请求必先打 A;
/// - 亲和按 (api_key, model) 钉住——用不同 key / 不同模型打破钉,
///   保证第 2、4 步仍走加权选路;
/// - auto_disable_threshold 默认 5 > 熔断阈值 2,A 只熔断不禁用。
#[tokio::test]
async fn circuit_breaker_skips_channel_within_cooldown_then_recovers() {
    let bad = MockUpstream::start().await;
    let good = MockUpstream::start().await;
    let app = TestApp::spawn_with_env(&[
        ("AIKUN_BREAKER_FAILURE_THRESHOLD", "2"),
        ("AIKUN_BREAKER_COOLDOWN_SECS", "1"),
    ])
    .await;
    // 三个独立用户(各自的 API key;create_user_with_key 用户名可自定义,
    // seed_user_funded 固定用户名会撞唯一约束)。建号后各自充值 1 元。
    let mk = |name: &'static str| async {
        let u = app
            .create_user_with_key(name, "user", KeyLimits::default())
            .await;
        assert_eq!(app.adjust_balance(&u.id, 1.0).await, 1.0);
        u
    };
    let u1 = mk("cb-user-1").await;
    let u2 = mk("cb-user-2").await;
    let u3 = mk("cb-user-3").await;
    app.seed_provider(&ProviderSeed {
        name: "bad",
        base_url: &bad.base,
        models: &["gpt-4", "gpt-4o"],
        weight: 1000.0,
        ..ProviderSeed::default()
    })
    .await;
    app.seed_provider(&ProviderSeed {
        name: "good",
        base_url: &good.base,
        models: &["gpt-4", "gpt-4o"],
        ..ProviderSeed::default()
    })
    .await;
    // 坏渠道脚本:足量 500(实际被消耗 ≤ 3 次)。
    for _ in 0..8 {
        bad.push_json(500, json!({"error": {"message": "boom"}}));
    }

    // r1/r2(u1/u2,gpt-4):加权必先打 A(500)→ failover B(200)。
    // 两次 A 失败 → 熔断 open(cooldown 1s)。
    for u in [&u1, &u2] {
        let resp = app
            .client()
            .post(format!("{}/v1/chat/completions", app.base))
            .json(&json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]}))
            .bearer_auth(&u.api_key)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
    let a_hits = || {
        bad.requests()
            .iter()
            .filter(|r| r.path == "/v1/chat/completions")
            .count()
    };
    assert_eq!(a_hits(), 2, "两次失败已计入熔断");

    // r3(u1,gpt-4o):亲和钉在 (u1key, gpt-4) 不覆盖此模型 → 加权选路;
    // 熔断窗口内 A 被跳过(无熔断时 weight 1000 必打 A)→ 直接 B。
    let resp = app
        .client()
        .post(format!("{}/v1/chat/completions", app.base))
        .json(&json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]}))
        .bearer_auth(&u1.api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(a_hits(), 2, "冷却窗口内坏渠道不再被选中");

    // 冷却到期(半开):A 恢复可选 → 加权再次命中 → 500 → failover B。
    // (单测已锁定窗口语义;此处验证端到端恢复路径。)
    tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
    let resp = app
        .client()
        .post(format!("{}/v1/chat/completions", app.base))
        .json(&json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]}))
        .bearer_auth(&u3.api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(a_hits(), 3, "冷却到期后半开试探一次(失败后再冷却)");
}
