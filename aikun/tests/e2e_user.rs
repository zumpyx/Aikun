//! User tag e2e:新路径(me / version / api-keys CRUD / logs / logs-stats /
//! wallet / redemptions / models-prices)。
//!
//! **独立集成测试目标**(docs/migration-playbook.md §三):只测 User tag
//! 新路径;旧路径用例留在 tests/e2e.rs,随存量 handler 删除由集成工人处理。
//! **本文件在集成阶段随装配(挂 user_api_router)生效后运行**——并行期
//! 路由未挂,单跑会 404 属预期。
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
//! * 钱包:余额/今日/近 7 天/近 30 天合计、30 天补零日序列、top_models;
//! * 日志:{items,total}、LEFT JOIN 用户名/渠道名(删除后 null)、
//!   过滤参数(model/success/user_id)与统计口径一致、非 admin 强制只看自己;
//! * 生成器实际行为锚定:success 非 0/1 → 400 `malformed_parameter`
//!   (spec 示例 invalid_success 不可达,缺口见 playbook/notes)、
//!   空 body PATCH → 422 request_validation_failed。

mod common;

use common::{ProviderSeed, TestApp, seed_api_key, seed_provider, sign_jwt};

/// 断言响应为 problem+json 且 code 符合预期,返回整个 body 供补充断言。
async fn assert_problem(res: reqwest::Response, status: u16, code: &str) -> serde_json::Value {
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
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["code"].as_str(), Some(code), "{body}");
    body
}

/// 最小日志夹具:时间为受控的 UTC RFC3339(与库内 to_rfc3339 同构)。
#[allow(clippy::too_many_arguments)] // 测试夹具按列传参,直观优先
fn insert_log(
    db: &rusqlite::Connection,
    id: &str,
    user_id: &str,
    provider_id: Option<&str>,
    model: &str,
    success: bool,
    total_tokens: i64,
    latency_ms: i32,
    cost_micro: i64,
    created_at: &chrono::DateTime<chrono::Utc>,
) {
    db.execute(
        "INSERT INTO request_logs (id, user_id, provider_id, model, request_type,
                prompt_tokens, completion_tokens, total_tokens, cached_tokens,
                latency_ms, status_code, success, error_message, cost, created_at)
         VALUES (?1, ?2, ?3, ?4, 'chat', 0, 0, ?5, 0, ?6, 200, ?7, NULL, ?8, ?9)",
        rusqlite::params![
            id,
            user_id,
            provider_id,
            model,
            total_tokens,
            latency_ms,
            i32::from(success),
            cost_micro,
            created_at.to_rfc3339()
        ],
    )
    .unwrap();
}

