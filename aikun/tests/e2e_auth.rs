//! Auth tag e2e:新路径 `POST /api/auth/login` + 前端/静态资产(Auth 试点批)。
//!
//! **独立集成测试目标**:并行批次的用例各写各的 `tests/e2e_<tag>.rs`,
//! 不改 `tests/e2e.rs`(纪律见 docs/migration-playbook.md)。本文件覆盖:
//! * 登录链路核心断言(迁自 e2e.rs 旧用例,断言不减:错误密码 401 /
//!   改密踢会话 / 禁用账户 403),错误体升级为 problem+json 断言;
//! * 登录限流两个维度(用户名 5 次 / IP 30 次,10 分钟窗口)与 Retry-After;
//! * 生成器请求校验的实际行为(422 request_validation_failed / 400
//!   malformed_request——与 spec 示例 400 invalid_body 的差异是已记录的
//!   生成器缺口,见 playbook 踩坑);
//! * 前端壳与内嵌静态资产:CSP/no-cache 头、嵌套 js 路径(生成路由
//!   `/static/{path}` 匹配不到,由装配 fallback 兜底)、白名单外 404 纯文本。
//!
//! 阶段 S 检查通道:夹具用户经管理面 API 创建(id 从创建响应取,
//! docs/surreal-migration.md §七)。

mod common;

use common::TestApp;
use serde_json::{Value, json};

/// POST /api/auth/login 的响应断言为 problem+json 且 code 符合预期,
/// 返回整个 body 供补充断言。
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

