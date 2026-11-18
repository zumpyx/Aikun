//! Admin·Providers 组 e2e:新路径 `/api/providers**`、`/api/model-health`、
//! `/api/channel-kinds`(并行工人·Providers 批,docs/migration-playbook.md §三)。
//!
//! **独立集成测试目标**:不改 `tests/e2e.rs`;本文件只测新路径,断言按
//! openapi.yaml v0.5.0 升级——problem+json `code`、`{items,total}` 信封、
//! PATCH 回资源、DELETE 204、批量统一 200(失败项 `error` → `code`)、
//! 201 语义(tests/model-tests)、SSE(model-tests stream=true)。
//! 旧路径用例留在 e2e.rs,由集成工人随旧 handler 删除。
//!
//! 核心行为锚点(强度不低于迁移前 e2e.rs 基线):
//! * 渠道 key 静态加密落库(enc:v1:,逐 key 密文不同)+ api_key 永不出参;
//! * 同 key 重复创建 409 duplicate_key(单建 / 批量失败组 / 编辑换 key);
//! * 批量建渠道(key↔账号 1:1,自动编号,批内去重);
//! * 跨字段校验(协议↔地址联动)、channel_kind 注册表校验;
//! * 连通性测试不更新 EMA 延迟;model_health 实测落库 + 过滤/分页。
//!
//! 阶段 S 检查通道:密文形态(enc:v1: 前缀、逐 key 独立密文)不再经
//! 数据库直连断言(SQLite 时代做法)——库内形态由 aikun-store 单测承担
//! (crypto.rs round_trip 的同明文异密文、channels_store.rs 的密文往返,
//! docs/surreal-migration.md §七第 2 层);e2e 锚定「api_key 永不出参 +
//! key 行为可用 + 查重/批量语义」。

mod common;

use std::sync::{Arc, Mutex};

use common::{MockUpstream, ProviderSeed, TestApp};
use serde_json::{Value, json};

/// POST /api/auth 系之外的失败响应断言为 problem+json 且 code 符合预期,
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

fn create_body(name: &str, base_url: &str, api_key: &str) -> Value {
    json!({
        "name": name,
        "openai_base_url": base_url,
        "api_key": api_key,
        "models": ["gpt-4"],
        "protocols": ["openai"],
    })
}

