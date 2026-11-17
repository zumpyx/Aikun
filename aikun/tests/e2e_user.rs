//! User tag e2e:新路径(me / version / api-keys CRUD / logs / logs-stats /
//! wallet / redemptions / models-prices)。
//!
//! **独立集成测试目标**(docs/migration-playbook.md §三):只测 User tag
//! 新路径;旧路径用例留在 tests/e2e.rs,随存量 handler 删除由集成工人处理。
//!
//! 覆盖的核心断言(强度不低于迁移前基线,见 e2e.rs 的
//! admin_manages_other_users_api_keys / wallet_returns_own_balance_and_
//! daily_costs / redemption_code_full_flow / public_prices_expand_wildcard_...):
//! * api-keys CRUD:{items,total} 信封、明文仅一次、掩码恒定、
//!   PATCH 返回资源、DELETE 204、跨用户 404(不暴露存在性)、
//!   user_id 参数仅 admin 生效;
//! * 兑换码全流程:201 创建语义(旧 200)、同事务核销+加余额+流水
//!   (kind='recharge')、重复/无效/过期/禁用统一 400 invalid_code
//!   problem+json、归一化(小写/内嵌空格)、5 次失败 429 + Retry-After;
//! * 钱包:余额/今日/近 30 天合计、30 天补零日序列、top_models;
//! * 日志:{items,total}、LEFT JOIN 用户名/渠道名(删除后 null)、
//!   过滤参数(model/success/user_id)与统计口径一致、非 admin 强制只看自己;
//!   (数据经真实 /v1 请求 + 受控 usage 的 mock 上游产生;历史时间窗的
//!   库内形态检查随检查通道改造移至 store 单测层,
//!   docs/surreal-migration.md §七)
//! * 生成器实际行为锚定:success 非 0/1 → 400 `malformed_parameter`
//!   (spec 示例 invalid_success 不可达,缺口见 playbook/notes)、
//!   空 body PATCH → 422 request_validation_failed。

mod common;

use std::time::Duration;

use common::{ProviderSeed, TestApp};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(10);

/// 断言响应为 problem+json 且 code 符合预期,返回整个 body 供补充断言。
async fn assert_problem(res: reqwest::Response, status: u16, code: &str) -> Value {
    assert_eq!(res.status(), status, "{:?}", res.text().await);
    let ct = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        ct.starts_with("application/problem+json"),
        "content-type 应为 problem+json,实际 {ct}"
    );
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["code"].as_str(), Some(code), "{body}");
    body
}

/// 受控 usage 的 OpenAI 补全(日志用例需要精确 token 数)。
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

