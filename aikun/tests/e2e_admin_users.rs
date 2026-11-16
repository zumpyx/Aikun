//! Admin · Users 组 e2e:新路径 users CRUD / 批量创建 / 调账流水。
//!
//! **独立集成测试目标**(并行纪律见 docs/migration-playbook.md §三):
//! 只测新路径;旧 `/api/admin/users*`、`/api/admin/billing/transactions`
//! 用例留在 e2e.rs,由集成工人随存量 handler 删除。本文件覆盖:
//! * users CRUD 生命周期 + `{items,total}` 信封 / 分页 / PATCH 回资源 /
//!   DELETE 204 / problem+json 错误码(api-mapping 全局语义 #3/#4/#6/#7);
//! * 批量创建逐项结果(失败项 `error` → `code`,随机密码仅回显一次);
//! * last_admin 保护 400 → **409**(语义 #8)与双 admin 解锁;
//! * 改密 token_version+1(经已交付的 `/api/auth/login` 验证踢会话);
//! * **调账流水断言**(本批核心):`POST /api/users/{id}/balance-transactions`
//!   201 + kind 判定 + 同事务"改余额+写流水"(201 后 DB 两侧同时可见),
//!   `GET /api/balance-transactions` 信封/过滤/分页/元单位
//!   (impl 在 admin_billing.rs,Billing 批;断言按 spec 契约锚定);
//! * 生成器请求校验实测行为(422 request_validation_failed / 400
//!   malformed_parameter),与 spec 400 示例的差异在用例注释中指出
//!   (playbook 踩坑 #3 同类,已记 notes 待裁决)。

mod common;

use common::{TestApp, seed_api_key};

/// 断言 problem+json 错误体:状态 + content-type + code,返回整个 body。
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

