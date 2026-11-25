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
    assert_eq!(r.status(), 201);

    // responses 可单独成渠道，也可作默认协议
    let r = create(json!({
        "protocols": ["responses"],
        "default_protocol": "responses"
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 201);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["default_protocol"], "responses");

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
    // 1s 冷却在 CI 负载下可能还没到期，多等一会儿再断言。
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
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

/// ZCode 渠道适配器(GLM Coding Plan):协议链路端到端——上游收到
/// Anthropic 形态请求打到 ultra 路径,JWT 同时出现在
/// x-api-key 与 Authorization(Bearer),伪装头(anthropic-beta /
/// X-ZCode-Agent 等)到达上游;OpenAI 客户端请求经协议转换正常返回;
/// 计费行落库(闲时免费口径由 channels crate 单测锁定,时间不可控)。
#[tokio::test]
async fn zcode_channel_anthropic_protocol_end_to_end() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    app.seed_provider(&ProviderSeed {
        name: "zcode",
        protocol: "anthropic", // 渠道只声明 anthropic 协议
        base_url: &mock.base,
        models: &["GLM-5.3-Flash"],
        api_key: "fake-zcode-jwt-token",
        ..ProviderSeed::default()
    })
    .await;

    // 渠道类型设为 zcode(管理面创建走 generic 默认,PATCH 改 kind)
    let provs = app
        .client()
        .get(format!("{}/api/providers", app.base))
        .bearer_auth(app.admin_token().await)
        .send()
        .await
        .unwrap();
    let provs: serde_json::Value = provs.json().await.unwrap();
    let pid = provs["items"][0]["id"].as_str().unwrap().to_string();
    let r = app
        .client()
        .patch(format!("{}/api/providers/{pid}", app.base))
        .bearer_auth(app.admin_token().await)
        .json(&serde_json::json!({"channel_kind": "zcode"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "PATCH channel_kind=zcode");

    // mock 上游：闲时窗口内会先打 off-peak（推 404 让其回退），非闲时直接打 ultra。
    // 无论哪种情况，最终都要有一条 200 的 Anthropic 响应。
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if aikun_channels::adapters::zcode::is_off_peak(now) {
        mock.push_json(404, json!({"code": 404, "msg": "no mock route"}));
    }
    mock.push_json(
        200,
        json!({
            "id": "msg_mock", "type": "message", "role": "assistant",
            "model": "GLM-5.3-Flash",
            "content": [{"type": "text", "text": "OK"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 3}
        }),
    );

    // OpenAI 客户端 → zcode(anthropic 上游):跨协议转换
    let resp = app
        .client()
        .post(format!("{}/v1/chat/completions", app.base))
        .json(&json!({
            "model": "GLM-5.3-Flash",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .bearer_auth(&user.api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "OK");

    // 上游收到的请求:端点路径 + JWT 双头 + 伪装头
    // 种子无国内标记 → 国际路径 ultra-zai
    let sent = mock
        .requests()
        .iter()
        .find(|r| r.path.contains("/api/v1/ultra-zai/anthropic/v1/messages"))
        .expect("ultra-zai 端点被命中")
        .clone();
    let h = &sent.headers;
    let get = |k: &str| -> String {
        h.iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert_eq!(get("x-api-key"), "fake-zcode-jwt-token", "JWT 进 x-api-key");
    assert_eq!(
        get("authorization"),
        "Bearer fake-zcode-jwt-token",
        "JWT 同时进 Bearer"
    );
    assert_eq!(get("anthropic-version"), "2023-06-01");
    assert!(
        get("anthropic-beta").contains("mid-conversation-system"),
        "伪装 beta 头到达上游: {}",
        get("anthropic-beta")
    );
    assert_eq!(get("x-zcode-agent"), "glm");
    assert!(
        get("user-agent").starts_with("ZCode/"),
        "UA = {}",
        get("user-agent")
    );
    let sent_body: Value = serde_json::from_str(&sent.body).unwrap();
    assert_eq!(sent_body["model"], "GLM-5.3-Flash");

    // 计费行落库(tokens 照记;cost 取决于执行时刻是否闲时窗口,不断言)
    let log = wait_success_log(&app, &user.id, WAIT).await;
    assert_eq!(log["total_tokens"], 8, "{log}");
    assert_eq!(log["model"], "GLM-5.3-Flash");
}

/// 渠道拆分(2026-11-18):category 过滤——generic 渠道只在 standard,
/// zcode 渠道只在 proxy;不带参数返回全部。
#[tokio::test]
async fn providers_category_filter() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let admin = app.admin_token().await;
    seed_openai_provider(&app, &mock, "std-one").await;
    app.seed_provider(&ProviderSeed {
        name: "proxy-one",
        protocol: "anthropic",
        base_url: &mock.base,
        models: &["GLM-5.3"],
        ..ProviderSeed::default()
    })
    .await;

    let all: Vec<String> = app
        .client()
        .get(format!("{}/api/providers?limit=200", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|p| p["name"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    // PATCH 第二个渠道的 channel_kind=zcode(种子默认 generic)
    let proxy_id: String = {
        let items = app
            .client()
            .get(format!("{}/api/providers?limit=200", app.base))
            .bearer_auth(&admin)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        items["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "proxy-one")
            .map(|p| p["id"].as_str().unwrap().to_string())
            .unwrap()
    };
    let r = app
        .client()
        .patch(format!("{}/api/providers/{proxy_id}", app.base))
        .bearer_auth(&admin)
        .json(&json!({"channel_kind": "zcode"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(all.len() >= 2, "两个渠道都在未过滤列表: {all:?}");

    let names_of = |cat: &str| {
        app.client()
            .get(format!(
                "{}/api/providers?category={cat}&limit=200",
                app.base
            ))
            .bearer_auth(&admin)
            .send()
    };
    let std: Vec<String> = names_of("standard")
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|p| p["name"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    let proxy: Vec<String> = names_of("proxy")
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|p| p["name"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        std.contains(&"std-one".to_string()) && !std.contains(&"proxy-one".to_string()),
        "standard: {std:?}"
    );
    assert!(
        proxy.contains(&"proxy-one".to_string()) && !proxy.contains(&"std-one".to_string()),
        "proxy: {proxy:?}"
    );
}

/// 渠道拆分:bulk 批量启停/删除(200 + 逐项结果;成功项立即失效拓扑)。
#[tokio::test]
async fn providers_bulk_enable_disable_delete() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let admin = app.admin_token().await;
    let p1 = seed_openai_provider(&app, &mock, "bulk-1").await;
    let p2 = app
        .seed_provider(&ProviderSeed {
            name: "bulk-2",
            base_url: &mock.base,
            ..ProviderSeed::default()
        })
        .await;

    // 批量禁用
    let r = app
        .client()
        .post(format!("{}/api/providers/bulk", app.base))
        .bearer_auth(&admin)
        .json(&json!({"action": "disable", "ids": [p1, p2]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["succeeded"], 2, "{body}");
    // 下一请求立即看到禁用:选路无渠道 → 503(通过 /v1 探测)
    let resp = app
        .client()
        .post(format!("{}/v1/chat/completions", app.base))
        .json(&json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]}))
        .bearer_auth(&app.seed_user_funded().await.api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503, "禁用后拓扑快照立即失效");

    // 批量启用
    let r = app
        .client()
        .post(format!("{}/api/providers/bulk", app.base))
        .bearer_auth(&admin)
        .json(&json!({"action": "enable", "ids": [p1.clone(), p2.clone()]}))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["succeeded"], 2, "{body}");

    // 批量删除(含一个不存在的 id → 逐项报告)
    let r = app
        .client()
        .post(format!("{}/api/providers/bulk", app.base))
        .bearer_auth(&admin)
        .json(&json!({"action": "delete", "ids": [p1, p2, "no-such-id"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["succeeded"], 2, "{body}");
    assert_eq!(body["requested"], 3);
    assert!(
        body["failed"]
            .as_array()
            .map(|f| f.len() == 1)
            .unwrap_or(false),
        "{body}"
    );

    let left: Value = app
        .client()
        .get(format!("{}/api/providers?limit=200", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(left["total"], 0, "{left}");
}

/// 渠道拆分:导出(含明文凭证)→ 删除 → 导入恢复(roundtrip)。
#[tokio::test]
async fn providers_export_import_roundtrip() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let admin = app.admin_token().await;
    let pid = app
        .seed_provider(&ProviderSeed {
            name: "rt-zcode",
            protocol: "anthropic",
            base_url: &mock.base,
            models: &["GLM-5.3"],
            api_key: "rt-jwt-secret-token",
            ..ProviderSeed::default()
        })
        .await;
    let r = app
        .client()
        .patch(format!("{}/api/providers/{pid}", app.base))
        .bearer_auth(&admin)
        .json(&json!({"channel_kind": "zcode"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 导出(proxy):明文 key 在文件里
    let export: Value = app
        .client()
        .get(format!("{}/api/providers/export?category=proxy", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(export["version"], 1);
    let items = export["providers"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 1, "{export}");
    assert_eq!(items[0]["api_key"], "rt-jwt-secret-token", "导出含明文凭证");
    assert_eq!(items[0]["channel_kind"], "zcode");
    assert_eq!(items[0]["default_protocol"], "anthropic");

    // 删除后导入恢复
    let r = app
        .client()
        .delete(format!("{}/api/providers/{pid}", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = app
        .client()
        .post(format!("{}/api/providers/import", app.base))
        .bearer_auth(&admin)
        .json(&export)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["imported"], 1, "{body}");

    // 恢复的渠道在 proxy 类别里且凭证可用(mock 上游收到原 key)
    let proxy_list: Value = app
        .client()
        .get(format!("{}/api/providers?category=proxy", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(proxy_list["total"], 1, "{proxy_list}");
    let new_pid = proxy_list["items"][0]["id"].as_str().unwrap().to_string();
    mock.push_json(200, json!({"data": [{"id": "GLM-5.3"}]}));
    let r = app
        .client()
        .post(format!("{}/api/providers/{new_pid}/tests", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let reqs = mock.requests();
    let sent = reqs
        .iter()
        .rev()
        .find(|q| q.path.contains("billing/balance"))
        .expect("健康探测打了 mock");
    assert!(
        sent.authorization.as_deref() == Some("Bearer rt-jwt-secret-token")
            || sent
                .headers
                .iter()
                .any(|(k, v)| k == "x-api-key" && v == "rt-jwt-secret-token"),
        "恢复后的凭证照常使用: auth={:?}",
        sent.authorization
    );

    // 再导入一遍:同 key → skipped
    let r = app
        .client()
        .post(format!("{}/api/providers/import", app.base))
        .bearer_auth(&admin)
        .json(&export)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["skipped"], 1, "同 key 跳过: {body}");
    assert_eq!(body["imported"], 0);
}

/// ZCode 在线登录:mock OAuth 端点走完整 init → poll(pending ×2 →
/// ready)→ 渠道自动创建(JWT 入库、zcode kind、anthropic 协议)。
#[tokio::test]
async fn zcode_oauth_login_flow() {
    let oauth = MockUpstream::start().await;
    let app = TestApp::spawn_with_env(&[
        ("AIKUN_ZCODE_CAPTCHA", "skip"),
        (
            "AIKUN_ZCODE_OAUTH_BASE",
            &format!("http://{}", oauth.base.trim_start_matches("http://")),
        ),
        ("AIKUN_HEALTH_CHECK_INTERVAL", "3600"),
    ])
    .await;
    let admin = app.admin_token().await;

    // 脚本:init 响应 → poll pending → pending → ready
    oauth.push_json(
        200,
        json!({"data": {"flow_id": "flow-1", "authorize_url": "https://zcode.z.ai/authorize?flow=flow-1"}}),
    );
    oauth.push_json(200, json!({"data": {"status": "pending"}}));
    oauth.push_json(200, json!({"data": {"status": "pending"}}));
    oauth.push_json(
        200,
        json!({"data": {"status": "ready", "token": "jwt-from-oauth-flow", "user": {"email": "dev@example.com"}}}),
    );

    // init
    let r = app
        .client()
        .post(format!("{}/api/providers/zcode/login-init", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "login-init");
    let init: Value = r.json().await.unwrap();
    assert_eq!(init["flow_id"], "flow-1");
    assert!(
        init["authorize_url"]
            .as_str()
            .unwrap()
            .contains("authorize")
    );
    let poll_token = init["poll_token"].as_str().unwrap().to_string();
    assert!(!poll_token.is_empty());

    // poll ×2 → pending
    for _ in 0..2 {
        let r = app
            .client()
            .post(format!("{}/api/providers/zcode/login-poll", app.base))
            .bearer_auth(&admin)
            .json(&json!({"flow_id": "flow-1", "poll_token": poll_token}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["status"], "pending", "{body}");
    }

    // poll → ready:渠道自动创建
    let r = app
        .client()
        .post(format!("{}/api/providers/zcode/login-poll", app.base))
        .bearer_auth(&admin)
        .json(&json!({"flow_id": "flow-1", "poll_token": poll_token}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["status"], "ready", "{body}");
    assert_eq!(body["email"], "dev@example.com");
    let provider = &body["provider"];
    assert_eq!(provider["channel_kind"], "zcode");
    assert!(
        provider["name"]
            .as_str()
            .unwrap_or("")
            .starts_with("zcode-"),
        "name 自动生成(token 唯一必需): {}",
        provider["name"]
    );
    assert_eq!(provider["default_protocol"], "anthropic");
    // 模型不再写死。登录后会拉一次官方配置；mock 不提供 catalog 时为空。
    assert!(provider["models"].is_array());

    // 上游收到 init(poll_token Bearer)与 poll
    let reqs = oauth.requests();
    let init_req = reqs
        .iter()
        .find(|q| q.path.ends_with("/oauth/cli/init"))
        .expect("init 打到 mock");
    assert!(
        init_req
            .authorization
            .as_deref()
            .unwrap_or("")
            .starts_with("Bearer "),
        "init 带 poll_token Bearer"
    );
    assert!(
        reqs.iter()
            .any(|q| q.path.contains("/oauth/cli/poll/flow-1")),
        "poll 打到 mock"
    );
}

/// ZCode 免费额度领取:完整序列(激活事件 → balance 已持有 → preview +
/// touch 待领清单 → 逐项 claim → campaign confirm)打到 mock 上游,
/// 结果分类(claimed / skipped)与转发头(Bearer JWT、X-Device-Mid)断言。
/// AIKUN_ZCODE_OAUTH_BASE 指向 mock(claim 端口同源复用该配置)。
#[tokio::test]
async fn zcode_claim_free_quota() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn_with_env(&[
        ("AIKUN_ZCODE_CAPTCHA", "skip"),
        (
            "AIKUN_ZCODE_OAUTH_BASE",
            &format!("http://{}", mock.base.trim_start_matches("http://")),
        ),
        ("AIKUN_HEALTH_CHECK_INTERVAL", "3600"),
    ])
    .await;
    let admin = app.admin_token().await;

    // 带可解析 sub 的 JWT 形态 key(jwt_sub 命中 → 激活事件会发)。
    let jwt = "h.eyJzdWIiOiJ1LTEyMyJ9.s";
    app.seed_provider(&ProviderSeed {
        name: "zcode-claim",
        protocol: "anthropic",
        base_url: &mock.base,
        models: &["GLM-5.3-Flash"],
        api_key: jwt,
        ..ProviderSeed::default()
    })
    .await;
    let provs = app
        .client()
        .get(format!("{}/api/providers", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let provs: Value = provs.json().await.unwrap();
    let pid = provs["items"][0]["id"].as_str().unwrap().to_string();
    let r = app
        .client()
        .patch(format!("{}/api/providers/{pid}", app.base))
        .bearer_auth(&admin)
        .json(&json!({"channel_kind": "zcode"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "PATCH channel_kind=zcode");

    // 上游脚本:事件 ×2 → balance → preview → touch → claim ×2 → confirm
    mock.push_json(200, json!({"code": 0}));
    mock.push_json(200, json!({"code": 0}));
    mock.push_json(
        200,
        json!({"code": 0, "data": {"plans": [{"plan_id": "owned_plan", "name": "Lite"}]}}),
    );
    mock.push_json(
        200,
        json!({"code": 0, "data": {"plans": [
            {"plan_id": "owned_plan", "name": "Lite", "priority": 1},
            {"plan_id": "daily_free", "name": "每日免费额度", "priority": 5},
        ]}}),
    );
    mock.push_json(
        200,
        json!({"code": 0, "data": {"deliveries": [{"campaign_id": "camp1", "banner": {"buttons": [
            {"action": {"args": {"plan_id": "touch_bonus"}}},
        ]}}]}}),
    );
    mock.push_json(200, json!({"code": 0}));
    mock.push_json(200, json!({"code": 0}));
    mock.push_json(200, json!({"code": 0}));

    let r = app
        .client()
        .post(format!("{}/api/providers/{pid}/zcode/claim", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "claim 端点 200");
    let res: Value = r.json().await.unwrap();
    assert_eq!(res["problems"].as_array().map(Vec::len), Some(0), "{res}");
    let claimed = res["claimed"].as_array().unwrap().clone();
    assert!(claimed.iter().any(|v| v == "每日免费额度"), "{res}");
    assert!(
        claimed.iter().any(|v| v == "touch_bonus"),
        "touch-only plan 也领取:{res}"
    );
    assert!(
        res["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "Lite"),
        "balance 已持有的跳过:{res}"
    );

    let reqs = mock.requests();
    // 激活事件:device_mid 稳定派生 + user_id 来自 JWT sub
    let events: Vec<_> = reqs
        .iter()
        .filter(|q| q.path.ends_with("/event/report"))
        .collect();
    assert_eq!(events.len(), 2, "app_launch + app_daily_active");
    let ev_body: Value = serde_json::from_str(&events[0].body).unwrap();
    assert_eq!(ev_body["user_id"], "u-123");
    assert!(
        ev_body["device_mid"].as_str().unwrap().contains('-'),
        "UUID 形态 device_mid"
    );
    // balance / preview / touch 各一次
    assert!(reqs.iter().any(|q| q.path.contains("/billing/balance")));
    assert!(reqs.iter().any(|q| q.path.contains("/billing/preview")));
    assert!(reqs.iter().any(|q| q.path.contains("/marketing/touch")));
    // claim:Bearer JWT + billing 伪装头 + plan_id 体
    let claims: Vec<_> = reqs
        .iter()
        .filter(|q| q.path.ends_with("/billing/claim"))
        .collect();
    assert_eq!(claims.len(), 2, "两个待领 plan 各一次 claim");
    let bodies: Vec<Value> = claims
        .iter()
        .map(|q| serde_json::from_str(&q.body).unwrap())
        .collect();
    assert!(
        bodies.iter().any(|b| b["plan_id"] == "daily_free"
            && bodies.iter().any(|b2| b2["plan_id"] == "touch_bonus"))
    );
    assert_eq!(
        claims[0].authorization.as_deref(),
        Some(format!("Bearer {jwt}").as_str())
    );
    assert!(
        claims[0]
            .headers
            .iter()
            .any(|(k, v)| k == "x-device-mid" && v.contains('-')),
        "billing 头带 device_mid"
    );
    // campaign confirm
    assert!(
        reqs.iter()
            .any(|q| q.path.ends_with("/marketing/touch/action")),
        "campaigned plan 领取后 confirm"
    );
}

/// MCP 远程工具透传:GET /v1/mcp 目录 + POST /v1/mcp/{server_key}
/// (JSON-RPC 体、Mcp-Session-Id 头转发、JWT 与伪装头送达上游、
/// 响应体原样回传)。
#[tokio::test]
async fn mcp_passthrough_end_to_end() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    let jwt = "fake-zcode-jwt-token";
    app.seed_provider(&ProviderSeed {
        name: "zcode-mcp",
        protocol: "anthropic",
        base_url: &mock.base,
        models: &["GLM-5.3-Flash"],
        api_key: jwt,
        ..ProviderSeed::default()
    })
    .await;
    let admin = app.admin_token().await;
    let provs = app
        .client()
        .get(format!("{}/api/providers", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let provs: Value = provs.json().await.unwrap();
    let pid = provs["items"][0]["id"].as_str().unwrap().to_string();
    let r = app
        .client()
        .patch(format!("{}/api/providers/{pid}", app.base))
        .bearer_auth(&admin)
        .json(&json!({"channel_kind": "zcode"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "PATCH channel_kind=zcode");

    // 目录
    let r = app
        .client()
        .get(format!("{}/v1/mcp", app.base))
        .bearer_auth(&user.api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "GET /v1/mcp");
    let cat: Value = r.json().await.unwrap();
    assert!(
        cat["servers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["key"] == "finance_tianyancha"),
        "{cat}"
    );

    // tools/call 透传(上游单条 JSON-RPC 响应)
    mock.push_json(
        200,
        json!({"jsonrpc": "2.0", "id": 2, "result": {"content": [{"type": "text", "text": "智谱华章"}]}}),
    );
    let r = app
        .client()
        .post(format!("{}/v1/mcp/finance_tianyancha", app.base))
        .bearer_auth(&user.api_key)
        .header("Mcp-Session-Id", "sess-1")
        .json(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "search_companies", "arguments": {"query": "智谱"}}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "POST /v1/mcp/finance_tianyancha");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["result"]["content"][0]["text"], "智谱华章", "{body}");

    let reqs = mock.requests();
    let mcp_req = reqs
        .iter()
        .find(|q| q.path.ends_with("/api/v1/mcp/server/finance_tianyancha"))
        .expect("MCP 请求打到上游");
    assert_eq!(
        mcp_req.authorization.as_deref(),
        Some(format!("Bearer {jwt}").as_str()),
        "渠道 JWT 送上游"
    );
    let sent: Value = serde_json::from_str(&mcp_req.body).unwrap();
    assert_eq!(sent["method"], "tools/call", "JSON-RPC 体透传");
    assert_eq!(sent["params"]["arguments"]["query"], "智谱", "扩展字段存活");
    assert!(
        mcp_req
            .headers
            .iter()
            .any(|(k, v)| k == "mcp-session-id" && v == "sess-1"),
        "客户端 MCP 会话头转发"
    );
    assert!(
        mcp_req
            .headers
            .iter()
            .any(|(k, v)| k == "x-zcode-agent" && v == "glm"),
        "伪装头注入"
    );

    // 无 zcode 渠道的 server_key 非法 → 400(生成层 pattern → vendor 形状)
    let r = app
        .client()
        .post(format!("{}/v1/mcp/bad$key", app.base))
        .bearer_auth(&user.api_key)
        .json(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "非法 server_key 400");
    let err: Value = r.json().await.unwrap();
    assert_eq!(err["error"]["type"], "invalid_request_error", "{err}");
}

/// kind 过滤(应用中心归组)+ 领取写 proxy_task 快照 + tasks 面板端点
/// + zai-plan(国际 API Key 形态)标准 Anthropic 转发。
#[tokio::test]
async fn providers_kind_filter_and_claim_tasks() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn_with_env(&[
        ("AIKUN_ZCODE_CAPTCHA", "skip"),
        (
            "AIKUN_ZCODE_OAUTH_BASE",
            &format!("http://{}", mock.base.trim_start_matches("http://")),
        ),
        ("AIKUN_HEALTH_CHECK_INTERVAL", "3600"),
    ])
    .await;
    let admin = app.admin_token().await;

    // zcode 渠道(无 sub 的 key → 激活事件跳过,领取序列短)
    app.seed_provider(&ProviderSeed {
        name: "zc",
        protocol: "anthropic",
        base_url: &mock.base,
        models: &["GLM-5.3-Flash"],
        api_key: "fake-jwt-no-sub",
        ..ProviderSeed::default()
    })
    .await;
    // 渠道类型设为 zcode(种子走 generic 默认,PATCH 改 kind)
    let provs = app
        .client()
        .get(format!("{}/api/providers", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let provs: Value = provs.json().await.unwrap();
    let zc_pid = provs["items"][0]["id"].as_str().unwrap().to_string();
    let r = app
        .client()
        .patch(format!("{}/api/providers/{zc_pid}", app.base))
        .bearer_auth(&admin)
        .json(&json!({"channel_kind": "zcode"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "PATCH channel_kind=zcode");
    // zai-plan 渠道(国际 API Key 形态;kind 在创建时直接带)
    let r = app
        .client()
        .post(format!("{}/api/providers", app.base))
        .bearer_auth(&admin)
        .json(&serde_json::json!({
            "name": "zai-key", "provider_type": "anthropic",
            "anthropic_base_url": mock.base, "api_key": "zai-key-1",
            "models": ["GLM-5.3"], "protocols": ["anthropic"], "default_protocol": "anthropic",
            "channel_kind": "zai-plan",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "zai-plan 创建");

    // kind 过滤
    let get_ids = |q: &str| {
        let url = format!("{}/api/providers?category=proxy{q}", app.base);
        url
    };
    let _ = get_ids;
    let r = app
        .client()
        .get(format!(
            "{}/api/providers?category=proxy&kind=zcode",
            app.base
        ))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    let kinds: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["channel_kind"].as_str().unwrap_or(""))
        .collect();
    assert!(kinds.iter().all(|k| *k == "zcode"), "{kinds:?}");
    let r = app
        .client()
        .get(format!(
            "{}/api/providers?category=proxy&kind=zcode,zai-plan",
            app.base
        ))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(
        v["items"].as_array().map(Vec::len),
        Some(2),
        "多值 kind: {v}"
    );

    // zai-plan 转发:标准 Anthropic 形态打到 mock 的 /v1/messages
    let user = app.seed_user_funded().await;
    mock.push_json(
        200,
        json!({"id": "m1", "type": "message", "role": "assistant",
               "content": [{"type": "text", "text": "hi"}],
               "model": "GLM-5.3", "stop_reason": "end_turn",
               "usage": {"input_tokens": 1, "output_tokens": 1}}),
    );
    let r = app
        .client()
        .post(format!("{}/v1/messages", app.base))
        .bearer_auth(&user.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&json!({"model": "GLM-5.3", "max_tokens": 16,
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "zai-plan 转发 200");
    let reqs = mock.requests();
    let mh = reqs
        .iter()
        .find(|q| q.path.ends_with("/v1/messages"))
        .expect("打到 /v1/messages(标准 Anthropic 形态)");
    assert_eq!(mh.authorization.as_deref(), Some("Bearer zai-key-1"));

    // 领取(无 sub key → 序列:balance → preview → touch → claim)
    let zc_id = {
        let r = app
            .client()
            .get(format!(
                "{}/api/providers?category=proxy&kind=zcode",
                app.base
            ))
            .bearer_auth(&admin)
            .send()
            .await
            .unwrap();
        let v: Value = r.json().await.unwrap();
        v["items"][0]["id"].as_str().unwrap().to_string()
    };
    mock.push_json(
        200,
        json!({"code": 0, "data": {"plans": [{"plan_id": "owned"}]}}),
    );
    mock.push_json(
        200,
        json!({"code": 0, "data": {"plans": [{"plan_id": "daily_free", "name": "每日免费额度", "priority": 5}]}}),
    );
    mock.push_json(200, json!({"code": 0, "data": {"deliveries": []}}));
    mock.push_json(200, json!({"code": 0}));
    let r = app
        .client()
        .post(format!("{}/api/providers/{zc_id}/zcode/claim", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let res: Value = r.json().await.unwrap();
    assert!(
        res["claimed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "每日免费额度"),
        "{res}"
    );

    // tasks 面板:快照已写(source=manual,ok=true)
    let r = app
        .client()
        .get(format!("{}/api/providers/zcode/tasks", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let t: Value = r.json().await.unwrap();
    let rec = t["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["provider_id"].as_str() == Some(zc_id.as_str()))
        .expect("proxy_task 快照存在");
    assert_eq!(rec["source"], "manual");
    assert_eq!(rec["ok"], true);
    assert_eq!(rec["task_type"], "zcode_claim");
}

/// 代理池 CRUD + 渠道批量添加选用代理(proxy_url 拷贝语义)。
#[tokio::test]
async fn proxy_pool_crud_and_channel_pick() {
    let app = TestApp::spawn().await;
    let admin = app.admin_token().await;

    // 非法 scheme 拒绝
    let r = app
        .client()
        .post(format!("{}/api/proxies", app.base))
        .bearer_auth(&admin)
        .json(&json!({"name": "bad", "url": "ftp://x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "非法 scheme 400");

    // 创建 socks5 代理
    let r = app
        .client()
        .post(format!("{}/api/proxies", app.base))
        .bearer_auth(&admin)
        .json(&json!({"name": "香港中转", "url": "socks5://u:p@127.0.0.1:1080", "note": "n1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let created: Value = r.json().await.unwrap();
    assert_eq!(created["url"], "socks5://u:p@127.0.0.1:1080");
    let pid = created["id"].as_str().unwrap().to_string();

    // 列表
    let r = app
        .client()
        .get(format!("{}/api/proxies", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["items"].as_array().map(Vec::len), Some(1));

    // 批量建渠道选用代理(proxy_url 拷贝进渠道)
    let r = app
        .client()
        .post(format!("{}/api/providers/batch", app.base))
        .bearer_auth(&admin)
        .json(&json!({
            "name": "zc", "channel_kind": "zcode", "protocols": ["anthropic"],
            "default_protocol": "anthropic",
            "openai_base_url": "https://zcode.z.ai", "anthropic_base_url": "https://zcode.z.ai",
            "api_keys": ["jwt-1"], "models": ["GLM-5.3"],
            "proxy_url": "socks5://u:p@127.0.0.1:1080",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "批量建渠道");
    let r = app
        .client()
        .get(format!("{}/api/providers?category=proxy", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(
        v["items"][0]["proxy_url"], "socks5://127.0.0.1:1080",
        "代理 URL 拷贝进渠道(出参脱敏口径)"
    );

    // 删除池条目;渠道不受影响
    let r = app
        .client()
        .delete(format!("{}/api/proxies/{pid}", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = app
        .client()
        .get(format!("{}/api/providers?category=proxy", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(
        v["items"][0]["proxy_url"], "socks5://127.0.0.1:1080",
        "删池条目不影响存量渠道(出参脱敏口径)"
    );
    // 再删 → 404
    let r = app
        .client()
        .delete(format!("{}/api/proxies/{pid}", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

/// 国内(BigModel 账号)在线登录:init 的 provider=bigmodel、ready 后
/// 渠道 base=zcode.bigmodel.cn(与账号体系同环境)。
#[tokio::test]
async fn zcode_oauth_login_cn_env() {
    let oauth = MockUpstream::start().await;
    let app = TestApp::spawn_with_env(&[
        ("AIKUN_ZCODE_CAPTCHA", "skip"),
        (
            "AIKUN_ZCODE_OAUTH_BASE",
            &format!("http://{}", oauth.base.trim_start_matches("http://")),
        ),
        ("AIKUN_HEALTH_CHECK_INTERVAL", "3600"),
    ])
    .await;
    let admin = app.admin_token().await;

    oauth.push_json(
        200,
        json!({"data": {"flow_id": "flow-cn", "authorize_url": "https://bigmodel.cn/login?appId=zcode&state=x"}}),
    );
    oauth.push_json(
        200,
        json!({"data": {"status": "ready", "token": "jwt-from-cn-oauth", "user": {"email": "cn@example.com"}}}),
    );

    let r = app
        .client()
        .post(format!("{}/api/providers/zcode/login-init", app.base))
        .bearer_auth(&admin)
        .json(&json!({"env": "cn"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "login-init(env=cn)");
    let r = app
        .client()
        .post(format!("{}/api/providers/zcode/login-poll", app.base))
        .bearer_auth(&admin)
        .json(&json!({"flow_id": "flow-cn", "poll_token": "any", "env": "cn"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["status"], "ready");
    let base = v["provider"]["anthropic_base_url"].as_str().unwrap();
    assert_eq!(
        base, "https://zcode.z.ai",
        "国内登录 API 也走 zcode.z.ai(bigmodel.cn 不可用)"
    );
    assert!(
        v["provider"]["note"]
            .as_str()
            .unwrap_or("")
            .starts_with("[国内]"),
        "国内标记进 note 前缀"
    );

    // init 请求体带 provider=bigmodel
    let reqs = oauth.requests();
    let init = reqs
        .iter()
        .find(|q| q.path.ends_with("/oauth/cli/init"))
        .expect("init 打到 mock");
    let body: Value = serde_json::from_str(&init.body).unwrap();
    assert_eq!(body["provider"], "bigmodel");
}

/// 国内手机号登录的 ready 响应:user.email 为空串(智谱账号常无邮箱)。
/// name 不依赖 email/phone:自动生成 zcode-{6hex},渠道正常创建。
#[tokio::test]
async fn zcode_oauth_login_cn_empty_email_falls_back() {
    let oauth = MockUpstream::start().await;
    let app = TestApp::spawn_with_env(&[
        ("AIKUN_ZCODE_CAPTCHA", "skip"),
        (
            "AIKUN_ZCODE_OAUTH_BASE",
            &format!("http://{}", oauth.base.trim_start_matches("http://")),
        ),
        ("AIKUN_HEALTH_CHECK_INTERVAL", "3600"),
    ])
    .await;
    let admin = app.admin_token().await;

    oauth.push_json(
        200,
        json!({"data": {"flow_id": "f2", "authorize_url": "https://bigmodel.cn/login?appId=zcode"}}),
    );
    // email 空串 + phone 存在(国内手机号账号形态)
    oauth.push_json(
        200,
        json!({"data": {"status": "ready", "token": "jwt-cn-phone", "user": {"email": "", "phone": "13800001111"}}}),
    );

    let r = app
        .client()
        .post(format!("{}/api/providers/zcode/login-init", app.base))
        .bearer_auth(&admin)
        .json(&json!({"env": "cn"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = app
        .client()
        .post(format!("{}/api/providers/zcode/login-poll", app.base))
        .bearer_auth(&admin)
        .json(&json!({"flow_id": "f2", "poll_token": "any", "env": "cn"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "空 email 不再 400");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["status"], "ready");
    assert!(
        v["provider"]["name"]
            .as_str()
            .unwrap_or("")
            .starts_with("zcode-"),
        "name 自动生成: {}",
        v["provider"]["name"]
    );

    // email/phone 全空:兜底 zcode-user(直接 poll,不再消费 init 响应)
    oauth.push_json(
        200,
        json!({"data": {"status": "ready", "token": "jwt-x", "user": {"email": ""}}}),
    );
    let r = app
        .client()
        .post(format!("{}/api/providers/zcode/login-poll", app.base))
        .bearer_auth(&admin)
        .json(&json!({"flow_id": "f3", "poll_token": "any", "env": "intl"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert!(
        v["provider"]["name"]
            .as_str()
            .unwrap_or("")
            .starts_with("zcode-"),
        "全空 email/phone 也不再 400,name 自动生成: {v}"
    );
}

/// 一键获取账号信息:token 查 billing/balance,自动生成名回填为账号
/// 标识、note 写摘要;手动命名不覆盖。
#[tokio::test]
async fn zcode_account_info_fetch_and_backfill() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn_with_env(&[
        ("AIKUN_ZCODE_CAPTCHA", "skip"),
        (
            "AIKUN_ZCODE_OAUTH_BASE",
            &format!("http://{}", mock.base.trim_start_matches("http://")),
        ),
        ("AIKUN_HEALTH_CHECK_INTERVAL", "3600"),
    ])
    .await;
    let admin = app.admin_token().await;

    // 建一个自动生成名的 zcode 渠道(在线登录;email/phone 全空)
    mock.push_json(
        200,
        json!({"data": {"flow_id": "f9", "authorize_url": "https://x"}}),
    );
    mock.push_json(
        200,
        json!({"data": {"status": "ready", "token": "jwt-info", "user": {"email": "", "phone": ""}}}),
    );
    let r = app
        .client()
        .post(format!("{}/api/providers/zcode/login-init", app.base))
        .bearer_auth(&admin)
        .json(&json!({"env": "intl"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = app
        .client()
        .post(format!("{}/api/providers/zcode/login-poll", app.base))
        .bearer_auth(&admin)
        .json(&json!({"flow_id": "f9", "poll_token": "any", "env": "intl"}))
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    let pid = v["provider"]["id"].as_str().unwrap().to_string();
    assert!(
        v["provider"]["name"]
            .as_str()
            .unwrap()
            .starts_with("zcode-")
    );
    // 渠道 base 指向 mock(account-info/领取一样跟渠道 base 走)
    let r = app
        .client()
        .patch(format!("{}/api/providers/{pid}", app.base))
        .bearer_auth(&admin)
        .json(&json!({"openai_base_url": mock.base, "anthropic_base_url": mock.base}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "PATCH base→mock");

    // 一键获取账号信息(balance 返回账号字段;intl 无业务 token →
    // 顺带查 reset/status,订阅/额度跳过)
    mock.push_json(
        200,
        json!({"code": 0, "data": {"account_id": "acc-123", "email": "user@z.ai",
               "plans": [{"plan_id": "p1", "name": "Pro"}],
               "balances": []}}),
    );
    mock.push_json(
        200,
        json!({"code": 0, "data": {"available_five_hour_resets": [],
               "available_week_resets": [{"expire_at": 1797955199}],
               "latest_five_hour_reset_history": null,
               "latest_week_reset_history": {"used_at": 1793604157}}}),
    );
    let r = app
        .client()
        .post(format!(
            "{}/api/providers/{pid}/zcode/account-info",
            app.base
        ))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "account-info 200");
    let info: Value = r.json().await.unwrap();
    assert_eq!(info["account_id"], "acc-123");
    assert_eq!(info["display"], "user@z.ai");
    assert_eq!(info["plan_name"], "Pro");
    assert_eq!(info["name_updated"], true, "自动生成名回填");
    assert_eq!(
        info["plan_credentials"], false,
        "批量/无家族 token 渠道无订阅凭证"
    );
    assert_eq!(
        info["reset_status"]["available_week_resets"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "周重置次数透传"
    );
    assert!(info["subscription"].is_null(), "无业务 token → 订阅 null");
    assert!(info["quota"].is_null(), "无业务 token → 额度 null");

    // 渠道已回填
    let r = app
        .client()
        .get(format!(
            "{}/api/providers?category=proxy&kind=zcode",
            app.base
        ))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    let row = &v["items"][0];
    assert_eq!(row["name"], "user@z.ai", "名称回填为账号标识");
    assert!(
        !row["note"].as_str().unwrap_or("").contains("acc-123"),
        "note 不再写账号摘要(2026-11-20 改版:名称即备注,note 只留归组标记)"
    );

    // GET 缓存读:POST 已持久化,fetched_at + info 都在
    let r = app
        .client()
        .get(format!(
            "{}/api/providers/{pid}/zcode/account-info",
            app.base
        ))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "GET 缓存 200");
    let cached: Value = r.json().await.unwrap();
    assert!(cached["fetched_at"].as_str().is_some(), "有拉取时间");
    assert_eq!(cached["info"]["account_id"], "acc-123", "缓存含账号信息");

    // 手动命名不被覆盖:改名后再获取,保持手动名
    let r = app
        .client()
        .patch(format!("{}/api/providers/{pid}", app.base))
        .bearer_auth(&admin)
        .json(&json!({"name": "我的主号"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    mock.push_json(
        200,
        json!({"code": 0, "data": {"account_id": "acc-123", "email": "user@z.ai", "plans": [{"name": "Pro"}]}}),
    );
    mock.push_json(
        200,
        json!({"code": 0, "data": {"available_five_hour_resets": [],
               "available_week_resets": []}}),
    );
    let r = app
        .client()
        .post(format!(
            "{}/api/providers/{pid}/zcode/account-info",
            app.base
        ))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let info: Value = r.json().await.unwrap();
    assert_eq!(info["name_updated"], false, "手动命名不覆盖");
}

/// ZCode 订阅/额度/重置三件套(Lite 等付费 Coding Plan):
/// 国内在线登录捕获 monitor 业务 token(poll 响应 data.bigmodel.
/// access_token)→ account-info 聚合(订阅 + 5h/周额度 + 重置次数,
/// monitor 请求带业务 token)→ reset-quota 消耗性重置(幂等键 +
/// 双头认证)→ 同一账号重复在线登录走「回填业务 token」而非 409。
#[tokio::test]
async fn zcode_plan_subscription_and_reset() {
    let mock = MockUpstream::start().await;
    let mock_host = format!("http://{}", mock.base.trim_start_matches("http://"));
    let app = TestApp::spawn_with_env(&[
        ("AIKUN_ZCODE_CAPTCHA", "skip"),
        ("AIKUN_ZCODE_OAUTH_BASE", &mock_host),
        ("AIKUN_ZCODE_BIGMODEL_HOST", &mock_host),
        ("AIKUN_HEALTH_CHECK_INTERVAL", "3600"),
    ])
    .await;
    let admin = app.admin_token().await;

    // ---- 国内在线登录:poll ready 携带 bigmodel 业务 token ----
    // 注意:登录建的渠道 base 是常量 zcode.z.ai(真网),ready 后 spawn
    // 的自动领取打真网(伪 JWT → 上游拒绝 → 快照记 problems,与
    // zcode_oauth_login_flow 同款容忍);等快照落库后 PATCH base→mock
    // 再走后续脚本,保证队列不被后台任务竞态消费。
    mock.push_json(
        200,
        json!({"data": {"flow_id": "f1", "authorize_url": "https://x"}}),
    );
    mock.push_json(
        200,
        json!({"data": {"status": "ready", "token": "jwt-cn-plan",
               "user": {"phone": "13800000000"},
               "bigmodel": {"access_token": "bm-token-1", "refresh_token": "r1"}}}),
    );
    let r = app
        .client()
        .post(format!("{}/api/providers/zcode/login-init", app.base))
        .bearer_auth(&admin)
        .json(&json!({"env": "cn"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "login-init");
    let r = app
        .client()
        .post(format!("{}/api/providers/zcode/login-poll", app.base))
        .bearer_auth(&admin)
        .json(&json!({"flow_id": "f1", "poll_token": "any", "env": "cn"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "login-poll ready");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["status"], "ready");
    let pid = v["provider"]["id"].as_str().unwrap().to_string();
    assert!(
        v["provider"]["note"]
            .as_str()
            .unwrap_or("")
            .starts_with("[国内]"),
        "国内归组标记"
    );

    // 等后台自动领取结束(proxy_task 快照落库)——脚本队列 FIFO,
    // 不等会把后续 account-info 的响应吃掉
    for _ in 0..300 {
        let r = app
            .client()
            .get(format!("{}/api/providers/zcode/tasks", app.base))
            .bearer_auth(&admin)
            .send()
            .await
            .unwrap();
        let t: Value = r.json().await.unwrap();
        if t["tasks"]
            .as_array()
            .map(|a| a.iter().any(|x| x["provider_id"] == pid.as_str()))
            .unwrap_or(false)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // 渠道 base 切到 mock(account-info / reset / monitor 脚本全走 mock)
    let r = app
        .client()
        .patch(format!("{}/api/providers/{pid}", app.base))
        .bearer_auth(&admin)
        .json(&json!({"openai_base_url": mock.base, "anthropic_base_url": mock.base}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "PATCH base→mock");

    // ---- account-info 聚合:balance + reset/status + monitor 订阅/额度 ----
    mock.push_json(
        200,
        json!({"code": 0, "data": {"plans": [{"plan_id": "zcode-v3-start-plan", "name": "ZCODE GLOBAL BUILD"}],
               "balances": [{"show_name": "GLM-5.3-Flash", "used_units": 1000, "total_units": 300000000,
                              "expires_at": 1795136400}]}}),
    );
    mock.push_json(
        200,
        json!({"code": 0, "data": {"available_five_hour_resets": [],
               "available_week_resets": [{"expire_at": 1797955199}],
               "latest_five_hour_reset_history": null,
               "latest_week_reset_history": {"used_at": 1793604157}}}),
    );
    mock.push_json(
        200,
        json!({"code": 0, "data": [{"productId": "coding-lite", "productName": "GLM Coding Plan Lite",
               "status": "active", "billingCycle": "monthly", "expireTime": 1800000000}]}),
    );
    mock.push_json(
        200,
        json!({"code": 0, "data": {"level": "lite", "limits": [
            {"type": "TIME_LIMIT_5H", "unit": 1, "number": 120, "usage": 30, "remaining": 90,
             "percentage": 25, "nextResetTime": 1795120000},
            {"type": "TIME_LIMIT_WEEK", "unit": 1, "number": 600, "usage": 100, "remaining": 500,
             "percentage": 16, "nextResetTime": 1795700000}]}}),
    );
    let r = app
        .client()
        .post(format!(
            "{}/api/providers/{pid}/zcode/account-info",
            app.base
        ))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "account-info 200");
    let info: Value = r.json().await.unwrap();
    assert_eq!(info["plan_credentials"], true, "业务 token 已落库");
    assert_eq!(
        info["reset_status"]["available_week_resets"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        info["subscription"][0]["productName"],
        "GLM Coding Plan Lite"
    );
    assert_eq!(info["quota"]["limits"].as_array().unwrap().len(), 2);
    assert!(info["warnings"].as_array().unwrap().is_empty(), "{info}");

    // monitor 请求带业务 token(单 authorization 头,无 Bearer 前缀)
    let reqs = mock.requests();
    let sub_req = reqs
        .iter()
        .find(|q| q.path.ends_with("/api/biz/subscription/list"))
        .expect("订阅请求打到 monitor mock");
    assert_eq!(
        sub_req.authorization.as_deref().unwrap_or(""),
        "bm-token-1",
        "monitor 用 OAuth 业务 token"
    );

    // ---- reset-quota:消耗性重置(幂等键 + 双头认证) ----
    mock.push_json(200, json!({"code": 0, "data": {"used": true}}));
    mock.push_json(
        200,
        json!({"code": 0, "data": {"available_five_hour_resets": [],
               "available_week_resets": []}}),
    );
    let r = app
        .client()
        .post(format!(
            "{}/api/providers/{pid}/zcode/reset-quota",
            app.base
        ))
        .bearer_auth(&admin)
        .json(&json!({"reset_type": "WEEK"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "reset-quota 200");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["used"], true);
    let reqs = mock.requests();
    let use_req = reqs
        .iter()
        .find(|q| q.path.ends_with("/coding-plan/reset/use"))
        .expect("reset/use 打到 mock");
    let body: Value = serde_json::from_str(&use_req.body).unwrap();
    assert_eq!(body["reset_type"], "WEEK");
    assert!(
        body["idempotency_key"].as_str().unwrap_or("").len() >= 16,
        "幂等键随请求生成"
    );
    let hdr = |k: &str| {
        use_req
            .headers
            .iter()
            .find(|(hk, _)| hk == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert_eq!(hdr("x-bigmodel-authorization"), "jwt-cn-plan", "双头认证");
    assert_eq!(hdr("bigmodel-target-type"), "PERSONAL", "个人口径头");

    // ---- 同一账号重复在线登录:回填业务 token,不报 409 ----
    mock.push_json(
        200,
        json!({"data": {"flow_id": "f2", "authorize_url": "https://x"}}),
    );
    mock.push_json(
        200,
        json!({"data": {"status": "ready", "token": "jwt-cn-plan",
               "user": {"phone": "13800000000"},
               "bigmodel": {"access_token": "bm-token-2"}}}),
    );
    let r = app
        .client()
        .post(format!("{}/api/providers/zcode/login-init", app.base))
        .bearer_auth(&admin)
        .json(&json!({"env": "cn"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = app
        .client()
        .post(format!("{}/api/providers/zcode/login-poll", app.base))
        .bearer_auth(&admin)
        .json(&json!({"flow_id": "f2", "poll_token": "any", "env": "cn"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "重复登录不 409");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["status"], "ready", "{v}");
    assert_eq!(v["provider"]["id"], pid, "定位到既有渠道");

    // 回填后的业务 token 生效:account-info 的 monitor 请求用新 token
    mock.push_json(
        200,
        json!({"code": 0, "data": {"plans": [], "balances": []}}),
    );
    mock.push_json(
        200,
        json!({"code": 0, "data": {"available_five_hour_resets": [],
               "available_week_resets": []}}),
    );
    mock.push_json(200, json!({"code": 0, "data": []}));
    mock.push_json(200, json!({"code": 0, "data": {"limits": []}}));
    let r = app
        .client()
        .post(format!(
            "{}/api/providers/{pid}/zcode/account-info",
            app.base
        ))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let reqs = mock.requests();
    let last_sub = reqs
        .iter()
        .rev()
        .find(|q| q.path.ends_with("/api/biz/subscription/list"))
        .expect("第二次订阅请求");
    assert_eq!(
        last_sub.authorization.as_deref().unwrap_or(""),
        "bm-token-2",
        "重复登录回填了新业务 token"
    );
}