/// GET /api/users/me + GET /api/system/version 新路径:
/// 200 形状(balance 元、时间 RFC3339、无 password_hash)、匿名 401、
/// 用户被软禁用后已签发 token → 401 invalid_token problem+json
/// (auth 中间件的 DB 复核)。
#[tokio::test]
async fn me_and_version_new_paths() {
    let app = TestApp::spawn().await;
    let user = app.seed_user().await;
    app.adjust_balance(&user.id, 1.0).await; // 余额 1 元
    let client = app.client();

    // 匿名 → 401(body 由装配层 auth_middleware 产生,只锚状态码)
    let anon = client
        .get(format!("{}/api/users/me", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
    let anon = client
        .get(format!("{}/api/system/version", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401, "version 刻意放鉴权后");

    // 正常:本人信息 + 余额(元)
    let r = client
        .get(format!("{}/api/users/me", app.base))
        .bearer_auth(&user.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let me: Value = r.json().await.unwrap();
    assert_eq!(me["id"], user.id.as_str());
    assert_eq!(me["username"], "e2e");
    assert_eq!(me["role"], "admin");
    assert_eq!(me["balance"], 1.0, "库内 1_000_000 微元 → 1 元");
    assert!(me.get("password_hash").is_none(), "不得回传密码哈希");
    assert!(
        me["created_at"].as_str().unwrap().contains('T'),
        "时间统一 RFC3339:{me}"
    );

    // version:形状 {version},值非空
    let r = client
        .get(format!("{}/api/system/version", app.base))
        .bearer_auth(&user.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert!(!v["version"].as_str().unwrap_or("").is_empty(), "{v}");

    // 用户被软禁用(DELETE)后已签发 token → 401 invalid_token problem+json
    //(装配层 auth_middleware 的 DB 复核挡下;旧直连物理删行的可观测
    // 等价:会话即刻失效)
    let resp = client
        .delete(format!("{}/api/users/{}", app.base, user.id))
        .bearer_auth(app.admin_token().await)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204, "{:?}", resp.text().await);
    let r = client
        .get(format!("{}/api/users/me", app.base))
        .bearer_auth(&user.token)
        .send()
        .await
        .unwrap();
    assert_problem(r, 401, "invalid_token").await;
}

/// api-keys CRUD 全流程 + 权限分叉(跨用户密钥是核心断言,强度沿用
/// e2e.rs::admin_manages_other_users_api_keys 并升级新语义):
/// 创建 201 明文仅一次 → 列表 {items,total} 恒掩码 → PATCH 返回资源 →
/// DELETE 204;admin ?user_id= 跨用户;普通用户 user_id 参数被忽略、
/// 操作他人 key 一律 404;非法 expires_at / 无字段 400。
/// (key 库内只存 sha256 哈希的形态断言由 aikun-store surreal 单测承担,
/// docs/surreal-migration.md §七第 2 层;e2e 锚定掩码/明文一次/属主语义。)
#[tokio::test]
async fn api_key_crud_and_cross_user_permissions() {
    let app = TestApp::spawn().await;
    let user = app.seed_user().await; // u-e2e(admin)+ k-e2e
    let bob = app
        .create_user_with_key("bob", "user", Default::default())
        .await;
    let client = app.client();
    let admin = user.token.clone();

    // 创建:201;完整明文仅此一次;默认值(0 限额、models 空、永不过期)
    let created = client
        .post(format!("{}/api/api-keys", app.base))
        .bearer_auth(&admin)
        .json(&json!({"name": "my-key", "models": ["gpt-4"], "rate_limit_rpm": 60}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201, "{:?}", created.text().await);
    let body: Value = created.json().await.unwrap();
    let plaintext = body["key"].as_str().unwrap().to_string();
    assert!(
        plaintext.starts_with("sk-") && plaintext.len() > 10,
        "创建响应应回完整明文:{body}"
    );
    assert_eq!(body["name"], "my-key");
    assert_eq!(body["models"], json!(["gpt-4"]));
    assert_eq!(body["rate_limit_rpm"], 60);
    assert_eq!(body["quota_daily_tokens"], 0);
    assert!(body["expires_at"].is_null(), "缺省 expires_at = 永不过期");
    assert!(body["created_at"].as_str().unwrap().contains('T'));
    let key_id = body["id"].as_str().unwrap().to_string();

    // 列表:{items,total};key 恒掩码,明文/哈希不出
    let r = client
        .get(format!("{}/api/api-keys", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let page: Value = r.json().await.unwrap();
    let items = page["items"].as_array().unwrap();
    assert_eq!(page["total"], 2, "k-e2e + 新建;{page}");
    assert_eq!(items.len(), 2);
    assert!(
        items
            .iter()
            .all(|k| k["key"].as_str().is_some_and(|v| v.starts_with("sk-****")))
    );
    // 掩码含明文后 4 位(库内只落 key_suffix 的出参口径)
    let mine = items.iter().find(|k| k["id"] == key_id.as_str()).unwrap();
    let suffix = &plaintext[plaintext.len() - 4..];
    assert_eq!(mine["key"].as_str().unwrap(), format!("sk-****{suffix}"));

    // admin ?user_id= 跨用户;bob 的 user_id 参数被忽略(只看自己,不报错)
    let r = client
        .get(format!("{}/api/api-keys?user_id={}", app.base, bob.id))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let page: Value = r.json().await.unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"][0]["id"], bob.api_key_id.as_str());
    assert_eq!(
        page["items"][0]["key"],
        format!("sk-****{}", &bob.api_key[bob.api_key.len() - 4..])
    );
    let r = client
        .get(format!("{}/api/api-keys?user_id={}", app.base, user.id))
        .bearer_auth(&bob.token)
        .send()
        .await
        .unwrap();
    let page: Value = r.json().await.unwrap();
    assert_eq!(page["total"], 1, "普通用户强制只看自己");
    assert_eq!(page["items"][0]["id"], bob.api_key_id.as_str());

    // PATCH:返回更新后的资源(不再是 {message});掩码视角
    let r = client
        .patch(format!("{}/api/api-keys/{}", app.base, key_id))
        .bearer_auth(&admin)
        .json(&json!({"name": "renamed", "expires_at": "2030-01-01T00:00:00Z"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    let updated: Value = r.json().await.unwrap();
    assert_eq!(updated["name"], "renamed");
    assert_eq!(updated["id"], key_id.as_str());
    assert!(
        updated["key"].as_str().unwrap().starts_with("sk-****"),
        "PATCH 回资源仍是掩码"
    );
    assert_eq!(updated["expires_at"], "2030-01-01T00:00:00Z");

    // expires_at 空串 = 清除(回 null);非法值 400 problem+json
    let r = client
        .patch(format!("{}/api/api-keys/{}", app.base, key_id))
        .bearer_auth(&admin)
        .json(&json!({"expires_at": ""}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.json::<Value>().await.unwrap()["expires_at"].is_null());
    let r = client
        .patch(format!("{}/api/api-keys/{}", app.base, key_id))
        .bearer_auth(&admin)
        .json(&json!({"expires_at": "2000-01-01T00:00:00Z"}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "invalid_expires_at").await;
    let r = client
        .post(format!("{}/api/api-keys", app.base))
        .bearer_auth(&admin)
        .json(&json!({"name": "x", "expires_at": "not-a-date"}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "invalid_expires_at").await;

    // 无可更新字段:未知字段-only 的 body 能过 schema(minProperties 数
    // 全部属性),impl 侧 400 no_fields_to_update(旧语义);空对象 {} 由
    // 生成器拦成 422 request_validation_failed(实测行为,见文件头)。
    let r = client
        .patch(format!("{}/api/api-keys/{}", app.base, key_id))
        .bearer_auth(&admin)
        .json(&json!({"unknown_field": 1}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "no_fields_to_update").await;
    let r = client
        .patch(format!("{}/api/api-keys/{}", app.base, key_id))
        .bearer_auth(&admin)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_problem(r, 422, "request_validation_failed").await;

    // 跨用户:bob 对 admin 的 key PATCH/DELETE 一律 404(不暴露存在性)
    let r = client
        .patch(format!("{}/api/api-keys/{}", app.base, key_id))
        .bearer_auth(&bob.token)
        .json(&json!({"is_active": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let r = client
        .delete(format!("{}/api/api-keys/{}", app.base, key_id))
        .bearer_auth(&bob.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // DELETE:204 无 body;admin 删 bob 的 key;重复删 404;列表归零
    let r = client
        .delete(format!("{}/api/api-keys/{}", app.base, bob.api_key_id))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert!(r.text().await.unwrap().is_empty(), "204 无 body");
    let r = client
        .get(format!("{}/api/api-keys?user_id={}", app.base, bob.id))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let page: Value = r.json().await.unwrap();
    assert_eq!(page["total"], 0, "删除后列表归零");
    let r = client
        .delete(format!("{}/api/api-keys/{}", app.base, bob.api_key_id))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

/// 日志列表与统计:{items,total} 信封、LEFT JOIN 用户名/渠道名(删除后
/// null)、过滤参数口径一致、非 admin 强制只看自己、分页;并锚定生成器
/// 对非法 success 的实际 422 短码。数据来自真实 /v1 请求(受控 usage)。
#[tokio::test]
async fn logs_list_and_stats_new_envelope() {
    let app = TestApp::spawn().await;
    let user = app.seed_user().await;
    app.adjust_balance(&user.id, 1.0).await;
    let bob = app
        .create_user_with_key("bob", "user", Default::default())
        .await;
    app.adjust_balance(&bob.id, 1.0).await;
    let mock = common::MockUpstream::start().await;
    app.seed_provider(&ProviderSeed {
        name: "p-one",
        base_url: &mock.base,
        ..ProviderSeed::default()
    })
    .await;
    // claude-3 渠道:先承接一次失败请求,随后删除(LEFT JOIN → null)
    let gone = app
        .seed_provider(&ProviderSeed {
            name: "p-gone",
            base_url: &mock.base,
            models: &["claude-3"],
            ..ProviderSeed::default()
        })
        .await;
    // gpt-4 价格:10 元/1M 输入、0 输出(受控 usage 全在 prompt 侧)
    app.seed_price("gpt-4", 10.0, 0.0, None).await;

    let chat = |key: &str, model: &str| {
        let client = app.client();
        let base = app.base.clone();
        let key = key.to_string();
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

    // admin 3 行(2 成 1 败:3 次尝试全 500 → 3 条失败行)+ bob 1 行
    mock.push_json(200, completion_with_usage(100, 0));
    assert_eq!(chat(&user.api_key, "gpt-4").await.status(), 200);
    mock.push_json(200, completion_with_usage(200, 0));
    assert_eq!(chat(&user.api_key, "gpt-4").await.status(), 200);
    for _ in 0..3 {
        mock.push_json(500, json!({"error": "internal"}));
    }
    // 上游 5xx 原状态透传给客户端(401/403 才掩码为 502)
    assert_eq!(chat(&user.api_key, "claude-3").await.status(), 500);
    // 渠道删除后,对应日志 provider_name 置 null(dangling 链接降级)
    let resp = app
        .client()
        .delete(format!("{}/api/providers/{gone}", app.base))
        .bearer_auth(app.admin_token().await)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    mock.push_json(200, completion_with_usage(5, 0));
    assert_eq!(chat(&bob.api_key, "gpt-4").await.status(), 200);

    // 失败行 fire-and-forget:等全部行落库(user 5 行 = 2 成 + 3 败;bob 1 行)
    app.wait_admin(&format!("/api/logs?user_id={}", user.id), WAIT, |page| {
        page["total"].as_i64() == Some(5)
    })
    .await;

    let client = app.client();
    let admin = user.token.clone();

    // admin 看全部:{items,total};JOIN 用户名/渠道名;删除渠道 → null
    let r = client
        .get(format!("{}/api/logs", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let page: Value = r.json().await.unwrap();
    assert_eq!(page["total"], 6, "{page}");
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 6);
    let claude_rows: Vec<&Value> = items
        .iter()
        .filter(|l| l["model"] == json!("claude-3"))
        .collect();
    assert_eq!(claude_rows.len(), 3, "3 次失败尝试各落一行");
    assert!(claude_rows.iter().all(|l| l["username"] == json!("e2e")));
    assert!(
        claude_rows.iter().all(|l| l["provider_name"].is_null()),
        "渠道已删 → null:{claude_rows:?}"
    );
    assert!(claude_rows.iter().all(|l| l["success"] == json!(false)));
    assert!(claude_rows.iter().all(|l| l["cost"] == json!(0.0)));
    let l1 = items.iter().find(|l| l["total_tokens"] == 100).unwrap();
    assert_eq!(l1["cost"], 0.001, "微元 1_000 → 0.001 元");
    assert_eq!(l1["provider_name"], "p-one");
    assert!(l1["created_at"].as_str().unwrap().contains('T'), "RFC3339");

    // 过滤:model + success;统计口径与列表一致
    let r = client
        .get(format!("{}/api/logs?model=gpt-4&success=1", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let page: Value = r.json().await.unwrap();
    assert_eq!(page["total"], 3, "gpt-4 且成功的 3 条(2 admin + 1 bob)");
    let r = client
        .get(format!("{}/api/logs/stats?model=gpt-4&success=1", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let s: Value = r.json().await.unwrap();
    assert_eq!(s["total_requests"], 3);
    assert_eq!(s["total_tokens"], 305);
    assert_eq!(s["success_rate"], 100.0);
    assert!((s["total_cost"].as_f64().unwrap() - 0.00305).abs() < 1e-12);

    // admin 全量统计:成功率 3/6 = 50.00(两位小数百分数)
    let r = client
        .get(format!("{}/api/logs/stats", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let s: Value = r.json().await.unwrap();
    assert_eq!(s["total_requests"], 6);
    assert_eq!(s["success_rate"], 50.0);
    assert!(s["avg_latency_ms"].as_i64().unwrap_or(-1) >= 0, "{s}");
    assert!((s["total_cost"].as_f64().unwrap() - 0.00305).abs() < 1e-12);

    // admin ?user_id= 只看 bob;bob 强制只看自己(user_id 参数被忽略)
    let r = client
        .get(format!("{}/api/logs?user_id={}", app.base, bob.id))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let page: Value = r.json().await.unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"][0]["total_tokens"], 5);
    let r = client
        .get(format!("{}/api/logs?user_id={}", app.base, user.id))
        .bearer_auth(&bob.token)
        .send()
        .await
        .unwrap();
    let page: Value = r.json().await.unwrap();
    assert_eq!(page["total"], 1, "普通用户强制只看自己");
    assert_eq!(page["items"][0]["total_tokens"], 5);

    // 分页:limit=2 → 两页,DESC 时间序
    let r = client
        .get(format!("{}/api/logs?limit=2", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let p1: Value = r.json().await.unwrap();
    assert_eq!(p1["total"], 6);
    assert_eq!(p1["items"].as_array().unwrap().len(), 2);
    let r = client
        .get(format!("{}/api/logs?limit=2&offset=2", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let p2: Value = r.json().await.unwrap();
    assert_eq!(p2["items"].as_array().unwrap().len(), 2);

    // 生成器实际行为(集成阶段实测锚定):success 非 0/1 的枚举越界由
    // 生成层 schema 校验拦成 422 request_validation_failed(spec 示例
    // invalid_success 不可达;工人批按 400 malformed_parameter 写错,已锚实测)
    let r = client
        .get(format!("{}/api/logs?success=2", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_problem(r, 422, "request_validation_failed").await;
}

/// 兑换码全流程 + 钱包(核心行为锚点,断言强度沿用
/// e2e.rs::redemption_code_full_flow / wallet_returns_own_balance_...):
/// 201 创建语义、三步同事务(核销 + 加余额 + kind='recharge' 流水)、
/// 归一化输入、无效/已用/过期统一 400 invalid_code、5 次失败 429 +
/// Retry-After;钱包 30 天序列/合计/top_models。
#[tokio::test]
async fn redeem_full_flow_and_wallet() {
    let app = TestApp::spawn().await;
    let user = app.seed_user().await; // 余额 0
    let jwt = app.admin_token().await;
    let client = app.client();

    // 渠道 + 价格(稍后产生一条 140 微元的今日消费明细,给钱包供数)
    let mock = common::MockUpstream::start().await;
    app.seed_provider(&ProviderSeed {
        name: "p1",
        base_url: &mock.base,
        ..ProviderSeed::default()
    })
    .await;
    app.seed_price("gpt-4", 10.0, 30.0, None).await;

    // 两张 10 元码 + 一张已过期 5 元码(明文仅生成响应回一次)
    let resp = client
        .post(format!("{}/api/redemption-codes", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"count": 2, "amount": 10.0, "batch": "b1", "note": "seed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{:?}", resp.text().await);
    let codes: Vec<String> = resp.json::<Value>().await.unwrap()["codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    let resp = client
        .post(format!("{}/api/redemption-codes", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"count": 1, "amount": 5.0, "batch": "b1", "expires_at": "2020-01-01"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let expired_code = resp.json::<Value>().await.unwrap()["codes"][0]
        .as_str()
        .unwrap()
        .to_string();

    // 兑换成功:201(旧 200)+ {amount, balance}(元);归一化 = 去全部
    // 空白 + 转大写(整串小写、段内嵌空格、首尾空格,归一化后命中)
    let mut normalized = codes[0].to_lowercase();
    normalized.insert(6, ' ');
    let spaced = format!(" {normalized} ");
    let r = client
        .post(format!("{}/api/users/me/redemptions", app.base))
        .bearer_auth(&user.token)
        .json(&json!({"code": spaced}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        201,
        "归一化兑换失败(输入 {spaced},原文 {}):{:?}",
        codes[0],
        r.text().await
    );
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["amount"], 10.0);
    assert_eq!(body["balance"], 10.0);

    // 核销 + 余额 + 流水三步同事务:管理面回读两侧
    let codes_page: Value = client
        .get(format!("{}/api/redemption-codes?batch=b1", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let used = codes_page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["status"] == "used")
        .expect("核销后应有 used 行");
    assert_eq!(used["used_by"], "e2e");
    let tx_page: Value = client
        .get(format!(
            "{}/api/balance-transactions?user_id={}",
            app.base, user.id
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tx = &tx_page["items"][0];
    assert_eq!(tx["amount"], 10.0, "{tx_page}");
    assert_eq!(tx["balance_after"], 10.0);
    assert_eq!(tx["kind"], "recharge");
    assert_eq!(tx["note"], "兑换码充值");

    // me 与钱包的余额口径一致(元)
    let r = client
        .get(format!("{}/api/users/me", app.base))
        .bearer_auth(&user.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["balance"], 10.0);

    // 今日一条消费明细:预充 140 微元再请求,消费后余额恰好回到 10 元
    //(钱包断言保持精确口径)
    app.adjust_balance(&user.id, 0.00014).await;
    let resp = client
        .post(format!("{}/v1/chat/completions", app.base))
        .bearer_auth(&user.api_key)
        .json(&json!({"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // 钱包:30 天补零序列;今日费用含明细 140 微元;top_models 就位
    let r = client
        .get(format!("{}/api/users/me/wallet", app.base))
        .bearer_auth(&user.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let w: Value = r.json().await.unwrap();
    assert_eq!(w["balance"], 10.0);
    assert!((w["today"]["cost"].as_f64().unwrap() - 0.00014).abs() < 1e-12);
    assert!((w["month"]["cost"].as_f64().unwrap() - 0.00014).abs() < 1e-12);
    assert_eq!(w["today"]["requests"], 1);
    assert_eq!(w["daily"].as_array().unwrap().len(), 30, "缺日补零");
    let last_day = w["daily"].as_array().unwrap().last().unwrap();
    assert!((last_day["cost"].as_f64().unwrap() - 0.00014).abs() < 1e-12);
    assert_eq!(w["top_models"][0]["model"], "gpt-4");

    // 重复兑换同一张 → 400;不存在 → 400;过期 → 400(统一 invalid_code,
    // problem+json,不细分防枚举)
    let r = client
        .post(format!("{}/api/users/me/redemptions", app.base))
        .bearer_auth(&user.token)
        .json(&json!({"code": codes[0]}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "invalid_code").await;
    let r = client
        .post(format!("{}/api/users/me/redemptions", app.base))
        .bearer_auth(&user.token)
        .json(&json!({"code": "AK-XXXX-XXXX-XXXX-9999"}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "invalid_code").await;
    let r = client
        .post(format!("{}/api/users/me/redemptions", app.base))
        .bearer_auth(&user.token)
        .json(&json!({"code": expired_code}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "invalid_code").await;
    // 成功兑换清失败窗口:此前 3 次失败不累计(codes[1] 仍可兑换)
    let r = client
        .post(format!("{}/api/users/me/redemptions", app.base))
        .bearer_auth(&user.token)
        .json(&json!({"code": codes[1]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    assert_eq!(r.json::<Value>().await.unwrap()["balance"], 20.0);

    // 防爆破:同用户 10 分钟 5 次失败 → 第 6 次 429 rate_limited + Retry-After
    for i in 0..5 {
        let r = client
            .post(format!("{}/api/users/me/redemptions", app.base))
            .bearer_auth(&user.token)
            .json(&json!({"code": format!("AK-XXXX-XXXX-XXXX-{:04}", i)}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "第 {i} 次失败应 400");
    }
    let r = client
        .post(format!("{}/api/users/me/redemptions", app.base))
        .bearer_auth(&user.token)
        .json(&json!({"code": "AK-XXXX-XXXX-XXXX-9998"}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 429, "rate_limited").await;
}

/// 在售价目(旧 GET /api/prices → GET /api/models/prices):
/// {items,total} 信封 + 分页、通配展开、未定价三 null、不可用渠道过滤、
/// 匿名 401。(unhealthy 渠道排除的选路口径由 registry 单测覆盖;
/// e2e 以停用渠道锚定「不可用渠道的模型不在售」。)
#[tokio::test]
async fn public_prices_paginated_new_path() {
    let app = TestApp::spawn().await;
    app.seed_user().await;
    let mock = common::MockUpstream::start().await;
    app.seed_provider(&ProviderSeed {
        name: "p1",
        base_url: &mock.base,
        models: &["gpt-4", "gpt-4o", "unpriced-model"],
        ..ProviderSeed::default()
    })
    .await;
    // 停用渠道的独占模型不应出现在售目录
    let sick = app
        .seed_provider(&ProviderSeed {
            name: "p2",
            base_url: &mock.base,
            models: &["sick-model"],
            ..ProviderSeed::default()
        })
        .await;
    let resp = app
        .client()
        .patch(format!("{}/api/providers/{sick}", app.base))
        .bearer_auth(app.admin_token().await)
        .json(&json!({"is_active": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    app.seed_price("gpt-*", 1.0, 2.0, Some(0.5)).await;
    app.seed_price("other-*", 9.0, 9.0, None).await;

    let client = app.client();

    let anon = client
        .get(format!("{}/api/models/prices", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);

    let r = client
        .get(format!("{}/api/models/prices", app.base))
        .bearer_auth(app.admin_token().await)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let page: Value = r.json().await.unwrap();
    let items = page["items"].as_array().unwrap();
    assert_eq!(page["total"], 3, "gpt-4 / gpt-4o / unpriced-model:{page}");
    assert_eq!(items.len(), 3);
    let gpt4 = items.iter().find(|p| p["model"] == "gpt-4").unwrap();
    assert_eq!(gpt4["prompt_price"], 1.0, "通配 gpt-* 展开为具体单价");
    assert_eq!(gpt4["completion_price"], 2.0);
    assert_eq!(gpt4["cached_price"], 0.5);
    let bare = items
        .iter()
        .find(|p| p["model"] == "unpriced-model")
        .unwrap();
    assert!(bare["prompt_price"].is_null(), "未定价三价全 null");
    assert!(bare["completion_price"].is_null());
    assert!(bare["cached_price"].is_null());
    assert!(
        items.iter().all(|p| p["model"] != "sick-model"),
        "停用渠道的模型不在售"
    );
    assert!(
        items
            .iter()
            .all(|p| p["model"] != "gpt-*" && p["model"] != "other-*"),
        "通配原文不出目录"
    );

    // 分页:total 恒为 3,页内按 limit/offset 切片
    let r = client
        .get(format!("{}/api/models/prices?limit=2", app.base))
        .bearer_auth(app.admin_token().await)
        .send()
        .await
        .unwrap();
    let p1: Value = r.json().await.unwrap();
    assert_eq!(p1["total"], 3);
    assert_eq!(p1["items"].as_array().unwrap().len(), 2);
    let r = client
        .get(format!("{}/api/models/prices?limit=2&offset=2", app.base))
        .bearer_auth(app.admin_token().await)
        .send()
        .await
        .unwrap();
    let p2: Value = r.json().await.unwrap();
    assert_eq!(p2["total"], 3);
    assert_eq!(p2["items"].as_array().unwrap().len(), 1);
}