/// 经新路径创建一个用户,返回 (id, username)。
async fn create_user(
    app: &TestApp,
    jwt: &str,
    username: &str,
    password: &str,
    role: &str,
) -> String {
    let resp = app
        .client()
        .post(format!("{}/api/users", app.base))
        .bearer_auth(jwt)
        .json(&serde_json::json!({
            "username": username,
            "password": password,
            "display_name": username,
            "role": role,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{:?}", resp.text().await);
    let body: serde_json::Value = resp.json().await.unwrap();
    body["id"].as_str().unwrap().to_string()
}

// ---------- users CRUD ----------

/// CRUD 生命周期 + 信封/分页/状态码语义变化锚点:
/// 创建 201(无 password_hash、无 balance 字段)→ 重复用户名 409 →
/// impl 级校验 400(全空格用户名/短密码:过 schema、被 impl 拒)→
/// 列表 `{items,total}` + limit clamp → 单查含 balance → PATCH 回
/// **资源**(语义 #6)→ 未知字段 body 400 no_fields_to_update →
/// 空对象 422(生成器 minProperties;spec 描述写 400,差异记 notes)→
/// DELETE **204**(语义 #4)→ 软禁用(行还在,is_active=0)→ 404。
#[tokio::test]
async fn users_crud_lifecycle_and_envelope() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db()); // u-e2e(admin,余额 1 元)
    let jwt = common::admin_jwt();
    let client = app.client();

    // 创建:201,User 资源形状
    let created = client
        .post(format!("{}/api/users", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({
            "username": "alice",
            "password": "password-1",
            "display_name": "Alice",
            "role": "user",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201, "{:?}", created.text().await);
    let body: serde_json::Value = created.json().await.unwrap();
    assert_eq!(body["username"], "alice");
    assert_eq!(body["display_name"], "Alice");
    assert_eq!(body["role"], "user");
    assert_eq!(body["is_active"], true);
    assert!(body["id"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(
        body["created_at"].as_str().unwrap().contains('T'),
        "时间应为 RFC3339:{body}"
    );
    assert!(body.get("password_hash").is_none(), "不得回传密码哈希");
    assert!(body.get("password").is_none(), "不得回传密码");
    assert!(
        body.get("balance").is_none(),
        "创建响应是 User(无 balance),列表/单查才是 UserWithBalance"
    );
    let alice_id = body["id"].as_str().unwrap().to_string();

    // 重复用户名 → 409 problem+json username_exists
    let dup = client
        .post(format!("{}/api/users", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"username": "alice", "password": "password-9"}))
        .send()
        .await
        .unwrap();
    let body = assert_problem(dup, 409, "username_exists").await;
    assert!(body["detail"].is_string(), "{body}");

    // impl 级校验(schema 表达不了的部分):全空格用户名、短密码 → 400
    let blank = client
        .post(format!("{}/api/users", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"username": "   ", "password": "password-1"}))
        .send()
        .await
        .unwrap();
    assert_problem(blank, 400, "invalid_username").await;
    let short = client
        .post(format!("{}/api/users", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"username": "bob", "password": "short"}))
        .send()
        .await
        .unwrap();
    assert_problem(short, 400, "invalid_password").await;

    // role 非枚举值:生成器实测 422(schema enum),spec 400 invalid_role
    // 示例不可达(与 playbook 踩坑 #3 同类,记 notes 待裁决)
    let bad_role = client
        .post(format!("{}/api/users", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"username": "eve", "password": "password-1", "role": "boss"}))
        .send()
        .await
        .unwrap();
    assert_problem(bad_role, 422, "request_validation_failed").await;

    // 列表:{items,total} 信封(语义 #3),项含 balance(元)
    let list = client
        .get(format!("{}/api/users", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200);
    let page: serde_json::Value = list.json().await.unwrap();
    // 全新库会 seed 默认 admin(db/mod.rs:920)+ 种子 u-e2e + 刚建的 alice
    assert_eq!(page["total"], 3, "{page}");
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    let ue2e = items.iter().find(|u| u["username"] == "e2e").unwrap();
    assert_eq!(ue2e["balance"], 1.0, "种子用户 1_000_000 微元应出参 1.0 元");
    let alice = items.iter().find(|u| u["username"] == "alice").unwrap();
    assert_eq!(alice["balance"], 0.0);

    // 分页:limit=1 → 页 1 项但 total 仍全量;limit=0 clamp 到 1
    let p1 = client
        .get(format!("{}/api/users?limit=1", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(p1["items"].as_array().unwrap().len(), 1);
    assert_eq!(p1["total"], 3);
    let p0 = client
        .get(format!("{}/api/users?limit=0", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(
        p0["items"].as_array().unwrap().len(),
        1,
        "limit=0 应 clamp 到 1"
    );
    // offset 为负:生成器 schema(minimum: 0)→ 422;limit 非数字 → 400
    let neg = client
        .get(format!("{}/api/users?offset=-1", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_problem(neg, 422, "request_validation_failed").await;
    let bad_limit = client
        .get(format!("{}/api/users?limit=abc", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_problem(bad_limit, 400, "malformed_parameter").await;

    // 单查:UserWithBalance;未知 id 404 problem+json
    let got = client
        .get(format!("{}/api/users/{}", app.base, alice_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200);
    let got: serde_json::Value = got.json().await.unwrap();
    assert_eq!(got["username"], "alice");
    assert_eq!(got["balance"], 0.0);
    let missing = client
        .get(format!("{}/api/users/no-such-user", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_problem(missing, 404, "not_found").await;

    // PATCH:返回更新后的资源(语义 #6),不再是 {message:'updated'}
    let patched = client
        .patch(format!("{}/api/users/{}", app.base, alice_id))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"display_name": "Alice II"}))
        .send()
        .await
        .unwrap();
    assert_eq!(patched.status(), 200, "{:?}", patched.text().await);
    let patched: serde_json::Value = patched.json().await.unwrap();
    assert_eq!(patched["display_name"], "Alice II");
    assert_eq!(patched["username"], "alice");
    assert_eq!(patched["id"], alice_id.as_str());

    // 空更新:仅未知字段(过 minProperties)→ impl 400 no_fields_to_update;
    // 空对象 {} → 生成器 422(spec 描述写 400,差异记 notes)
    let unknown_only = client
        .patch(format!("{}/api/users/{}", app.base, alice_id))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"unknown_field": 1}))
        .send()
        .await
        .unwrap();
    assert_problem(unknown_only, 400, "no_fields_to_update").await;
    let empty = client
        .patch(format!("{}/api/users/{}", app.base, alice_id))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_problem(empty, 422, "request_validation_failed").await;

    // DELETE:204 无 body(语义 #4);软禁用保留(行还在,is_active=0)
    let deleted = client
        .delete(format!("{}/api/users/{}", app.base, alice_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 204);
    assert!(deleted.text().await.unwrap().is_empty(), "204 应无 body");
    let (n, active): (i64, i64) = app
        .db()
        .query_row(
            "SELECT COUNT(*), is_active FROM users WHERE id = ?1",
            rusqlite::params![alice_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((n, active), (1, 0), "DELETE 是软禁用,不物理删除");
    let after = client
        .get(format!("{}/api/users/{}", app.base, alice_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), 200);
    assert_eq!(
        after.json::<serde_json::Value>().await.unwrap()["is_active"],
        false
    );

    // 未知 id 删除 → 404
    let missing = client
        .delete(format!("{}/api/users/no-such-user", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_problem(missing, 404, "not_found").await;
}

// ---------- 批量创建 ----------

/// 批量创建逐项独立成败:整体 200,created/failed 计数,失败项 `code`
/// (旧 `error`,语义 #7/表),随机密码仅回显一次;空批次 400;超上限
/// 经生成器 422(schema maxItems=100),spec 400 batch_too_large 示例
/// 不可达(记 notes)。
#[tokio::test]
async fn users_batch_create_per_item_results() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    let jwt = common::admin_jwt();
    let client = app.client();

    // 先单个创建 carol,批量里再放一次 → username_exists 逐项失败
    create_user(&app, &jwt, "carol", "password-3", "user").await;

    let resp = client
        .post(format!("{}/api/users/batch", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"users": [
            {"username": "b1"},                                              // 随机密码
            {"username": "b2", "password": "password-2", "display_name": "B2"},
            {"username": "carol", "password": "password-3"},                 // 重复
            {"username": "   "},                                             // trim 后空
            {"username": "b4", "password": "short"},                         // 短密码
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["created"], 2, "{body}");
    assert_eq!(body["failed"], 3, "{body}");
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 5);

    let by_name = |n: &str| results.iter().find(|r| r["username"] == n).unwrap();
    let b1 = by_name("b1");
    assert_eq!(b1["ok"], true, "{b1}");
    assert!(b1["id"].as_str().is_some_and(|s| !s.is_empty()), "{b1}");
    assert_eq!(b1["role"], "user", "缺省角色回显实际生效值");
    assert!(
        b1["password"].as_str().is_some_and(|p| !p.is_empty()),
        "随机生成的密码应在 per-item 结果回显一次:{b1}"
    );
    let b2 = by_name("b2");
    assert_eq!(b2["ok"], true);
    assert!(b2.get("password").is_none(), "自带密码的项不回显密码:{b2}");
    let carol = by_name("carol");
    assert_eq!(carol["ok"], false);
    assert_eq!(
        carol["code"], "username_exists",
        "失败项字段应为 code(旧 error)"
    );
    assert_eq!(by_name("b4")["code"], "invalid_password");
    let blank = results
        .iter()
        .find(|r| r["code"] == "invalid_username")
        .expect("全空格用户名应有 invalid_username 失败项");
    assert_eq!(blank["ok"], false);

    // 库内真实生效:b1/b2 落库,carol 仍只有一条
    let (n_b, n_c): (i64, i64) = app
        .db()
        .query_row(
            "SELECT
               (SELECT COUNT(*) FROM users WHERE username IN ('b1','b2')),
               (SELECT COUNT(*) FROM users WHERE username = 'carol')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((n_b, n_c), (2, 1));

    // 空批次 → 400 empty_batch(schema 无 minItems,可达 impl)
    let empty = client
        .post(format!("{}/api/users/batch", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"users": []}))
        .send()
        .await
        .unwrap();
    assert_problem(empty, 400, "empty_batch").await;

    // 101 项 → 生成器 maxItems=100 → 422(spec 400 batch_too_large 不可达)
    let too_many: Vec<_> = (0..101)
        .map(|i| serde_json::json!({"username": format!("bulk-{i}")}))
        .collect();
    let big = client
        .post(format!("{}/api/users/batch", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"users": too_many}))
        .send()
        .await
        .unwrap();
    assert_problem(big, 422, "request_validation_failed").await;
}

// ---------- last_admin 保护 ----------

/// 唯一活跃 admin 不可被降级/禁用/软删:409 last_admin(problem+json,
/// 语义 #8,旧 400);出现第二个 admin 后保护解除。后续请求用新 admin
/// 的 JWT——auth 中间件会用库内角色覆写 token 声明
/// (src/auth/mod.rs:276),u-e2e 被降级后旧 token 不再过 require_admin。
/// 前置:全新库 seed 了默认 admin(db/mod.rs:920),先软禁用它让 u-e2e
/// 成为唯一活跃 admin(此时有两个活跃 admin,禁用不触发保护)。
#[tokio::test]
async fn last_admin_guard_returns_409() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    let jwt = common::admin_jwt();
    let client = app.client();

    let seeded_admin_id: String = app
        .db()
        .query_row("SELECT id FROM users WHERE username = 'admin'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let disabled = client
        .delete(format!("{}/api/users/{}", app.base, seeded_admin_id))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(disabled.status(), 204, "{:?}", disabled.text().await);

    let demote = client
        .patch(format!("{}/api/users/u-e2e", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"role": "user"}))
        .send()
        .await
        .unwrap();
    let body = assert_problem(demote, 409, "last_admin").await;
    assert!(body["detail"].is_string(), "{body}");

    let disable = client
        .patch(format!("{}/api/users/u-e2e", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"is_active": false}))
        .send()
        .await
        .unwrap();
    assert_problem(disable, 409, "last_admin").await;

    let del = client
        .delete(format!("{}/api/users/u-e2e", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_problem(del, 409, "last_admin").await;

    // 保护生效:三条都被拒,库内状态未变
    let (role, active): (String, i64) = app
        .db()
        .query_row(
            "SELECT role, is_active FROM users WHERE id = 'u-e2e'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((role.as_str(), active), ("admin", 1));

    // 第二个 admin 出现 → 降级 u-e2e 放行,且返回更新后资源
    let root2_id = create_user(&app, &jwt, "root2", "password-2", "admin").await;
    let demote = client
        .patch(format!("{}/api/users/u-e2e", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"role": "user"}))
        .send()
        .await
        .unwrap();
    assert_eq!(demote.status(), 200, "{:?}", demote.text().await);
    let demoted: serde_json::Value = demote.json().await.unwrap();
    assert_eq!(demoted["role"], "user", "PATCH 应返回更新后的资源");

    // 此时 root2 成为唯一活跃 admin,保护转移
    let root2_jwt = common::sign_jwt(&root2_id, "root2", "admin");
    let guard = client
        .patch(format!("{}/api/users/{}", app.base, root2_id))
        .bearer_auth(&root2_jwt)
        .json(&serde_json::json!({"is_active": false}))
        .send()
        .await
        .unwrap();
    assert_problem(guard, 409, "last_admin").await;
}

// ---------- 改密踢会话 ----------

/// 改密后 token_version+1:旧密码登录 401、新密码 200(登录端点已随
/// Auth 批交付,链路全程新路径)。
#[tokio::test]
async fn password_rotation_revokes_login_sessions() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    let jwt = common::admin_jwt();
    let client = app.client();
    create_user(&app, &jwt, "dave", "old-password-1", "user").await;

    let login = |password: &'static str| {
        let client = client.clone();
        let url = format!("{}/api/auth/login", app.base);
        async move {
            client
                .post(url)
                .json(&serde_json::json!({"username": "dave", "password": password}))
                .send()
                .await
                .unwrap()
        }
    };
    assert_eq!(login("old-password-1").await.status(), 200);

    let dave_id: String = app
        .db()
        .query_row("SELECT id FROM users WHERE username = 'dave'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let patched = client
        .patch(format!("{}/api/users/{}", app.base, dave_id))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"password": "new-password-2"}))
        .send()
        .await
        .unwrap();
    assert_eq!(patched.status(), 200, "{:?}", patched.text().await);
    let rotated: serde_json::Value = patched.json().await.unwrap();
    assert_eq!(rotated["username"], "dave", "改密 PATCH 应回更新后资源");

    let tv: i64 = app
        .db()
        .query_row(
            "SELECT token_version FROM users WHERE username = 'dave'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tv, 1, "改密应递增 token_version");

    assert_problem(login("old-password-1").await, 401, "invalid_credentials").await;
    assert_eq!(login("new-password-2").await.status(), 200);
}

// ---------- 调账流水(本批核心断言;impl 在 admin_billing.rs) ----------

/// 调账事务原子性与 201 语义:改余额与写流水同事务落库(201 返回后
/// DB 两侧立即可见、金额一致),正 recharge / 负 adjust,金额出参元;
/// 非法金额 400、未知用户 404。
#[tokio::test]
async fn balance_adjust_records_transaction_atomically() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    let jwt = common::admin_jwt();
    let client = app.client();
    // 从零余额起算(种子默认带 1 元)
    app.db()
        .execute("UPDATE users SET balance = 0 WHERE id = 'u-e2e'", [])
        .unwrap();

    // 充值 100 → 201(旧 200,语义 #5),kind=recharge
    let resp = client
        .post(format!("{}/api/users/u-e2e/balance-transactions", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"amount": 100.0, "note": "首充"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201, "{:?}", resp.text().await);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["balance"], 100.0, "{body}");
    assert_eq!(body["kind"], "recharge");

    // 扣减 30 → 余额 70,kind=adjust
    let resp = client
        .post(format!("{}/api/users/u-e2e/balance-transactions", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"amount": -30.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["balance"], 70.0, "{body}");
    assert_eq!(body["kind"], "adjust");

    // 原子性:余额已改 AND 流水已写,两行金额(微元)与响应一致
    let (balance, rows): (i64, i64) = app
        .db()
        .query_row(
            "SELECT
               (SELECT balance FROM users WHERE id = 'u-e2e'),
               (SELECT COUNT(*) FROM billing_transactions WHERE user_id = 'u-e2e')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(balance, 70_000_000, "库内微元口径");
    assert_eq!(rows, 2, "改余额与写流水必须同事务:不能只有一边");
    let (sum_micro,): (i64,) = app
        .db()
        .query_row(
            "SELECT COALESCE(SUM(amount), 0) FROM billing_transactions WHERE user_id = 'u-e2e'",
            [],
            |r| Ok((r.get(0)?,)),
        )
        .unwrap();
    assert_eq!(sum_micro, balance, "流水累计应等于余额(从零起算)");

    // 流水列表:{items,total} 信封,最新在前,amount/balance_after 出参元
    let resp = client
        .get(format!("{}/api/balance-transactions", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let page: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(page["total"], 2, "{page}");
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["amount"], -30.0);
    assert_eq!(items[0]["balance_after"], 70.0);
    assert_eq!(items[0]["kind"], "adjust");
    assert_eq!(items[0]["user_id"], "u-e2e");
    assert_eq!(items[0]["username"], "e2e", "LEFT JOIN 用户名");
    assert_eq!(items[1]["amount"], 100.0);
    assert_eq!(items[1]["kind"], "recharge");
    assert_eq!(items[1]["note"], "首充");
    assert!(
        items[0]["created_at"].as_str().unwrap().contains('T'),
        "时间应 RFC3339(语义 #9):{items:?}"
    );

    // 非法金额:0 与低于最小记账单位(1 微元)→ 400 invalid_amount
    let zero = client
        .post(format!("{}/api/users/u-e2e/balance-transactions", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"amount": 0}))
        .send()
        .await
        .unwrap();
    assert_problem(zero, 400, "invalid_amount").await;
    let tiny = client
        .post(format!("{}/api/users/u-e2e/balance-transactions", app.base))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"amount": 0.0000001}))
        .send()
        .await
        .unwrap();
    assert_problem(tiny, 400, "invalid_amount").await;

    // 未知用户 → 404
    let missing = client
        .post(format!(
            "{}/api/users/nobody/balance-transactions",
            app.base
        ))
        .bearer_auth(&jwt)
        .json(&serde_json::json!({"amount": 1.0}))
        .send()
        .await
        .unwrap();
    assert_problem(missing, 404, "not_found").await;
}

/// 流水查询的过滤/分页/limit clamp(默认 50、clamp 1..=200,语义 #3;
/// 旧版 limit 固定 200):user_id 过滤、limit/offset 翻页、越界参数由
/// 生成器拦截。
#[tokio::test]
async fn transactions_list_filter_and_pagination() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    let jwt = common::admin_jwt();
    let client = app.client();
    app.db()
        .execute("UPDATE users SET balance = 0 WHERE id = 'u-e2e'", [])
        .unwrap();

    let adjust = |uid: &str, amount: f64| {
        let client = client.clone();
        let jwt = jwt.clone();
        let url = format!("{}/api/users/{}/balance-transactions", app.base, uid);
        async move {
            let resp = client
                .post(url)
                .bearer_auth(&jwt)
                .json(&serde_json::json!({"amount": amount}))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 201, "{:?}", resp.text().await);
        }
    };
    adjust("u-e2e", 100.0).await;
    adjust("u-e2e", 50.0).await;
    let other_id = create_user(&app, &jwt, "zoe", "password-1", "user").await;
    adjust(&other_id, 5.0).await;

    // 全量:total 3;user_id 过滤
    let all = client
        .get(format!("{}/api/balance-transactions", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(all["total"], 3, "{all}");
    assert_eq!(all["items"].as_array().unwrap().len(), 3);

    let mine = client
        .get(format!(
            "{}/api/balance-transactions?user_id=u-e2e",
            app.base
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(mine["total"], 2);
    assert!(
        mine["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["user_id"] == "u-e2e"),
        "{mine}"
    );

    let others = client
        .get(format!(
            "{}/api/balance-transactions?user_id={}",
            app.base, other_id
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(others["total"], 1);
    assert_eq!(others["items"][0]["username"], "zoe");

    // 分页:limit=1 + offset 翻页;total 恒全量
    let page1 = client
        .get(format!("{}/api/balance-transactions?limit=1", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(page1["items"].as_array().unwrap().len(), 1);
    assert_eq!(page1["total"], 3);
    let page2 = client
        .get(format!(
            "{}/api/balance-transactions?limit=1&offset=1",
            app.base
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(page2["items"].as_array().unwrap().len(), 1);
    let id1 = page1["items"][0]["id"].as_str().unwrap();
    let id2 = page2["items"][0]["id"].as_str().unwrap();
    assert_ne!(id1, id2, "offset 应翻到不同流水");

    // limit clamp:0 → 1;201 → 200(有 3 条,应全量返回而非被截到默认 50 以下)
    let clamped = client
        .get(format!("{}/api/balance-transactions?limit=0", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(
        clamped["items"].as_array().unwrap().len(),
        1,
        "limit=0 clamp 到 1"
    );
    let over = client
        .get(format!("{}/api/balance-transactions?limit=201", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(
        over["items"].as_array().unwrap().len(),
        3,
        "limit=201 clamp 到 200"
    );

    // 未知 user_id:空页(LEFT JOIN 语义,空 = 全部是缺省;显式未知 = 空)
    let none = client
        .get(format!(
            "{}/api/balance-transactions?user_id=nobody",
            app.base
        ))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(none["total"], 0);
    assert_eq!(none["items"].as_array().unwrap().len(), 0);
}

// ---------- Admin 边界 ----------

/// Admin 边界由装配层中间件承担(auth_middleware + require_admin,
/// architecture §三.3):匿名 401、user 角色 403。只断言状态码——
/// 中间件错误体形态(过渡期仍是网关扁平体)由集成阶段统一收敛。
#[tokio::test]
async fn admin_users_requires_admin_role() {
    let app = TestApp::spawn().await;
    seed_api_key(&app.db());
    let client = app.client();

    let anon = client
        .get(format!("{}/api/users", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);

    let plain_id = create_user(&app, &common::admin_jwt(), "plain", "password-1", "user").await;
    let plain_jwt = common::sign_jwt(&plain_id, "plain", "user");
    let forbidden = client
        .get(format!("{}/api/users", app.base))
        .bearer_auth(&plain_jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), 403);
}