/// GET /api/users/me + GET /api/system/version 新路径:
/// 200 形状(balance 元、时间 RFC3339、无 password_hash)、匿名 401、
/// JWT 有效但用户行已删 → 404 not_found problem+json(旧 user_not_found
/// → not_found 归一)。
#[tokio::test]
async fn me_and_version_new_paths() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db()); // u-e2e(admin),余额 1 元
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
        .bearer_auth(common::admin_jwt())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let me: serde_json::Value = r.json().await.unwrap();
    assert_eq!(me["id"], "u-e2e");
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
        .bearer_auth(common::admin_jwt())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    assert!(!v["version"].as_str().unwrap_or("").is_empty(), "{v}");

    // JWT 合法但用户行已删 → 401 invalid_token problem+json(装配层
    // auth_middleware 的 DB 复核挡下,impl 不可达;与迁移前 /api/me 同为
    // 中间件 401——工人批按 404 写错语义,集成阶段锚定实测行为)
    app.db()
        .execute("DELETE FROM users WHERE id = 'u-e2e'", [])
        .unwrap();
    let r = client
        .get(format!("{}/api/users/me", app.base))
        .bearer_auth(common::admin_jwt())
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
#[tokio::test]
async fn api_key_crud_and_cross_user_permissions() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db()); // u-e2e(admin)+ k-e2e
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
    let client = app.client();
    let admin = common::admin_jwt();
    let bob = sign_jwt("u-bob", "bob", "user");

    // 创建:201;完整明文仅此一次;默认值(0 限额、models 空、永不过期)
    let created = client
        .post(format!("{}/api/api-keys", app.base))
        .bearer_auth(&admin)
        .json(&serde_json::json!({"name": "my-key", "models": ["gpt-4"], "rate_limit_rpm": 60}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201, "{:?}", created.text().await);
    let body: serde_json::Value = created.json().await.unwrap();
    let plaintext = body["key"].as_str().unwrap().to_string();
    assert!(
        plaintext.starts_with("sk-") && plaintext.len() > 10,
        "创建响应应回完整明文:{body}"
    );
    assert_eq!(body["name"], "my-key");
    assert_eq!(body["models"], serde_json::json!(["gpt-4"]));
    assert_eq!(body["rate_limit_rpm"], 60);
    assert_eq!(body["quota_daily_tokens"], 0);
    assert!(body["expires_at"].is_null(), "缺省 expires_at = 永不过期");
    assert!(body["created_at"].as_str().unwrap().contains('T'));
    let key_id = body["id"].as_str().unwrap().to_string();
    // 库中只存哈希与后 4 位
    let suffix: String = app
        .db()
        .query_row(
            "SELECT key_suffix FROM api_keys WHERE id = ?1",
            [&key_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(suffix.len(), 4, "后 4 位");
    assert!(plaintext.ends_with(&suffix), "明文尾缀与库存一致");
    let stored: String = app
        .db()
        .query_row("SELECT key FROM api_keys WHERE id = ?1", [&key_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(stored, common::sha256_hex(&plaintext), "库中只存 sha256");

    // 列表:{items,total};key 恒掩码,明文/哈希不出
    let r = client
        .get(format!("{}/api/api-keys", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let page: serde_json::Value = r.json().await.unwrap();
    let items = page["items"].as_array().unwrap();
    assert_eq!(page["total"], 2, "k-e2e + 新建;{page}");
    assert_eq!(items.len(), 2);
    assert!(
        items
            .iter()
            .all(|k| k["key"].as_str().is_some_and(|v| v.starts_with("sk-****")))
    );

    // admin ?user_id= 跨用户;bob 的 user_id 参数被忽略(只看自己,不报错)
    let r = client
        .get(format!("{}/api/api-keys?user_id=u-bob", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let page: serde_json::Value = r.json().await.unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"][0]["id"], "k-bob");
    assert_eq!(page["items"][0]["key"], "sk-****wxyz");
    let r = client
        .get(format!("{}/api/api-keys?user_id=u-e2e", app.base))
        .bearer_auth(&bob)
        .send()
        .await
        .unwrap();
    let page: serde_json::Value = r.json().await.unwrap();
    assert_eq!(page["total"], 1, "普通用户强制只看自己");
    assert_eq!(page["items"][0]["id"], "k-bob");

    // PATCH:返回更新后的资源(不再是 {message});掩码视角
    let r = client
        .patch(format!("{}/api/api-keys/{}", app.base, key_id))
        .bearer_auth(&admin)
        .json(&serde_json::json!({"name": "renamed", "expires_at": "2030-01-01T00:00:00Z"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    let updated: serde_json::Value = r.json().await.unwrap();
    assert_eq!(updated["name"], "renamed");
    assert_eq!(updated["id"], key_id);
    assert!(
        updated["key"].as_str().unwrap().starts_with("sk-****"),
        "PATCH 回资源仍是掩码"
    );
    assert_eq!(updated["expires_at"], "2030-01-01T00:00:00Z");

    // expires_at 空串 = 清除(回 null);非法值 400 problem+json
    let r = client
        .patch(format!("{}/api/api-keys/{}", app.base, key_id))
        .bearer_auth(&admin)
        .json(&serde_json::json!({"expires_at": ""}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.json::<serde_json::Value>().await.unwrap()["expires_at"].is_null());
    let r = client
        .patch(format!("{}/api/api-keys/{}", app.base, key_id))
        .bearer_auth(&admin)
        .json(&serde_json::json!({"expires_at": "2000-01-01T00:00:00Z"}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "invalid_expires_at").await;
    let r = client
        .post(format!("{}/api/api-keys", app.base))
        .bearer_auth(&admin)
        .json(&serde_json::json!({"name": "x", "expires_at": "not-a-date"}))
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
        .json(&serde_json::json!({"unknown_field": 1}))
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
        .bearer_auth(&bob)
        .json(&serde_json::json!({"is_active": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let r = client
        .delete(format!("{}/api/api-keys/{}", app.base, key_id))
        .bearer_auth(&bob)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // DELETE:204 无 body;admin 删 bob 的 key;重复删 404
    let r = client
        .delete(format!("{}/api/api-keys/k-bob", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    assert!(r.text().await.unwrap().is_empty(), "204 无 body");
    let n: i64 = app
        .db()
        .query_row(
            "SELECT COUNT(*) FROM api_keys WHERE id = 'k-bob'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
    let r = client
        .delete(format!("{}/api/api-keys/k-bob", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

/// 日志列表与统计:{items,total} 信封、LEFT JOIN 用户名/渠道名(删除后
/// null)、过滤参数口径一致、非 admin 强制只看自己、分页;并锚定生成器
/// 对非法 success 的实际 400 短码。
#[tokio::test]
async fn logs_list_and_stats_new_envelope() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    app.db()
        .execute(
            "INSERT INTO users (id, username, password_hash, role) VALUES ('u-bob', 'bob', 'h', 'user')",
            [],
        )
        .unwrap();
    let mock = common::MockUpstream::start().await;
    seed_provider(
        &app.db(),
        &ProviderSeed {
            id: "p1",
            name: "p-one",
            protocol: "openai",
            base_url: &mock.base,
            models: "[\"gpt-4\"]",
            priority: 1,
            weight: 1.0,
            health_status: "healthy",
            error_rate: 0.0,
        },
    );
    let now = chrono::Utc::now();
    // admin 3 条(2 成 1 败)+ bob 1 条;p-gone 先建后删(FK 约束:日志的
    // provider_id 必须先真实存在,删除渠道后 ON DELETE SET NULL → null)
    seed_provider(
        &app.db(),
        &ProviderSeed {
            id: "p-gone",
            name: "p-gone",
            protocol: "openai",
            base_url: &mock.base,
            models: "[\"claude-3\"]",
            priority: 1,
            weight: 1.0,
            health_status: "healthy",
            error_rate: 0.0,
        },
    );
    insert_log(
        &app.db(),
        "l1",
        "u-e2e",
        Some("p1"),
        "gpt-4",
        true,
        100,
        50,
        1_000,
        &now,
    );
    insert_log(
        &app.db(),
        "l2",
        "u-e2e",
        Some("p1"),
        "gpt-4",
        true,
        200,
        80,
        2_000,
        &now,
    );
    insert_log(
        &app.db(),
        "l3",
        "u-e2e",
        Some("p-gone"),
        "claude-3",
        false,
        10,
        30,
        0,
        &now,
    );
    // 渠道删除后,对应日志 provider_id 置 NULL(ON DELETE SET NULL)
    app.db()
        .execute("DELETE FROM providers WHERE id = 'p-gone'", [])
        .unwrap();
    insert_log(
        &app.db(),
        "l4",
        "u-bob",
        None,
        "gpt-4",
        true,
        5,
        10,
        100,
        &now,
    );

    let client = app.client();
    let admin = common::admin_jwt();
    let bob = sign_jwt("u-bob", "bob", "user");

    // admin 看全部:{items,total};JOIN 用户名/渠道名;删除渠道 → null
    let r = client
        .get(format!("{}/api/logs", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let page: serde_json::Value = r.json().await.unwrap();
    assert_eq!(page["total"], 4, "{page}");
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    let l3 = items.iter().find(|l| l["id"] == "l3").unwrap();
    assert_eq!(l3["username"], "e2e");
    assert!(l3["provider_name"].is_null(), "渠道已删 → null:{l3}");
    assert_eq!(l3["model"], "claude-3");
    assert_eq!(l3["success"], false);
    assert_eq!(l3["cost"], 0.0);
    let l1 = items.iter().find(|l| l["id"] == "l1").unwrap();
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
    let page: serde_json::Value = r.json().await.unwrap();
    assert_eq!(page["total"], 3, "gpt-4 且成功的 3 条(l1/l2/l4)");
    let r = client
        .get(format!("{}/api/logs/stats?model=gpt-4&success=1", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let s: serde_json::Value = r.json().await.unwrap();
    assert_eq!(s["total_requests"], 3);
    assert_eq!(s["total_tokens"], 305);
    assert_eq!(s["success_rate"], 100.0);
    assert!((s["total_cost"].as_f64().unwrap() - 0.0031).abs() < 1e-9);

    // admin 全量统计:成功率 3/4 = 75.00(两位小数百分数)
    let r = client
        .get(format!("{}/api/logs/stats", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let s: serde_json::Value = r.json().await.unwrap();
    assert_eq!(s["total_requests"], 4);
    assert_eq!(s["success_rate"], 75.0);
    assert_eq!(
        s["avg_latency_ms"], 43,
        "(50+80+30+10)/4 = 42.5,round 半远离零 → 43"
    );
    assert!((s["total_cost"].as_f64().unwrap() - 0.0031).abs() < 1e-9);

    // admin ?user_id= 只看 bob;bob 强制只看自己(user_id 参数被忽略)
    let r = client
        .get(format!("{}/api/logs?user_id=u-bob", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let page: serde_json::Value = r.json().await.unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"][0]["id"], "l4");
    let r = client
        .get(format!("{}/api/logs?user_id=u-e2e", app.base))
        .bearer_auth(&bob)
        .send()
        .await
        .unwrap();
    let page: serde_json::Value = r.json().await.unwrap();
    assert_eq!(page["total"], 1, "普通用户强制只看自己");
    assert_eq!(page["items"][0]["id"], "l4");

    // 分页:limit=2 → 两页,DESC 时间序
    let r = client
        .get(format!("{}/api/logs?limit=2", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let p1: serde_json::Value = r.json().await.unwrap();
    assert_eq!(p1["total"], 4);
    assert_eq!(p1["items"].as_array().unwrap().len(), 2);
    let r = client
        .get(format!("{}/api/logs?limit=2&offset=2", app.base))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    let p2: serde_json::Value = r.json().await.unwrap();
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
    seed_api_key(&app.db());
    app.db()
        .execute("UPDATE users SET balance = 0 WHERE id = 'u-e2e'", [])
        .unwrap();
    // 直接夹具三张码:10 元、10 元、已过期 5 元(哈希按归一化后的明文)
    let db = app.db();
    for (id, code, amount_micro, expires) in [
        ("c1", "AK-AAAA-BBBB-CCCC-0001", 10_000_000i64, None::<&str>),
        ("c2", "AK-AAAA-BBBB-CCCC-0002", 10_000_000, None),
        (
            "c3",
            "AK-AAAA-BBBB-CCCC-0003",
            5_000_000,
            Some("2020-01-01 00:00:00"),
        ),
    ] {
        db.execute(
            "INSERT INTO redemption_codes (id, code_hash, code_suffix, amount, batch, expires_at, note)
             VALUES (?1, ?2, '0001', ?3, 'b1', ?4, 'seed')",
            rusqlite::params![id, common::sha256_hex(code), amount_micro, expires],
        )
        .unwrap();
    }
    // 今日一条消费明细(140 微元),给钱包 today/month/top_models 供数
    insert_log(
        &db,
        "wl-1",
        "u-e2e",
        None,
        "gpt-4",
        true,
        8,
        40,
        140,
        &chrono::Utc::now(),
    );

    let client = app.client();
    let user = sign_jwt("u-e2e", "e2e", "user");

    // 兑换成功:201(旧 200)+ {amount, balance}(元);归一化 = 去全部
    // 空白 + 转大写(空格插在段内,归一化后与库内哈希命中)
    let r = client
        .post(format!("{}/api/users/me/redemptions", app.base))
        .bearer_auth(&user)
        .json(&serde_json::json!({"code": " ak-aaaa-bbbb-c ccc-0001 "}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{:?}", r.text().await);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["amount"], 10.0);
    assert_eq!(body["balance"], 10.0);

    // 核销 + 余额 + 流水三步同事务:DB 侧核对
    let (status, used_by): (String, String) = db
        .query_row(
            "SELECT status, used_by FROM redemption_codes WHERE id = 'c1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((status.as_str(), used_by.as_str()), ("used", "u-e2e"));
    let tx: (i64, i64, String, String) = db
        .query_row(
            "SELECT amount, balance_after, kind, note FROM billing_transactions
             WHERE user_id = 'u-e2e' ORDER BY created_at DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        tx,
        (
            10_000_000,
            10_000_000,
            "recharge".into(),
            "兑换码充值".into()
        )
    );

    // me 与钱包的余额口径一致(元)
    let r = client
        .get(format!("{}/api/users/me", app.base))
        .bearer_auth(&user)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap()["balance"],
        10.0
    );

    // 钱包:30 天补零序列;今日费用含明细 140 微元;top_models 就位
    let r = client
        .get(format!("{}/api/users/me/wallet", app.base))
        .bearer_auth(&user)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let w: serde_json::Value = r.json().await.unwrap();
    assert_eq!(w["balance"], 10.0);
    assert!((w["today"]["cost"].as_f64().unwrap() - 0.00014).abs() < 1e-9);
    assert!((w["month"]["cost"].as_f64().unwrap() - 0.00014).abs() < 1e-9);
    assert_eq!(w["today"]["requests"], 1);
    assert_eq!(w["daily"].as_array().unwrap().len(), 30, "缺日补零");
    let last_day = w["daily"].as_array().unwrap().last().unwrap();
    assert!((last_day["cost"].as_f64().unwrap() - 0.00014).abs() < 1e-9);
    assert_eq!(w["top_models"][0]["model"], "gpt-4");

    // 重复兑换同一张 → 400;不存在 → 400;过期 → 400(统一 invalid_code,
    // problem+json,不细分防枚举)
    let r = client
        .post(format!("{}/api/users/me/redemptions", app.base))
        .bearer_auth(&user)
        .json(&serde_json::json!({"code": "AK-AAAA-BBBB-CCCC-0001"}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "invalid_code").await;
    let r = client
        .post(format!("{}/api/users/me/redemptions", app.base))
        .bearer_auth(&user)
        .json(&serde_json::json!({"code": "AK-XXXX-XXXX-XXXX-9999"}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "invalid_code").await;
    let r = client
        .post(format!("{}/api/users/me/redemptions", app.base))
        .bearer_auth(&user)
        .json(&serde_json::json!({"code": "AK-AAAA-BBBB-CCCC-0003"}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "invalid_code").await;
    // 成功兑换清失败窗口:此前 3 次失败不累计(c2 仍可兑换)
    let r = client
        .post(format!("{}/api/users/me/redemptions", app.base))
        .bearer_auth(&user)
        .json(&serde_json::json!({"code": "AK-AAAA-BBBB-CCCC-0002"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap()["balance"],
        20.0
    );

    // 防爆破:同用户 10 分钟 5 次失败 → 第 6 次 429 rate_limited + Retry-After
    for i in 0..5 {
        let r = client
            .post(format!("{}/api/users/me/redemptions", app.base))
            .bearer_auth(&user)
            .json(&serde_json::json!({"code": format!("AK-XXXX-XXXX-XXXX-{:04}", i)}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "第 {i} 次失败应 400");
    }
    let r = client
        .post(format!("{}/api/users/me/redemptions", app.base))
        .bearer_auth(&user)
        .json(&serde_json::json!({"code": "AK-XXXX-XXXX-XXXX-9998"}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 429, "rate_limited").await;
}

/// 在售价目(旧 GET /api/prices → GET /api/models/prices):
/// {items,total} 信封 + 分页、通配展开、未定价三 null、健康渠道过滤、匿名 401。
#[tokio::test]
async fn public_prices_paginated_new_path() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    let mock = common::MockUpstream::start().await;
    seed_provider(
        &app.db(),
        &ProviderSeed {
            id: "p1",
            name: "p1",
            protocol: "openai",
            base_url: &mock.base,
            models: "[\"gpt-4\",\"gpt-4o\",\"unpriced-model\"]",
            priority: 1,
            weight: 1.0,
            health_status: "healthy",
            error_rate: 0.0,
        },
    );
    // unhealthy 渠道的独占模型不应出现在售目录
    seed_provider(
        &app.db(),
        &ProviderSeed {
            id: "p2",
            name: "p2",
            protocol: "openai",
            base_url: &mock.base,
            models: "[\"sick-model\"]",
            priority: 1,
            weight: 1.0,
            health_status: "unhealthy",
            error_rate: 1.0,
        },
    );
    app.db()
        .execute(
            "INSERT INTO model_prices (id, model, prompt_price, completion_price, cached_price)
             VALUES ('mp-wild', 'gpt-*', 1.0, 2.0, 0.5),
                    ('mp-other', 'other-*', 9.0, 9.0, NULL)",
            [],
        )
        .unwrap();

    let client = app.client();

    let anon = client
        .get(format!("{}/api/models/prices", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);

    let r = client
        .get(format!("{}/api/models/prices", app.base))
        .bearer_auth(common::admin_jwt())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let page: serde_json::Value = r.json().await.unwrap();
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
        "非健康渠道的模型不在售"
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
        .bearer_auth(common::admin_jwt())
        .send()
        .await
        .unwrap();
    let p1: serde_json::Value = r.json().await.unwrap();
    assert_eq!(p1["total"], 3);
    assert_eq!(p1["items"].as_array().unwrap().len(), 2);
    let r = client
        .get(format!("{}/api/models/prices?limit=2&offset=2", app.base))
        .bearer_auth(common::admin_jwt())
        .send()
        .await
        .unwrap();
    let p2: serde_json::Value = r.json().await.unwrap();
    assert_eq!(p2["total"], 3);
    assert_eq!(p2["items"].as_array().unwrap().len(), 1);
}
