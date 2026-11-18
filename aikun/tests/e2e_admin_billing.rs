//! Admin tag · billing 方法组 e2e:新路径 `/api/prices`(CRUD + 通配计费)、
//! `/api/balance-transactions`(调账 201 + 流水分页)、
//! `/api/redemption-codes`(生成/列表/DELETE 禁用)+ 兑换全流程。
//!
//! **独立集成测试目标**(纪律见 docs/migration-playbook.md):本文件只测
//! 新路径;旧路径用例留在 e2e.rs,由集成工人随旧 handler 删除时同步清理。
//! 用例整体迁自 e2e.rs 的三个锚点用例(admin_prices_crud_and_wildcard_billing
//! / admin_adjust_balance_records_transaction / redemption_code_full_flow
//! / redemption_bruteforce_rate_limited),断言升级到新契约:
//! * `{items,total}` 信封 + limit/offset 分页(默认 50,clamp 1..=200);
//! * 删除 204、调账 201、兑换 201(创建流水语义);
//! * problem+json 的 `code` / `detail`;
//! * 生成器实际校验行为(422 request_validation_failed / 400
//!   malformed_request,与 spec 示例的差异是已记录的生成器口径,锚实测)。
//!
//! 阶段 S 检查通道:seed/断言全走管理面 API(见 tests/common);计费断言
//! 经 /api/logs(stats)与 /api/users 回读,微元口径换算成元后比对。
//!
//! `seed_openai_provider` / `chat_completions` 是 e2e.rs 同名助手的本地
//! 副本(integration test 目标间不共享非 common 代码;通配计费需要经
//! /v1 代理真实记账,/v1 契约冻结不变)。

mod common;

use std::time::Duration;

use common::{MockUpstream, ProviderSeed, TestApp};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(10);

/// problem+json 断言:状态 + content-type + code,返回整个 body 供补充断言。
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

