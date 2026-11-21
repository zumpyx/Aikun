//! BigModel Coding Plan PAT(项目访问令牌)签发——官方链路炼化自
//! zai-org/ZCode 开源源码(project-access-token-client.ts,2026-11-21):
//!
//! 1. `GET https://bigmodel.cn/api/biz/customer/getCustomerInfo`
//!    (**裸业务 token**,官方注释明确「customerInfo 要求裸业务 token;
//!    Bearer 格式拒绝会被误判为登录失效」)→ organizations/projects,
//!    个人取「默认组织/默认项目」,否则第一个。
//! 2. `GET /api/biz/v1/organization/{org}/projects/{proj}/api_keys`
//!    (个人域:裸 token)→ 找 name=zcode-api-key 的既有 Key,
//!    没有 → POST 创建(name=zcode-api-key,usageScene=CODING_PLAN)。
//! 3. `POST .../api_keys/{apiKeyId}/access_tokens`(Bearer 业务 token)
//!    body `{clientType:"zcode", clientVersion:3.18.2}` → 短期 PAT。
//!
//! PAT 是 **X-Coding-Plan-Api-Key 的官方真值**(off-peak/订阅 chat 的
//! 模型请求鉴权头);业务 token 过期时官方也无 refresh 接口,须重新
//! 在线登录。本模块带进程内缓存(PAT 短寿命,过期由调用方刷新)。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

const BM_ORIGIN: &str = "https://bigmodel.cn";
const CLIENT_VERSION: &str = "3.18.2";
const KEY_NAME: &str = "zcode-api-key";

struct Cached {
    pat: String,
    /// JWT 指纹(业务 token 换新即失效)。
    jwt_fp: u64,
    at: Instant,
}

static CACHE: LazyLock<Mutex<HashMap<u64, Cached>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// PAT 缓存 TTL(官方「短期令牌」;保守 30 分钟,过期重签——签发本身幂等)。
const PAT_TTL: Duration = Duration::from_secs(30 * 60);

fn fp(jwt: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    jwt.hash(&mut h);
    h.finish()
}

async fn bm_json(
    method: reqwest::Method,
    url: &str,
    auth_value: &str,
    body: Option<&Value>,
) -> Result<(u16, Value), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;
    let mut req = client
        .request(method, url)
        .header("authorization", auth_value);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("请求失败: {}", e.without_url()))?;
    let status = resp.status().as_u16();
    let parsed: Value = resp.json().await.unwrap_or(Value::Null);
    Ok((status, parsed))
}

/// 取/签发 PAT(全链;失败 Err 带可读原因)。
pub async fn ensure_pat(bm_token: &str) -> Result<String, String> {
    let key = fp(bm_token);
    if let Some(c) = CACHE.lock().unwrap().get(&key)
        && c.jwt_fp == key
        && c.at.elapsed() < PAT_TTL
    {
        return Ok(c.pat.clone());
    }

    // 1) 组织/项目定位(裸 token;Bearer 会被误判)
    let (status, body) = bm_json(
        reqwest::Method::GET,
        &format!("{BM_ORIGIN}/api/biz/customer/getCustomerInfo"),
        bm_token,
        None,
    )
    .await?;
    if body["code"].as_i64() == Some(401) {
        return Err(
            "业务 token 已过期(BigModel 无 refresh 接口)——请重新「在线登录」刷新凭证".to_string(),
        );
    }
    // bigmodel.cn 业务域信封:成功码 = 200(非 zcode.z.ai 的 0)
    if status != 200 || body["code"].as_i64().unwrap_or(-1) != 200 {
        return Err(format!(
            "customerInfo 失败(code {} {})",
            body["code"].as_i64().unwrap_or(-1),
            body["msg"].as_str().unwrap_or_default()
        ));
    }
    let orgs = body["data"]["organizations"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    // 个人:优先「默认」组织/项目,否则第一个
    let (org_id, proj_id) = orgs
        .iter()
        .find(|o| {
            o["organizationName"]
                .as_str()
                .is_some_and(|n| n.contains("默认"))
        })
        .or_else(|| orgs.first())
        .and_then(|o| {
            let ps = o["projects"].as_array().cloned().unwrap_or_default();
            let p = ps
                .iter()
                .find(|p| {
                    p["projectName"]
                        .as_str()
                        .is_some_and(|n| n.contains("默认"))
                })
                .or_else(|| ps.first())?;
            Some((
                o["organizationId"].as_str()?.to_string(),
                p["projectId"].as_str()?.to_string(),
            ))
        })
        .ok_or("customerInfo 无可用组织/项目")?;

    // 2) API Key 列表(裸 token;个人域保留旧格式)
    let key_url = format!(
        "{BM_ORIGIN}/api/biz/v1/organization/{}/projects/{}/api_keys",
        org_id, proj_id
    );
    let (status, body) = bm_json(reqwest::Method::GET, &key_url, bm_token, None).await?;
    let mut api_key_id = String::new();
    if status == 200 {
        // 实测形状:{code:200, data:{total, data:[{name, apiKey, ...}]}}
        let entries = body["data"]["data"]
            .as_array()
            .or_else(|| body["data"].as_array())
            .cloned()
            .unwrap_or_default();
        for k in &entries {
            if k["name"].as_str() == Some(KEY_NAME) {
                api_key_id = k["apiKey"].as_str().unwrap_or_default().to_string();
                break;
            }
        }
    }
    if api_key_id.is_empty() {
        let (status, body) = bm_json(
            reqwest::Method::POST,
            &key_url,
            bm_token,
            Some(&serde_json::json!({
                "name": KEY_NAME,
                "usageScene": "CODING_PLAN",
            })),
        )
        .await?;
        api_key_id = body["data"]["data"]["apiKey"]
            .as_str()
            .or_else(|| body["data"]["apiKey"].as_str())
            .or_else(|| body["data"]["data"]["id"].as_str())
            .unwrap_or_default()
            .to_string();
        if status != 200 || api_key_id.is_empty() {
            return Err(format!(
                "创建 zcode-api-key 失败(HTTP {status} {})",
                body["msg"].as_str().unwrap_or_default()
            ));
        }
    }

    // 3) 签发 PAT(Bearer 业务 token)
    let (status, body) = bm_json(
        reqwest::Method::POST,
        &format!("{key_url}/{api_key_id}/access_tokens"),
        &format!("Bearer {bm_token}"),
        Some(&serde_json::json!({
            "clientType": "zcode",
            "clientVersion": CLIENT_VERSION,
        })),
    )
    .await?;
    // 实测形状:{code:200, data:{accessToken, tokenType:"Bearer", expiresIn}}
    let pat = body["data"]["accessToken"]
        .as_str()
        .or_else(|| body["data"]["token"].as_str())
        .unwrap_or_default()
        .to_string();
    if (status != 200 || body["code"].as_i64() != Some(200)) || pat.is_empty() {
        return Err(format!(
            "PAT 签发失败(HTTP {status} {})",
            body["msg"].as_str().unwrap_or_default()
        ));
    }
    CACHE.lock().unwrap().insert(
        key,
        Cached {
            pat: pat.clone(),
            jwt_fp: key,
            at: Instant::now(),
        },
    );
    Ok(pat)
}