/// 登录链路(核心行为锚点,断言强度不低于迁移前基线):
/// 错误密码 401 invalid_credentials(problem+json)→ 正确密码 200 签发
/// token+user(无 password_hash;时间为 RFC3339)→ /api/users/me 可用 →
/// 管理员改密后旧 token 401(踢会话)→ 旧密码 401、新密码可登录 →
/// 禁用账户:密码正确也 403 account_disabled,已签发 token 失效 →
/// 匿名访问受保护接口 401。
#[tokio::test]
async fn login_flow_and_session_revocation() {
    let app = TestApp::spawn().await;
    let client = app.client();
    let admin = app.admin_token().await;

    let created = client
        .post(format!("{}/api/users", app.base))
        .bearer_auth(&admin)
        .json(&json!({"username": "alice", "password": "old-password-1", "role": "user"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201, "{:?}", created.text().await);
    let user_id = created.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let login = |password: &'static str| {
        let client = client.clone();
        let url = format!("{}/api/auth/login", app.base);
        async move {
            client
                .post(url)
                .json(&json!({"username": "alice", "password": password}))
                .send()
                .await
                .unwrap()
        }
    };
    let me = |token: String| {
        let client = client.clone();
        let url = format!("{}/api/users/me", app.base);
        async move { client.get(url).bearer_auth(token).send().await.unwrap() }
    };

    // 错误密码 → 401 problem+json invalid_credentials,不区分用户不存在/密码错误
    let body = assert_problem(login("wrong-password").await, 401, "invalid_credentials").await;
    assert!(body["detail"].is_string(), "{body}");

    // 正确密码 → token + user(无密码哈希;时间统一 RFC3339)
    let ok = login("old-password-1").await;
    assert_eq!(ok.status(), 200);
    let body: Value = ok.json().await.unwrap();
    let old_token = body["token"].as_str().unwrap().to_string();
    assert_eq!(body["user"]["username"], "alice");
    assert_eq!(body["user"]["role"], "user");
    assert!(
        body["user"].get("password_hash").is_none(),
        "不得回传密码哈希"
    );
    assert!(
        body["user"]["created_at"].as_str().unwrap().contains('T'),
        "出参时间应为 RFC3339:{body}"
    );
    let r = me(old_token.clone()).await;
    assert_eq!(r.status(), 200);
    let me_body: Value = r.json().await.unwrap();
    assert_eq!(me_body["username"], "alice");

    // 管理员改密 → token_version + 1,旧 token 立即失效
    let patched = client
        .patch(format!("{}/api/users/{}", app.base, user_id))
        .bearer_auth(&admin)
        .json(&json!({"password": "new-password-2"}))
        .send()
        .await
        .unwrap();
    assert_eq!(patched.status(), 200);
    assert_eq!(me(old_token).await.status(), 401, "改密后旧会话应被踢下线");

    // 旧密码不再可用,新密码可登录且新 token 有效
    assert_eq!(login("old-password-1").await.status(), 401);
    let ok = login("new-password-2").await;
    assert_eq!(ok.status(), 200);
    let new_token = ok.json::<Value>().await.unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(me(new_token.clone()).await.status(), 200);

    // 禁用账户:密码正确也 403 problem+json account_disabled;已签发 token 同样失效
    let disabled = client
        .patch(format!("{}/api/users/{}", app.base, user_id))
        .bearer_auth(&admin)
        .json(&json!({"is_active": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(disabled.status(), 200);
    assert_problem(login("new-password-2").await, 403, "account_disabled").await;
    assert_eq!(me(new_token).await.status(), 401);

    // 未带凭证访问受保护接口 → 401
    let anon = client
        .get(format!("{}/api/users/me", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
}

/// 用户名维度限流:同一用户名 10 分钟窗口内 5 次失败后,第 6 次(哪怕
/// 密码正确、凭证本可通过)在验密前被 429 rate_limited 拒绝,并带
/// Retry-After 头(spec 目标态新增,api-mapping 全局语义 #12)。
#[tokio::test]
async fn login_rate_limited_by_username() {
    let app = TestApp::spawn().await;
    let client = app.client();

    let created = client
        .post(format!("{}/api/users", app.base))
        .bearer_auth(app.admin_token().await)
        .json(&json!({"username": "bob", "password": "right-password", "role": "user"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);

    for _ in 0..5 {
        let r = client
            .post(format!("{}/api/auth/login", app.base))
            .json(&json!({"username": "bob", "password": "wrong"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
    }

    let limited = client
        .post(format!("{}/api/auth/login", app.base))
        .json(&json!({"username": "bob", "password": "right-password"}))
        .send()
        .await
        .unwrap();
    assert_problem(limited, 429, "rate_limited").await;
    let retry_after = client
        .post(format!("{}/api/auth/login", app.base))
        .json(&json!({"username": "bob", "password": "right-password"}))
        .send()
        .await
        .unwrap()
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .expect("429 应带可解析的 Retry-After 秒数");
    assert!((1..=600).contains(&retry_after), "{retry_after}");
}

/// IP 维度限流:换用户名无法绕过——6 个未知用户名 × 5 次失败 = 30 次
/// 401(单用户名均未触限)后,第 31 次(全新用户名)在验密前被 429。
/// IP 维度在装配层中间件(生成 trait 方法签名只有 body,见
/// crates/aikun-admin/src/auth.rs::login_ip_guard)。
#[tokio::test]
async fn login_rate_limited_by_ip() {
    let app = TestApp::spawn().await;
    let client = app.client();

    for i in 0..6 {
        for _ in 0..5 {
            let r = client
                .post(format!("{}/api/auth/login", app.base))
                .json(&json!({
                    "username": format!("ghost-user-{i}"),
                    "password": "wrong"
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 401, "单用户名 5 次内只应 401");
        }
    }

    // 全新用户名:用户名维度未触限,IP 维度(30 次/10 分钟)应拦截
    let limited = client
        .post(format!("{}/api/auth/login", app.base))
        .json(&json!({"username": "fresh-user", "password": "whatever"}))
        .send()
        .await
        .unwrap();
    assert_problem(limited, 429, "rate_limited").await;
}

/// 生成器请求校验的实际行为(e2e 锚定实测,防无意识漂移):
/// * 缺必填字段 → 422 request_validation_failed(注意:spec 示例写的是
///   400 invalid_body,生成器 422 优先——缺口记录在 migration-playbook);
/// * 非法 JSON → 400 malformed_request。
#[tokio::test]
async fn login_rejects_body_missing_fields() {
    let app = TestApp::spawn().await;
    let client = app.client();

    let missing = client
        .post(format!("{}/api/auth/login", app.base))
        .json(&json!({"username": "x"}))
        .send()
        .await
        .unwrap();
    assert_problem(missing, 422, "request_validation_failed").await;

    let malformed = client
        .post(format!("{}/api/auth/login", app.base))
        .header("content-type", "application/json")
        .body("this is not json")
        .send()
        .await
        .unwrap();
    assert_problem(malformed, 400, "malformed_request").await;
}

/// 前端壳与内嵌静态资产(Auth tag 的 serveFrontend/serveStaticAsset):
/// CSP / no-cache 头保持;React 产物(`/static/assets/*` 嵌套路径)走装配
/// fallback;白名单外一律 404 纯文本 'not found'(非 JSON)。
#[tokio::test]
async fn frontend_and_static_assets() {
    let app = TestApp::spawn().await;
    let client = app.client();

    // 前端壳
    let r = client.get(format!("{}/", app.base)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let ct = r.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(ct.starts_with("text/html"), "content-type: {ct}");
    assert!(ct.contains("charset=utf-8"), "content-type: {ct}");
    let csp = r
        .headers()
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(csp.starts_with("default-src 'self'"), "CSP: {csp}");
    assert_eq!(
        r.headers().get("cache-control").unwrap(),
        "no-cache, no-store, must-revalidate"
    );
    assert_eq!(r.headers().get("pragma").unwrap(), "no-cache");
    assert!(r.text().await.unwrap().contains("<!DOCTYPE html>"));

    // 单段资产路由:React 产物全部在 assets/ 嵌套路径下,单段一律 404
    // 纯文本(生成路由 NotFound 变体语义保持)
    let r = client
        .get(format!("{}/static/style.css", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(r.text().await.unwrap(), "not found");

    // 嵌套资产(生成路由匹配不到,装配 fallback 兜底)——React 产物全部
    // 在此:vite base=/static/,构建产物提交仓库,build.rs 扫 dist 嵌入
    let r = client
        .get(format!("{}/static/assets/index.css", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        r.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/css")
    );
    assert!(r.headers().get("content-security-policy").is_some());
    let r = client
        .get(format!("{}/static/assets/app.js", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        r.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/javascript"),
        "js mime"
    );
    assert!(r.headers().get("content-security-policy").is_some());
    let body = r.text().await.unwrap();
    assert!(
        body.len() > 100_000,
        "app bundle 应完整内嵌(实际 {} 字节)",
        body.len()
    );

    // 白名单外:单段与嵌套都是 404 纯文本 'not found'
    let r = client
        .get(format!("{}/static/nope.css", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(r.text().await.unwrap(), "not found");
    let r = client
        .get(format!("{}/static/js/nope.js", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(r.text().await.unwrap(), "not found");

    // 未匹配的非静态路径维持默认空 404(与迁移前一致)
    let r = client
        .get(format!("{}/definitely-not-a-route", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}