/// 价格 CRUD + 通配计费(核心行为锚点,强度不低于迁移前基线):
/// 创建通配价格 → 409 重复 → /v1 请求按通配计费(11 微元)→
/// {items,total} 信封与 in_use 宽容过滤 → 分页 → 204 删除/404 →
/// PATCH 回资源 + cached_price 三态 → 空 PATCH 400 → 非法通配/价格 400/422。
#[tokio::test]
async fn admin_prices_crud_wildcard_billing_and_pagination() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let user = app.seed_user_funded().await;
    seed_openai_provider(&app, &mock, "p1").await;
    let jwt = app.admin_token().await;
    let client = app.client();

    // 创建通配价格 gpt-*(1, 2)/1M → 201;cached_price 未传,显式 null
    let resp = client
        .post(format!("{}/api/prices", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"model": "gpt-*", "prompt_price": 1.0, "completion_price": 2.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{:?}", resp.text().await);
    let price: Value = resp.json().await.unwrap();
    assert_eq!(price["model"], "gpt-*");
    assert!(
        price["cached_price"].is_null(),
        "未传 cached_price → null:{price}"
    );
    assert!(
        price["created_at"].as_str().unwrap().contains('T'),
        "出参时间应为 RFC3339:{price}"
    );
    let id = price["id"].as_str().unwrap().to_string();

    // 重复创建同一 model → 409 problem+json duplicate_model
    let resp = client
        .post(format!("{}/api/prices", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"model": "gpt-*"}))
        .send()
        .await
        .unwrap();
    assert_problem(resp, 409, "duplicate_model").await;

    // 请求命中通配价格:cost = (3+2)×1/1M + 3×2/1M = 0.000011 元 = 11 微元
    // (mock 5 个输入含 2 缓存,无 cached_price 时缓存按输入价计)
    let resp = chat_completions(&app, &user.api_key).await;
    assert_eq!(resp.status(), 200);
    app.wait_admin(&format!("/api/logs/stats?user_id={}", user.id), WAIT, |s| {
        (s["total_cost"].as_f64().unwrap_or(0.0) - 0.000011).abs() < 1e-12
    })
    .await;

    // 列表:{items,total} 信封(旧裸数组)
    let resp = client
        .get(format!("{}/api/prices", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let page: Value = resp.json().await.unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);

    // in_use=1:只返回渠道模型(gpt-4)会命中的条目;other-* 无命中被滤掉
    let resp = client
        .post(format!("{}/api/prices", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"model": "other-*", "prompt_price": 1.0, "completion_price": 1.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let other_id = resp.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = client
        .get(format!("{}/api/prices?in_use=1", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let in_use: Value = resp.json().await.unwrap();
    let items = in_use["items"].as_array().unwrap();
    assert_eq!(in_use["total"], 1);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["model"], "gpt-*");

    // in_use 非 '1' 值静默忽略、等同缺省(宽容语义,api-mapping 备注 9,
    // 刻意不严格化为 400)
    let resp = client
        .get(format!("{}/api/prices?in_use=0", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let all: Value = resp.json().await.unwrap();
    assert_eq!(all["total"], 2, "in_use=0 等同缺省:{all}");

    // 分页:ORDER BY model(gpt-* < other-*);limit/offset 生效,total 不变
    let resp = client
        .get(format!("{}/api/prices?limit=1", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    let p1: Value = resp.json().await.unwrap();
    assert_eq!(p1["total"], 2);
    assert_eq!(p1["items"].as_array().unwrap().len(), 1);
    assert_eq!(p1["items"][0]["model"], "gpt-*");
    let resp = client
        .get(format!("{}/api/prices?limit=1&offset=1", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    let p2: Value = resp.json().await.unwrap();
    assert_eq!(p2["total"], 2);
    assert_eq!(p2["items"][0]["model"], "other-*");

    // 负 offset 被生成层 schema(minimum: 0)拦成 422 —— 实际行为锚定
    // (spec 参数描述"默认 0",生成器对越界值按 schema 拒绝)
    let resp = client
        .get(format!("{}/api/prices?offset=-1", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_problem(resp, 422, "request_validation_failed").await;

    // 删除 → 204 No Content(旧 200 {message:'deleted'});再删 → 404 not_found
    let resp = client
        .delete(format!("{}/api/prices/{}", app.base, other_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert!(resp.text().await.unwrap().is_empty(), "204 应无 body");
    let resp = client
        .delete(format!("{}/api/prices/{}", app.base, other_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_problem(resp, 404, "not_found").await;

    // PATCH 回更新后的资源;cached_price 三态:数值 → null → 缺省不改动
    let resp = client
        .patch(format!("{}/api/prices/{}", app.base, id))
        .bearer_auth(&jwt)
        .json(&json!({"prompt_price": 5.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["prompt_price"], 5.0);
    let resp = client
        .patch(format!("{}/api/prices/{}", app.base, id))
        .bearer_auth(&jwt)
        .json(&json!({"cached_price": 1.5}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["cached_price"], 1.5);
    let resp = client
        .patch(format!("{}/api/prices/{}", app.base, id))
        .bearer_auth(&jwt)
        .json(&json!({"cached_price": null}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["cached_price"].is_null(),
        "显式 null 应清空缓存价:{body}"
    );
    assert_eq!(body["prompt_price"], 5.0, "缺省字段不影响已有值");

    // 空 PATCH(所有字段缺省)→ 400 no_fields_to_update
    let resp = client
        .patch(format!("{}/api/prices/{}", app.base, id))
        .bearer_auth(&jwt)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_problem(resp, 400, "no_fields_to_update").await;

    // 非法通配(a** 前缀含 *、空 model)→ 400 invalid_model;
    // 负价格被生成层 schema(minimum: 0)拦成 422(旧 400 invalid_price,
    // 生成器口径,锚实测)
    for (body, status, code) in [
        (
            json!({"model": "a**", "prompt_price": 1.0}),
            400,
            "invalid_model",
        ),
        (json!({"model": "  "}), 400, "invalid_model"),
        (
            json!({"model": "ok-model", "prompt_price": -1.0}),
            422,
            "request_validation_failed",
        ),
    ] {
        let resp = client
            .post(format!("{}/api/prices", app.base))
            .bearer_auth(&jwt)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_problem(resp, status, code).await;
    }

    let resp = client
        .delete(format!("{}/api/prices/{}", app.base, id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

/// 调账(创建余额流水)+ 流水查询:200 → 201(创建语义)、同事务改余额
/// 写流水、正 recharge 负 adjust、{items,total} + 分页、user_id 过滤、
/// 非零有限/微元下限 400、用户不存在 404。
#[tokio::test]
async fn admin_adjust_balance_and_transactions() {
    let app = TestApp::spawn().await;
    let user = app.seed_user().await; // 新建用户余额 0,从零起算
    let jwt = app.admin_token().await;
    let client = app.client();

    // 充值 100 → 201(旧 200),body 为 AdjustBalanceResult
    let resp = client
        .post(format!(
            "{}/api/users/{}/balance-transactions",
            app.base, user.id
        ))
        .bearer_auth(&jwt)
        .json(&json!({"amount": 100.0, "note": "首充"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{:?}", resp.text().await);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["balance"], 100.0);
    assert_eq!(body["kind"], "recharge");

    // 扣减 30 → 余额 70,kind=adjust
    let resp = client
        .post(format!(
            "{}/api/users/{}/balance-transactions",
            app.base, user.id
        ))
        .bearer_auth(&jwt)
        .json(&json!({"amount": -30.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["balance"], 70.0);
    assert_eq!(body["kind"], "adjust");

    // 流水:{items,total}(旧裸数组),最新在前,LEFT JOIN 用户名
    let resp = client
        .get(format!("{}/api/balance-transactions", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let page: Value = resp.json().await.unwrap();
    let arr = page["items"].as_array().unwrap();
    assert_eq!(page["total"], 2);
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["amount"], -30.0);
    assert_eq!(arr[0]["balance_after"], 70.0);
    assert_eq!(arr[0]["kind"], "adjust");
    assert_eq!(arr[0]["username"], "e2e");
    assert_eq!(arr[1]["amount"], 100.0);
    assert_eq!(arr[1]["kind"], "recharge");
    assert_eq!(arr[1]["note"], "首充");
    assert!(
        arr[0]["created_at"].as_str().unwrap().contains('T'),
        "出参时间应为 RFC3339"
    );

    // user_id 过滤:命中/未命中;limit 分页(limit 旧固定 200 → 可传)
    for (qs, expect_total, expect_items) in [
        (format!("?user_id={}", user.id), 2, 2),
        ("?user_id=nobody".to_string(), 0, 0),
        ("?limit=1".to_string(), 2, 1),
    ] {
        let resp = client
            .get(format!("{}/api/balance-transactions{qs}", app.base))
            .bearer_auth(&jwt)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let page: Value = resp.json().await.unwrap();
        assert_eq!(page["total"], expect_total, "{qs}");
        assert_eq!(
            page["items"].as_array().unwrap().len(),
            expect_items,
            "{qs}"
        );
    }

    // 非零有限:0 → 400;0.0000001 元(四舍五入为 0 微元)→ 400;
    // 不存在的用户 → 404
    let resp = client
        .post(format!(
            "{}/api/users/{}/balance-transactions",
            app.base, user.id
        ))
        .bearer_auth(&jwt)
        .json(&json!({"amount": 0}))
        .send()
        .await
        .unwrap();
    assert_problem(resp, 400, "invalid_amount").await;
    let resp = client
        .post(format!(
            "{}/api/users/{}/balance-transactions",
            app.base, user.id
        ))
        .bearer_auth(&jwt)
        .json(&json!({"amount": 0.0000001}))
        .send()
        .await
        .unwrap();
    assert_problem(resp, 400, "invalid_amount").await;
    let resp = client
        .post(format!(
            "{}/api/users/nobody/balance-transactions",
            app.base
        ))
        .bearer_auth(&jwt)
        .json(&json!({"amount": 1.0}))
        .send()
        .await
        .unwrap();
    assert_problem(resp, 404, "not_found").await;
}

/// 兑换码全流程(核心行为锚点):生成(明文仅一次)→ 用户兑换(新路径
/// /api/users/me/redemptions,201)→ 重复/不存在/过期统一 400(防枚举)→
/// 流水记 recharge → 管理端掩码列表/expired 计算态/筛选/分页 →
/// DELETE 禁用(204)/409 not_unused → 非法参数(422/400 分流)。
#[tokio::test]
async fn redemption_code_full_flow() {
    let app = TestApp::spawn().await;
    let user = app.seed_user().await; // 余额 0
    let jwt = app.admin_token().await;
    let client = app.client();

    // 批量生成 3 张 10 元码 + 1 张已过期码
    let resp = client
        .post(format!("{}/api/redemption-codes", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"count": 3, "amount": 10.0, "batch": "b1", "note": "活动"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{:?}", resp.text().await);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["amount"], 10.0);
    assert_eq!(body["batch"], "b1");
    assert_eq!(body["count"], 3);
    let codes: Vec<String> = body["codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    assert_eq!(codes.len(), 3);
    // 明文形状:AK-XXXX-XXXX-XXXX-XXXX(22 字符,4 段)
    assert!(
        codes
            .iter()
            .all(|c| { c.len() == 22 && c.starts_with("AK-") && c.matches('-').count() == 4 })
    );
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

    // 用户兑换(新路径,User 批挂载;旧 /api/wallet/redeem)
    let redeem = |code: &str| {
        let client = client.clone();
        let token = user.token.clone();
        let url = format!("{}/api/users/me/redemptions", app.base);
        let code = code.to_string();
        async move {
            client
                .post(url)
                .bearer_auth(token)
                .json(&json!({"code": code}))
                .send()
                .await
                .unwrap()
        }
    };

    // 兑换第一张:余额 0 → 10;201(创建兑换记录语义,旧 200)
    let resp = redeem(&codes[0]).await;
    assert_eq!(resp.status(), 201, "{:?}", resp.text().await);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["amount"], 10.0);
    assert_eq!(body["balance"], 10.0);

    // 重复兑换同一张 → 400;不存在的码 → 400;过期码 → 400
    //(口径一致不细分,防枚举)
    let resp = redeem(&codes[0]).await;
    assert_problem(resp, 400, "invalid_code").await;
    let resp = redeem("AK-AAAA-BBBB-CCCC-DDDD").await;
    assert_problem(resp, 400, "invalid_code").await;
    let resp = redeem(&expired_code).await;
    assert_problem(resp, 400, "invalid_code").await;

    // 核销事务:流水有一条 10 元 recharge,备注"兑换码充值"
    let resp = client
        .get(format!("{}/api/balance-transactions", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    let page: Value = resp.json().await.unwrap();
    let arr = page["items"].as_array().unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(arr[0]["amount"], 10.0);
    assert_eq!(arr[0]["kind"], "recharge");
    assert_eq!(arr[0]["note"], "兑换码充值");

    // 管理端列表:4 张码(3 正常 + 1 过期未用),第一张已使用;
    // 明文/哈希永不出库,只有掩码
    let resp = client
        .get(format!("{}/api/redemption-codes?batch=b1", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let list: Value = resp.json().await.unwrap();
    assert_eq!(list["total"], 4);
    let arr = list["items"].as_array().unwrap();
    assert_eq!(arr.len(), 4);
    assert!(arr.iter().all(|c| {
        let masked = c["code_masked"].as_str().unwrap();
        masked.len() == 22 && masked.starts_with("AK-****-****-****-")
    }));
    assert!(
        arr.iter()
            .all(|c| c.get("code_hash").is_none() && c.get("code").is_none())
    );
    let used = arr.iter().find(|c| c["status"] == "used").unwrap();
    assert_eq!(used["used_by"], "e2e");
    // 过期未用:status 仍为 unused,expired 计算字段为 true
    let expired_unused = arr
        .iter()
        .find(|c| c["status"] == "unused" && !c["expires_at"].is_null())
        .unwrap();
    assert_eq!(expired_unused["expired"], true);
    assert!(arr.iter().filter(|c| c["expired"] == true).count() == 1);

    // expired 筛选:只返回过期未用的那 1 张
    let resp = client
        .get(format!(
            "{}/api/redemption-codes?batch=b1&status=expired",
            app.base
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let filtered: Value = resp.json().await.unwrap();
    assert_eq!(filtered["total"], 1);
    assert_eq!(filtered["items"][0]["expired"], true);

    // 分页:limit=2 offset=2
    let resp = client
        .get(format!(
            "{}/api/redemption-codes?batch=b1&limit=2&offset=2",
            app.base
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let page2: Value = resp.json().await.unwrap();
    assert_eq!(page2["total"], 4);
    assert_eq!(page2["items"].as_array().unwrap().len(), 2);

    // 禁用一张未过期且未使用的 → 204(旧 200 {message});重复禁用 → 409
    let unused_id = arr
        .iter()
        .find(|c| c["status"] == "unused" && c["expires_at"].is_null())
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = client
        .delete(format!("{}/api/redemption-codes/{}", app.base, unused_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let resp = client
        .delete(format!("{}/api/redemption-codes/{}", app.base, unused_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_problem(resp, 409, "not_unused").await;
    // 不存在的码 → 404
    let resp = client
        .delete(format!("{}/api/redemption-codes/no-such-code", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_problem(resp, 404, "not_found").await;

    // 被禁用的是 codes[1] 或 codes[2]:未禁用的另一张仍可兑换
    let resp = redeem(&codes[1]).await;
    let disabled_first = resp.status() == 400;
    if !disabled_first {
        assert_eq!(resp.status(), 201);
        let resp = redeem(&codes[2]).await;
        assert_eq!(resp.status(), 400);
    } else {
        let resp = redeem(&codes[2]).await;
        assert_eq!(resp.status(), 201);
    }

    // 非法参数分流:count 越界被生成层 schema(min/max)拦成 422(旧 400
    // invalid_count 不可达,生成器口径,锚实测);其余仍是 impl 侧 400
    for (body, status, code) in [
        (
            json!({"count": 501, "amount": 1.0, "batch": "x"}),
            422,
            "request_validation_failed",
        ),
        (
            json!({"count": 1, "amount": -1.0, "batch": "x"}),
            400,
            "invalid_amount",
        ),
        (
            json!({"count": 1, "amount": 2_000_000.0, "batch": "x"}),
            400,
            "invalid_amount",
        ),
        (
            json!({"count": 1, "amount": 0.0000001, "batch": "x"}),
            400,
            "invalid_amount",
        ),
        (
            json!({"count": 1, "amount": 1.0, "batch": ""}),
            400,
            "invalid_batch",
        ),
        (
            json!({"count": 1, "amount": 1.0, "batch": "x", "expires_at": "2026-01-01T00:00:00Z"}),
            400,
            "invalid_expires_at",
        ),
    ] {
        let resp = client
            .post(format!("{}/api/redemption-codes", app.base))
            .bearer_auth(&jwt)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_problem(resp, status, code).await;
    }
}

/// 兑换防爆破(口径不变):同一用户 10 分钟内失败 5 次 → 第 6 次 429
/// rate_limited,并带 Retry-After 头(spec 目标态新增,api-mapping
/// 全局语义 #12;用户维度窗口在 redeem impl(User 批)。
#[tokio::test]
async fn redemption_bruteforce_rate_limited() {
    let app = TestApp::spawn().await;
    let user = app.seed_user().await;
    for i in 0..5 {
        let resp = app
            .client()
            .post(format!("{}/api/users/me/redemptions", app.base))
            .bearer_auth(&user.token)
            .json(&json!({"code": format!("AK-XXXX-XXXX-XXXX-{:04}", i)}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }
    let resp = app
        .client()
        .post(format!("{}/api/users/me/redemptions", app.base))
        .bearer_auth(&user.token)
        .json(&json!({"code": "AK-XXXX-XXXX-XXXX-9999"}))
        .send()
        .await
        .unwrap();
    assert_problem(resp, 429, "rate_limited").await;
    // Retry-After:可解析的秒数(窗口 10 分钟,建议值应落在 1..=600)
    // —— 429 变体专用 Headers 结构由生成路由写出;这里锚定它确实出现
    let resp = app
        .client()
        .post(format!("{}/api/users/me/redemptions", app.base))
        .bearer_auth(&user.token)
        .json(&json!({"code": "AK-XXXX-XXXX-XXXX-9998"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .expect("429 应带可解析的 Retry-After 秒数");
    assert!((1..=600).contains(&retry_after), "{retry_after}");
}