/// 渠道 CRUD + 信封 + 脱敏 + PATCH 回资源 + DELETE 204。
#[tokio::test]
async fn providers_crud_envelope_and_sanitization() {
    let app = TestApp::spawn().await;
    let client = app.client();
    let jwt = app.admin_token().await;

    // 创建:201 + api_key 永不出参 + proxy_url 脱敏(内嵌凭证只回显 host)
    let created = client
        .post(format!("{}/api/providers", app.base))
        .bearer_auth(&jwt)
        .json(&json!({
            "name": "crud",
            "openai_base_url": "http://127.0.0.1:1",
            "api_key": "sk-crud-key",
            "models": ["gpt-4", "gpt-4o"],
            "protocols": ["openai"],
            "proxy_url": "socks5://user:secret@10.0.0.9:1080",
            "note": "n1",
            "priority": 5,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201, "{:?}", created.text().await);
    let body: Value = created.json().await.unwrap();
    let id = body["id"].as_str().unwrap().to_string();
    assert!(!id.is_empty());
    assert!(body.get("api_key").is_none(), "api_key 永不出参:{body}");
    assert_eq!(body["proxy_url"], "socks5://10.0.0.9:1080");
    assert_eq!(body["protocols"], json!(["openai"]));
    assert_eq!(body["default_protocol"], "openai");
    assert_eq!(body["health_status"], "unknown");
    assert_eq!(body["priority"], 5);
    assert!(
        body["created_at"].as_str().unwrap().contains('T'),
        "出参时间应为 RFC3339:{body}"
    );
    assert_eq!(
        body["model_mapping"],
        json!({}),
        "model_mapping 缺省回空对象"
    );
    // 密文静态落库的库内形态(enc:v1: 前缀、不含明文)由 aikun-store
    // 单测锚定(channels_store 的密文往返;见文件头)。

    // 列表:{items,total} 信封
    let list = client
        .get(format!("{}/api/providers", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200);
    let list: Value = list.json().await.unwrap();
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(list["items"][0]["id"], json!(id));
    // limit=0 越界静默 clamp 为 1(spec:不报 400)
    let paged = client
        .get(format!("{}/api/providers?limit=0&offset=0", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(paged.status(), 200);
    let paged: Value = paged.json().await.unwrap();
    assert_eq!(paged["total"], 1);
    assert_eq!(paged["items"].as_array().unwrap().len(), 1);

    // 详情
    let detail = client
        .get(format!("{}/api/providers/{id}", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(detail.status(), 200);
    let detail: Value = detail.json().await.unwrap();
    assert!(detail.get("api_key").is_none());
    assert_eq!(detail["proxy_url"], "socks5://10.0.0.9:1080");

    // PATCH:返回更新后的资源(旧版是 {message:'updated'})
    let patched = client
        .patch(format!("{}/api/providers/{id}", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"note": "n2", "priority": 9}))
        .send()
        .await
        .unwrap();
    assert_eq!(patched.status(), 200, "{:?}", patched.text().await);
    let patched: Value = patched.json().await.unwrap();
    assert_eq!(patched["note"], "n2");
    assert_eq!(patched["priority"], 9);
    assert_eq!(patched["id"], json!(id));

    // 最终状态联动校验:改协议为 anthropic 但没有 anthropic 地址 → 400
    let bad = client
        .patch(format!("{}/api/providers/{id}", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"protocols": ["anthropic"]}))
        .send()
        .await
        .unwrap();
    assert_problem(bad, 400, "invalid_base_url").await;

    // 只有未知字段(全部可更新字段为 None)→ 400 no_fields_to_update
    // (schema minProperties=1 拦空对象,这里带一个未知字段穿过生成层)
    let nofields = client
        .patch(format!("{}/api/providers/{id}", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"unknown_field": 1}))
        .send()
        .await
        .unwrap();
    assert_problem(nofields, 400, "no_fields_to_update").await;

    // DELETE:204(旧版 200 {message});删后 404
    let deleted = client
        .delete(format!("{}/api/providers/{id}", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 204);
    let gone = client
        .get(format!("{}/api/providers/{id}", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_problem(gone, 404, "not_found").await;
    let again = client
        .delete(format!("{}/api/providers/{id}", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_problem(again, 404, "not_found").await;
}

/// key 加密与重复检测:单建 409 / 批量失败组(code 字段)/ 编辑换 key 409;
/// 空 key 不参与查重(两个无 key 渠道可并存)。
#[tokio::test]
async fn provider_key_encrypted_and_duplicate_rejected() {
    let app = TestApp::spawn().await;
    let client = app.client();
    let jwt = app.admin_token().await;

    let created = client
        .post(format!("{}/api/providers", app.base))
        .bearer_auth(&jwt)
        .json(&create_body("dup", "http://127.0.0.1:1", "same-key"))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);

    // 同 key 再建(不同名)→ 409 problem+json duplicate_key,detail 指出已用渠道
    let dup = client
        .post(format!("{}/api/providers", app.base))
        .bearer_auth(&jwt)
        .json(&create_body("dup2", "http://127.0.0.1:1", "same-key"))
        .send()
        .await
        .unwrap();
    let body = assert_problem(dup, 409, "duplicate_key").await;
    assert!(body["detail"].as_str().unwrap().contains("dup"), "{body}");

    // 批量:same-key 与库存重复、new-1 批内重复 → 失败组;new-1 创建成功。
    // 状态码统一 200(旧版部分成功 201 / 全败 400)。
    let batch = client
        .post(format!("{}/api/providers/batch", app.base))
        .bearer_auth(&jwt)
        .json(&json!({
            "name": "dedup",
            "openai_base_url": "http://127.0.0.1:1",
            "models": ["gpt-4"],
            "protocols": ["openai"],
            "api_keys": ["same-key", "new-1", "new-1"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(batch.status(), 200, "{:?}", batch.text().await);
    let batch: Value = batch.json().await.unwrap();
    assert_eq!(batch["created"], 1);
    let failed = batch["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 2);
    assert!(
        failed.iter().all(|f| f["code"] == "duplicate_key"),
        "失败项字段应为 code(旧 error):{failed:?}"
    );
    assert_eq!(batch["providers"].as_array().unwrap().len(), 1);

    // 编辑渠道换成已占用的 key → 409(渠道 id 经列表按名定位)
    let list: Value = client
        .get(format!("{}/api/providers?limit=50", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dup_id = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == json!("dup"))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let patched = client
        .patch(format!("{}/api/providers/{dup_id}", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"api_key": "new-1"}))
        .send()
        .await
        .unwrap();
    assert_problem(patched, 409, "duplicate_key").await;

    // 空 key 不参与查重:两个无 key 渠道可并存(空串哈希会互相误判)
    for name in ["keyless-a", "keyless-b"] {
        let r = client
            .post(format!("{}/api/providers", app.base))
            .bearer_auth(&jwt)
            .json(&create_body(name, "http://127.0.0.1:1", ""))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201, "{:?}", r.text().await);
    }
}

/// 批量建渠道:自动编号 -01..-N、协议字段落库;逐 key 独立加密的库内
/// 形态(密文互不相同)由 aikun-store 单测锚定(AES-256-GCM 随机 nonce,
/// crypto.rs round_trip 的同明文异密文断言);请求整体不可用 → 400。
#[tokio::test]
async fn batch_create_providers_names_and_ciphers() {
    let app = TestApp::spawn().await;
    let client = app.client();
    let jwt = app.admin_token().await;

    let resp = client
        .post(format!("{}/api/providers/batch", app.base))
        .bearer_auth(&jwt)
        .json(&json!({
            "name": "batch",
            "openai_base_url": "http://127.0.0.1:1",
            "models": ["gpt-4"],
            "protocols": ["openai"],
            "api_keys": ["k1", "k2", "k3"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["created"], 3);
    assert_eq!(body["failed"].as_array().unwrap().len(), 0);
    let names: Vec<&str> = body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["batch-01", "batch-02", "batch-03"]);

    // 落库核对(管理面回读):协议字段正确、api_key 永不出参
    let list: Value = client
        .get(format!("{}/api/providers?limit=50", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows: Vec<&Value> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["name"].as_str().is_some_and(|n| n.starts_with("batch-")))
        .collect();
    assert_eq!(rows.len(), 3);
    for p in &rows {
        assert_eq!(p["default_protocol"], "openai");
        assert!(p.get("api_key").is_none(), "api_key 永不出参:{p}");
    }

    // api_keys 全为空白(trim 后为空)→ 400 invalid_api_keys
    // (schema minItems 只拦空数组,空白串穿过生成层由 impl 拦)
    let r = client
        .post(format!("{}/api/providers/batch", app.base))
        .bearer_auth(&jwt)
        .json(&json!({
            "name": "x", "openai_base_url": "http://a",
            "models": [], "api_keys": ["   "],
        }))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "invalid_api_keys").await;

    // name 空 → 400 invalid_name
    let r = client
        .post(format!("{}/api/providers/batch", app.base))
        .bearer_auth(&jwt)
        .json(&json!({
            "name": "  ", "openai_base_url": "http://a",
            "models": [], "api_keys": ["k"],
        }))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "invalid_name").await;

    // 跨字段:勾选 openai 但缺地址 → 逐 key 校验失败。批量统一 200 语义
    // (成败全在 body,见文件头;工人批按旧版"全败 400"写错,锚实测修正)
    let r = client
        .post(format!("{}/api/providers/batch", app.base))
        .bearer_auth(&jwt)
        .json(&json!({
            "name": "x", "models": [], "api_keys": ["k"], "protocols": ["openai"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["created"], 0, "{body}");
    assert_eq!(body["failed"][0]["code"], "invalid_base_url", "{body}");
    assert_eq!(body["failed"][0]["status"], 400, "{body}");
}

/// 渠道类型目录 + channel_kind 注册表校验(创建与 PATCH)。
#[tokio::test]
async fn channel_kinds_catalog_and_validation() {
    let app = TestApp::spawn().await;
    let client = app.client();
    let jwt = app.admin_token().await;

    // 目录:{items,total} 信封,无分页参数(目录型);generic 排第一
    let kinds: Value = app.admin_get("/api/channel-kinds").await;
    let list = kinds["items"].as_array().unwrap();
    assert_eq!(kinds["total"].as_i64().unwrap() as usize, list.len());
    assert_eq!(list[0]["kind"], "generic");
    assert!(
        list.iter()
            .any(|k| k["kind"] == "qoder" && k["experimental"] == true),
        "{kinds}"
    );
    assert!(list.iter().any(|k| k["kind"] == "workbuddy"));

    // 未注册类型 → 400 invalid_channel_kind
    let r = client
        .post(format!("{}/api/providers", app.base))
        .bearer_auth(&jwt)
        .json(&json!({
            "name": "ck-bad", "openai_base_url": "http://127.0.0.1:1",
            "api_key": "sk-ck", "models": ["gpt-4"],
            "channel_kind": "no-such-kind",
        }))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "invalid_channel_kind").await;

    // 合法类型 → 201,回显 channel_kind
    let r = client
        .post(format!("{}/api/providers", app.base))
        .bearer_auth(&jwt)
        .json(&json!({
            "name": "ck-ok", "openai_base_url": "http://127.0.0.1:1",
            "api_key": "sk-ck-ok", "models": ["gpt-4"],
            "channel_kind": "workbuddy",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let created: Value = r.json().await.unwrap();
    assert_eq!(created["channel_kind"], "workbuddy");
    let id = created["id"].as_str().unwrap().to_string();

    // PATCH 改类型同样受注册表校验
    let r = client
        .patch(format!("{}/api/providers/{id}", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"channel_kind": "bogus"}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "invalid_channel_kind").await;
    let r = client
        .patch(format!("{}/api/providers/{id}", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"channel_kind": "qoder"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        app.provider(&id).await["channel_kind"],
        json!("qoder"),
        "PATCH 后渠道类型应生效"
    );
}

/// 连通性测试(201 探测结果;不更新 EMA 延迟)+ 模型实测(业务失败也是
/// 201,ok/status/error 在 body)+ model-health 列表(过滤/分页/RFC3339)。
#[tokio::test]
async fn provider_test_and_model_health() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let p1 = app
        .seed_provider(&ProviderSeed {
            name: "probe",
            base_url: &mock.base,
            weight: 1.0,
            api_key: "upstream-key",
            ..ProviderSeed::default()
        })
        .await;
    let client = app.client();
    let jwt = app.admin_token().await;

    // 连通性测试:201 + 探测结果字段;落库 health_status/last_health_check
    let r = client
        .post(format!("{}/api/providers/{p1}/tests", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{:?}", r.text().await);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["provider_id"], p1);
    assert_eq!(body["name"], "probe");
    assert_eq!(body["status"], "healthy"); // 库内 unhealthy 词表收敛为 down
    assert!(body["latency_ms"].as_f64().unwrap() >= 0.0);
    assert!(body["message"].as_str().unwrap().contains("healthy"));
    // 库内回读:health_status/last_health_check 落库;latency_ms 不被 ping 污染
    let p = app.provider(&p1).await;
    assert_eq!(p["health_status"], "healthy", "{p}");
    assert!(p["last_health_check"].is_string(), "探测时间应落库:{p}");
    // 刻意不更新 latency_ms:ping 延迟不代表真实请求延迟,不污染选路 EMA
    assert_eq!(p["latency_ms"], 0.0, "连通性测试不应改写 EMA 延迟");

    // 模型实测(非流式):201;凭证为入库密钥(创建即加密,解密后直通)
    let r = client
        .post(format!("{}/api/providers/{p1}/model-tests", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"model": "gpt-4", "message": "你好"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{:?}", r.text().await);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["protocol"], "openai");
    assert_eq!(body["stream"], false);
    assert!(body["response"].is_string(), "非流式回上游原文:{body}");
    assert!(
        mock.requests()
            .iter()
            .any(|req| req.path == "/v1/chat/completions"
                && req.authorization.as_deref() == Some("Bearer upstream-key")
                && req.body.contains("你好")),
        "探针应带库存凭证打到上游(build_api_url 拼版本段 /v1)"
    );

    // 业务失败也是成功响应:上游 500 → 201 + ok:false + status/error 在 body
    mock.push_json(500, json!({"error": {"message": "boom"}}));
    let r = client
        .post(format!("{}/api/providers/{p1}/model-tests", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"model": "gpt-4"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "业务失败仍是成功响应(200→201 语义)");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["status"], 500);
    assert!(body["error"].as_str().unwrap().contains("boom"), "{body}");

    // model-health 列表:{items,total} + provider_id 过滤 + RFC3339 时间
    //(实测落库异步,轮询列表端点直到行出现——旧 DB COUNT 的通道等价)
    app.wait_admin(
        &format!("/api/model-health?provider_id={p1}"),
        std::time::Duration::from_secs(5),
        |mh| mh["total"].as_i64().unwrap_or(0) >= 1,
    )
    .await;
    let r = client
        .get(format!("{}/api/model-health?provider_id={p1}", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let mh: Value = r.json().await.unwrap();
    let items = mh["items"].as_array().unwrap();
    assert_eq!(mh["total"].as_i64().unwrap() as usize, items.len());
    assert_eq!(items.len(), 1, "每 (渠道,模型) 一行(最新)");
    assert_eq!(items[0]["provider_id"], p1);
    assert_eq!(items[0]["model"], "gpt-4");
    assert_eq!(items[0]["status"], "unhealthy"); // 最近一次实测是 500
    assert!(
        items[0]["checked_at"].as_str().unwrap().contains('T'),
        "checked_at 应为 RFC3339:{items:?}"
    );
    // 过滤到不存在的渠道 → 空页(total 0)
    let r = client
        .get(format!("{}/api/model-health?provider_id=nobody", app.base))
        .bearer_auth(&jwt)
        .send()
        .await
        .unwrap();
    let mh: Value = r.json().await.unwrap();
    assert_eq!(mh["total"], 0);

    // 渠道未声明的协议 → 400 unsupported_protocol
    let r = client
        .post(format!("{}/api/providers/{p1}/model-tests", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"model": "gpt-4", "protocol": "anthropic"}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "unsupported_protocol").await;

    // 不存在的渠道 → 404;model 空白 → 400 missing_model(schema 只拦缺字段)
    let r = client
        .post(format!("{}/api/providers/nobody/model-tests", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"model": "gpt-4"}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 404, "not_found").await;
    let r = client
        .post(format!("{}/api/providers/{p1}/model-tests", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"model": "   "}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "missing_model").await;
}

/// fetch-models 动作端点:编辑态回退(库存密钥解密使用)、502 映射
/// (upstream_error 只回显 host / no_models)、400 细分、404。
#[tokio::test]
async fn fetch_models_action_endpoint() {
    let app = TestApp::spawn().await;
    let client = app.client();
    let jwt = app.admin_token().await;
    let upstream = FetchModelsUpstream::start().await;

    // 正常拉取:{"data":[{"id":...}]} → {models:[...]}
    upstream.push(200, json!({"data": [{"id": "m-1"}, {"id": "m-2"}]}));
    let r = client
        .post(format!("{}/api/providers/fetch-models", app.base))
        .bearer_auth(&jwt)
        .json(&json!({
            "base_url": upstream.base(),
            "api_key": "sk-fetch",
            "protocol": "openai",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["models"], json!(["m-1", "m-2"]));
    assert_eq!(
        upstream.authorizations().last().map(String::as_str),
        Some("Bearer sk-fetch")
    );

    // 编辑态回退:api_key 留空 + provider_id → 用库存密文解密后的 key
    let created = client
        .post(format!("{}/api/providers", app.base))
        .bearer_auth(&jwt)
        .json(&create_body("edit", &upstream.base(), "sk-stored-edit-key"))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    let created: Value = created.json().await.unwrap();
    let pid = created["id"].as_str().unwrap().to_string();
    upstream.push(200, json!({"data": [{"id": "stored-m"}]}));
    let r = client
        .post(format!("{}/api/providers/fetch-models", app.base))
        .bearer_auth(&jwt)
        .json(&json!({
            "base_url": upstream.base(),
            "api_key": "",
            "protocol": "openai",
            "provider_id": pid,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["models"], json!(["stored-m"]));
    assert!(
        upstream
            .authorizations()
            .iter()
            .any(|a| a == "Bearer sk-stored-edit-key"),
        "编辑态应回退到库存(解密后)key"
    );

    // 上游非 200 → 502 upstream_error,detail 含上游状态
    upstream.push(401, json!({"error": {"message": "bad key"}}));
    let r = client
        .post(format!("{}/api/providers/fetch-models", app.base))
        .bearer_auth(&jwt)
        .json(&json!({
            "base_url": upstream.base(),
            "api_key": "sk-fetch",
            "protocol": "openai",
        }))
        .send()
        .await
        .unwrap();
    let body = assert_problem(r, 502, "upstream_error").await;
    assert!(body["detail"].as_str().unwrap().contains("401"), "{body}");

    // 200 但没有模型列表 → 502 no_models
    upstream.push(200, json!({"object": "list"}));
    let r = client
        .post(format!("{}/api/providers/fetch-models", app.base))
        .bearer_auth(&jwt)
        .json(&json!({
            "base_url": upstream.base(),
            "api_key": "sk-fetch",
            "protocol": "openai",
        }))
        .send()
        .await
        .unwrap();
    assert_problem(r, 502, "no_models").await;

    // api_key 为空且无 provider_id → 400 missing_config(免密探测不支持)
    let r = client
        .post(format!("{}/api/providers/fetch-models", app.base))
        .bearer_auth(&jwt)
        .json(&json!({"base_url": upstream.base(), "api_key": ""}))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "missing_config").await;

    // provider_type 旧回退字段给 responses → 400 invalid_protocol
    let r = client
        .post(format!("{}/api/providers/fetch-models", app.base))
        .bearer_auth(&jwt)
        .json(&json!({
            "base_url": upstream.base(),
            "api_key": "sk-fetch",
            "provider_type": "responses",
        }))
        .send()
        .await
        .unwrap();
    assert_problem(r, 400, "invalid_protocol").await;

    // provider_id 不存在 → 404
    let r = client
        .post(format!("{}/api/providers/fetch-models", app.base))
        .bearer_auth(&jwt)
        .json(&json!({
            "base_url": upstream.base(),
            "api_key": "",
            "provider_id": "no-such-provider",
        }))
        .send()
        .await
        .unwrap();
    assert_problem(r, 404, "not_found").await;
}

/// 模型实测流式路径:stream=true → 201 text/event-stream;事件契约见
/// crates/aikun-admin/src/admin_providers.rs::ProviderProbes::probe_model_stream
/// (delta 增量事件 + 末条 result 事件);流结束落 model_health。
#[tokio::test]
async fn model_test_streams_sse() {
    let mock = MockUpstream::start().await;
    let app = TestApp::spawn().await;
    let p1 = app
        .seed_provider(&ProviderSeed {
            name: "sse",
            base_url: &mock.base,
            weight: 1.0,
            ..ProviderSeed::default()
        })
        .await;
    mock.push_sse(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"He\"}}]}\n\n\
         data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"}}]}\n\n\
         data: [DONE]\n\n",
    );
    let client = app.client();

    let r = client
        .post(format!("{}/api/providers/{p1}/model-tests", app.base))
        .bearer_auth(app.admin_token().await)
        .json(&json!({"model": "gpt-4", "message": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{:?}", r.text().await);
    let ct = r
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(ct.starts_with("text/event-stream"), "content-type: {ct}");
    let body = r.text().await.unwrap();
    assert!(
        body.contains("event: delta"),
        "SSE 应含 delta 事件:\n{body}"
    );
    assert!(
        body.contains("He") && body.contains("llo"),
        "增量内容:\n{body}"
    );
    assert!(
        body.contains("event: result"),
        "末条应为 result 事件(ModelTestResult):\n{body}"
    );
    // 解析最后一个 result 事件的 data 负载(帧内 data: 前缀行)
    let after_last_result = body.rsplit("event: result").next().unwrap_or("");
    let result_line = after_last_result
        .lines()
        .find(|l| l.starts_with("data:"))
        .expect("result 事件应带 data 行");
    let result: Value = serde_json::from_str(result_line.trim_start_matches("data:").trim())
        .expect("result 事件 data 应为 JSON");
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["stream"], true);

    // 流结束落 model_health(适配器在流结束时写库;轮询列表端点)
    app.wait_admin(
        &format!("/api/model-health?provider_id={p1}"),
        std::time::Duration::from_secs(5),
        |mh| {
            mh["items"]
                .as_array()
                .and_then(|a| a.first())
                .is_some_and(|m| m["status"] == json!("healthy"))
        },
    )
    .await;
}

/// 管理面守护:匿名 401(problem+json)、非 admin 403 forbidden。
/// (auth_middleware + require_admin 在装配层;码词表按 spec 401/403。)
#[tokio::test]
async fn providers_require_admin() {
    let app = TestApp::spawn().await;
    let client = app.client();

    let anon = client
        .get(format!("{}/api/providers", app.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
    let ct = anon
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        ct.starts_with("application/problem+json"),
        "content-type: {ct}"
    );
    let body: Value = anon.json().await.unwrap();
    assert!(
        body["code"] == "invalid_token" || body["code"] == "unauthorized",
        "401 code 词表:{body}"
    );

    // 非 admin 403:auth 中间件用库内角色覆写 token 声明(auth/mod.rs 的
    // authenticate_jwt),须用真实 user 行(管理面创建 + 登录)。
    app.create_user("plain", "plain-password-1", "user").await;
    let plain_token = app.login("plain", "plain-password-1").await;
    let forbidden = client
        .get(format!("{}/api/providers", app.base))
        .bearer_auth(plain_token)
        .send()
        .await
        .unwrap();
    assert_problem(forbidden, 403, "forbidden").await;
}

// ---------- fetch-models 专用可控上游 ----------

/// `common::MockUpstream` 把所有 `/models` 请求当健康探测回默认 200,
/// 无法脚本化;fetch-models 的用例(就是对 /models 的拉取)用这个
/// 逐条脚本的小上游:按队列回预设 (status, body),并记录 Authorization。
struct FetchModelsUpstream {
    base: String,
    state: Arc<Mutex<FetchModelsState>>,
}

struct FetchModelsState {
    script: std::collections::VecDeque<(u16, Value)>,
    authorizations: Vec<String>,
}

impl FetchModelsUpstream {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(FetchModelsState {
            script: std::collections::VecDeque::new(),
            authorizations: Vec::new(),
        }));
        let router_state = Arc::clone(&state);
        let app = axum::Router::new()
            .route(
                // 网关侧 models_url 经 build_api_url 拼出 {base}/v1/models
                // (OpenAI 约定;渠道适配器决定,与对话/健康检查同口径)
                "/v1/models",
                axum::routing::get(
                    |axum::extract::State(st): axum::extract::State<
                        Arc<Mutex<FetchModelsState>>,
                    >,
                     headers: axum::http::HeaderMap| async move {
                        let auth = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        let (status, body) = {
                            let mut st = st.lock().unwrap();
                            if let Some(auth) = auth {
                                st.authorizations.push(auth);
                            }
                            st.script
                                .pop_front()
                                .unwrap_or((200, json!({"data": [{"id": "default"}]})))
                        };
                        (
                            axum::http::StatusCode::from_u16(status).unwrap(),
                            axum::Json(body),
                        )
                    },
                ),
            )
            .with_state(router_state);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base: format!("http://127.0.0.1:{port}"),
            state,
        }
    }

    fn base(&self) -> String {
        self.base.clone()
    }

    fn push(&self, status: u16, body: Value) {
        self.state.lock().unwrap().script.push_back((status, body));
    }

    fn authorizations(&self) -> Vec<String> {
        self.state.lock().unwrap().authorizations.clone()
    }
}
