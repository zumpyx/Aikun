//! MiniMax Code 账号信息：模型、额度、每日签到。
//!
//! 对照 MiniMax-AI/minimax-code：
//! * 签到：`https://agent.minimaxi.com/minimax-cloud/api/v1/signin/{status,claim}`
//! * 额度：`https://www.minimaxi.com/v1/api/openplatform/coding_plan/remains`
//! * 模型：models-dev catalog 描述符，失败时回退官方 catalog

use md5::{Digest, Md5};
use serde_json::Value;

const CN_GATEWAY: &str = "https://agent.minimaxi.com";
const CN_OPEN: &str = "https://www.minimaxi.com";
const SIGNIN_STATUS: &str = "/minimax-cloud/api/v1/signin/status";
const SIGNIN_CLAIM: &str = "/minimax-cloud/api/v1/signin/claim";
const REMAINS: &str = "/v1/api/openplatform/coding_plan/remains";
const MODELS_DESC: &str = "/mavis/api/v1/models-dev/catalog";

pub async fn account_info(client: &reqwest::Client, token: &str) -> Value {
    let user_id = jwt_user_id(token).unwrap_or_default();
    let mut out = serde_json::json!({ "user_id": user_id });

    match signed_get(
        client,
        &format!("{CN_GATEWAY}{SIGNIN_STATUS}"),
        token,
        &user_id,
    )
    .await
    {
        Ok(v) => out["signin"] = v,
        Err(e) => out["signin_error"] = Value::String(e),
    }
    match client
        .get(format!("{CN_OPEN}{REMAINS}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(r) => match r.json::<Value>().await {
            Ok(v) => out["quota"] = v,
            Err(e) => out["quota_error"] = Value::String(e.to_string()),
        },
        Err(e) => out["quota_error"] = Value::String(e.to_string()),
    }
    match fetch_models(client, token).await {
        Ok(m) => out["models"] = Value::Array(m.into_iter().map(Value::String).collect()),
        Err(e) => out["models_error"] = Value::String(e),
    }
    out
}

pub async fn claim_signin(client: &reqwest::Client, token: &str) -> Result<Value, String> {
    let user_id = jwt_user_id(token).ok_or("token 里没有 user id")?;
    signed_post(
        client,
        &format!("{CN_GATEWAY}{SIGNIN_CLAIM}"),
        token,
        &user_id,
        "{}",
    )
    .await
}

pub async fn fetch_models(client: &reqwest::Client, token: &str) -> Result<Vec<String>, String> {
    let desc_url = format!("{CN_GATEWAY}{MODELS_DESC}");
    if let Ok(r) = client
        .get(&desc_url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
    {
        if let Ok(body) = r.json::<Value>().await {
            if let Some(url) = body.get("catalog_url").and_then(|v| v.as_str()) {
                if let Ok(cat) = client.get(url).send().await {
                    if let Ok(catalog) = cat.json::<Value>().await {
                        let models = extract_model_ids(&catalog);
                        if !models.is_empty() {
                            return Ok(models);
                        }
                    }
                }
            }
        }
    }
    Ok(minimax_fallback())
}

fn minimax_fallback() -> Vec<String> {
    aikun_channels::adapters::minimax_code::DEFAULT_MODELS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn extract_model_ids(catalog: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(obj) = catalog.as_object() else {
        return out;
    };
    for (provider, body) in obj {
        if !provider.to_lowercase().contains("minimax") {
            continue;
        }
        if let Some(models) = body.get("models").and_then(|m| m.as_object()) {
            for id in models.keys() {
                if !out.iter().any(|x| x == id) {
                    out.push(id.clone());
                }
            }
        }
    }
    out
}

async fn signed_get(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    user_id: &str,
) -> Result<Value, String> {
    signed(client, "GET", url, token, user_id, None).await
}

async fn signed_post(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    user_id: &str,
    body: &str,
) -> Result<Value, String> {
    signed(client, "POST", url, token, user_id, Some(body)).await
}

async fn signed(
    client: &reqwest::Client,
    method: &str,
    endpoint: &str,
    token: &str,
    user_id: &str,
    body: Option<&str>,
) -> Result<Value, String> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut url = reqwest::Url::parse(endpoint).map_err(|e| e.to_string())?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("device_platform", "web");
        q.append_pair("biz_id", "3");
        q.append_pair("app_id", "3001");
        q.append_pair("version_code", "22201");
        q.append_pair("is_desktop", "1");
        q.append_pair("desktop_version", "0.6.9");
        q.append_pair("unix", &now_ms.to_string());
        q.append_pair("sys_language", "zh");
        q.append_pair("lang", "zh");
        q.append_pair("device_id", "0");
        q.append_pair("os_name", "linux");
        q.append_pair("browser_name", "mcode");
        q.append_pair("user_id", user_id);
        q.append_pair("client", "mcode");
    }
    let path = format!("{}?{}", url.path(), url.query().unwrap_or(""));
    let yy_body = body.unwrap_or("{}");
    let sig_body = body.unwrap_or("");
    let second = now_ms / 1000;
    let yy = md5_hex(&format!(
        "{}_{}{}ooui",
        urlencoding(&path),
        yy_body,
        md5_hex(&now_ms.to_string())
    ));
    let signature = md5_hex(&format!("{second}I*7Cf%WZ#S&%1RlZJ&C2{sig_body}"));
    let mut req = client
        .request(method.parse().unwrap_or(reqwest::Method::GET), url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", "MiniMaxCode")
        .header("Authorization", format!("Bearer {token}"))
        .header("yy", yy)
        .header("x-timestamp", second.to_string())
        .header("x-signature", signature);
    if let Some(b) = body {
        req = req.body(b.to_string());
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json().await.map_err(|e| e.to_string())
}

fn md5_hex(s: &str) -> String {
    let mut h = Md5::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn jwt_user_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = b64_decode(payload)?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    for key in ["user_id", "uid", "sub", "real_user_id"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
        if let Some(n) = v.get(key).and_then(|x| x.as_i64()) {
            return Some(n.to_string());
        }
    }
    None
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut s = s.replace('-', "+").replace('_', "/");
    while s.len() % 4 != 0 {
        s.push('=');
    }
    base64_decode(&s)
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let bytes: Vec<u8> = s.bytes().filter(|b| *b != b'=').collect();
    let mut i = 0;
    while i + 4 <= bytes.len() + (4 - bytes.len() % 4) % 4 {
        if i + 1 >= bytes.len() {
            break;
        }
        let n = |c: u8| T.iter().position(|x| *x == c).unwrap_or(0) as u32;
        let a = n(*bytes.get(i)?);
        let b = n(*bytes.get(i + 1).unwrap_or(&b'A'));
        let c = n(*bytes.get(i + 2).unwrap_or(&b'A'));
        let d = n(*bytes.get(i + 3).unwrap_or(&b'A'));
        let triple = (a << 18) | (b << 12) | (c << 6) | d;
        out.push((triple >> 16) as u8);
        if i + 2 < bytes.len() {
            out.push((triple >> 8) as u8);
        }
        if i + 3 < bytes.len() {
            out.push(triple as u8);
        }
        i += 4;
    }
    Some(out)
}
