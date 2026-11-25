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
    let Ok(r) = client
        .get(&desc_url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
    else {
        return Ok(minimax_fallback());
    };
    let Ok(body) = r.json::<Value>().await else {
        return Ok(minimax_fallback());
    };
    let Some(url) = body.get("catalog_url").and_then(|v| v.as_str()) else {
        return Ok(minimax_fallback());
    };
    let Ok(cat) = client.get(url).send().await else {
        return Ok(minimax_fallback());
    };
    let Ok(catalog) = cat.json::<Value>().await else {
        return Ok(minimax_fallback());
    };
    let models = extract_model_ids(&catalog);
    if models.is_empty() {
        Ok(minimax_fallback())
    } else {
        Ok(models)
    }
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
        if let Some(s) = v.get(key).and_then(|x| x.as_str())
            && !s.is_empty()
        {
            return Some(s.to_string());
        }
        if let Some(n) = v.get(key).and_then(|x| x.as_i64()) {
            return Some(n.to_string());
        }
    }
    None
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut s = s.replace('-', "+").replace('_', "/");
    while !s.len().is_multiple_of(4) {
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

/// 自动签到设置。库里有覆盖就用覆盖，否则用启动配置。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CheckinSchedule {
    pub enabled: bool,
    pub hour: u32,
    pub minute: u32,
    /// 到点后的随机抖动秒数。整批先抖一次，每个账号再抖一次。
    pub jitter_secs: u64,
}

impl CheckinSchedule {
    pub fn from_config(c: &crate::config::AppConfig) -> Self {
        Self {
            enabled: c.minimax_auto_checkin,
            hour: c.minimax_checkin_hour.min(23),
            minute: c.minimax_checkin_minute.min(59),
            jitter_secs: c.minimax_checkin_jitter_secs,
        }
    }
}

const SETTING_KEY: &str = "minimax_checkin";

pub async fn load_schedule(
    store: &crate::store::AppStore,
    fallback: &crate::config::AppConfig,
) -> CheckinSchedule {
    let mut res = match store
        .handle
        .db()
        .query("SELECT * FROM type::record('setting', $k);")
        .bind(("k", SETTING_KEY))
        .await
    {
        Ok(r) => r,
        Err(_) => return CheckinSchedule::from_config(fallback),
    };
    let rows: Vec<Value> = res.take(0).unwrap_or_default();
    rows.first()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_else(|| CheckinSchedule::from_config(fallback))
}

pub async fn save_schedule(
    store: &crate::store::AppStore,
    s: &CheckinSchedule,
) -> Result<(), String> {
    store
        .handle
        .db()
        .query(
            "UPSERT type::record('setting', $k) SET enabled = $enabled, hour = $hour, minute = $minute, jitter_secs = $jitter;",
        )
        .bind(("k", SETTING_KEY))
        .bind(("enabled", s.enabled))
        .bind(("hour", s.hour))
        .bind(("minute", s.minute))
        .bind(("jitter", s.jitter_secs))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 每天北京时间 hour:minute 后，随机抖动再签到全部活跃 MiniMax 账号。
pub fn spawn_auto_checkin(
    store: std::sync::Arc<crate::store::AppStore>,
    config: crate::config::AppConfig,
) {
    tokio::spawn(async move {
        loop {
            let sched = load_schedule(&store, &config).await;
            if !sched.enabled {
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                continue;
            }
            let now = chrono::Utc::now().timestamp();
            let wait = secs_until(sched.hour, sched.minute, now);
            tracing::info!(
                "minimax 自动签到:下次北京时间 {:02}:{:02}，抖动 {}s",
                sched.hour,
                sched.minute,
                sched.jitter_secs
            );
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            let jitter = rand_secs(sched.jitter_secs);
            if jitter > 0 {
                tracing::info!("minimax 自动签到:抖动 {jitter}s 后开始");
                tokio::time::sleep(std::time::Duration::from_secs(jitter)).await;
            }
            run_checkins(&store, sched.jitter_secs).await;
        }
    });
}

async fn run_checkins(store: &crate::store::AppStore, jitter_secs: u64) {
    let providers = match store.channels.active_channels().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("minimax 自动签到:读渠道失败: {e}");
            return;
        }
    };
    let targets: Vec<_> = providers
        .into_iter()
        .filter(|p| p.channel_kind.trim() == "minimax-code")
        .collect();
    if targets.is_empty() {
        tracing::info!("minimax 自动签到:无活跃账号,跳过");
        return;
    }
    tracing::info!("minimax 自动签到开始:{} 个账号", targets.len());
    for p in targets {
        let gap = rand_secs(jitter_secs.clamp(1, 30));
        tokio::time::sleep(std::time::Duration::from_secs(gap)).await;
        let token = crate::crypto::decrypt_or_plain(&store.cipher, &p.api_key);
        let client = reqwest::Client::new();
        match claim_signin(&client, &token).await {
            Ok(_) => tracing::info!("minimax 自动签到({}) 完成", p.id),
            Err(e) => tracing::warn!("minimax 自动签到({}) 失败: {e}", p.id),
        }
    }
}

fn secs_until(hour: u32, minute: u32, now_unix: i64) -> u64 {
    let target = i64::from(hour) * 3600 + i64::from(minute) * 60 - 8 * 3600;
    let now = now_unix.rem_euclid(86_400);
    let mut delta = target - now;
    if delta <= 0 {
        delta += 86_400;
    }
    delta as u64
}

fn rand_secs(max: u64) -> u64 {
    if max == 0 {
        return 0;
    }
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    n % (max + 1)
}

/// MiniMax Code 在线登录：OAuth device code。
/// 对照 minimax-code：account.minimax.cn /oauth2/device/code + /oauth2/token。
const CLIENT_ID: &str = "mcode-public";
const SCOPE: &str = "agent.default";
const AUDIENCE: &str = "agent-backend";

#[derive(Clone)]
pub struct DeviceFlow {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub code_verifier: String,
    pub interval_sec: u64,
    pub expires_in: u64,
    pub poll_as_user_code: bool,
}

pub async fn start_device_flow(client: &reqwest::Client, cn: bool) -> Result<DeviceFlow, String> {
    let origin = if cn {
        "https://account.minimax.cn"
    } else {
        "https://account.minimax.io"
    };
    let verifier = random_b64url(32);
    let challenge = sha256_b64url(verifier.as_bytes());
    let resp = client
        .post(format!("{origin}/oauth2/device/code"))
        .form(&[
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("audience", AUDIENCE),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("device/code HTTP {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    let user_code = body["user_code"].as_str().unwrap_or("").to_string();
    let device_code = body["device_code"]
        .as_str()
        .unwrap_or(user_code.as_str())
        .to_string();
    let verification_uri = body["verification_uri"]
        .as_str()
        .or_else(|| body["verification_url"].as_str())
        .unwrap_or("")
        .to_string();
    if user_code.is_empty() || verification_uri.is_empty() {
        return Err("device/code 响应缺少 user_code 或 verification_uri".into());
    }
    let poll_as_user_code = body.get("device_code").and_then(|v| v.as_str()).is_none();
    Ok(DeviceFlow {
        device_code,
        user_code,
        verification_uri_complete: body["verification_uri_complete"]
            .as_str()
            .unwrap_or(&verification_uri)
            .to_string(),
        verification_uri,
        code_verifier: verifier,
        interval_sec: body["interval"].as_u64().unwrap_or(5),
        expires_in: body["expires_in"].as_u64().unwrap_or(300),
        poll_as_user_code,
    })
}

pub async fn poll_device_token(
    client: &reqwest::Client,
    cn: bool,
    flow: &DeviceFlow,
) -> Result<Option<Value>, String> {
    let origin = if cn {
        "https://account.minimax.cn"
    } else {
        "https://account.minimax.io"
    };
    let mut form = vec![
        (
            "grant_type",
            "urn:ietf:params:oauth:grant-type:device_code".to_string(),
        ),
        ("client_id", CLIENT_ID.to_string()),
        ("code_verifier", flow.code_verifier.clone()),
    ];
    if flow.poll_as_user_code {
        form.push(("user_code", flow.user_code.clone()));
    } else {
        form.push(("device_code", flow.device_code.clone()));
    }
    let resp = client
        .post(format!("{origin}/oauth2/token"))
        .form(&form)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        let st = body["status"].as_str().unwrap_or("");
        if st == "pending" || st == "slow_down" {
            return Ok(None);
        }
        if body.get("access_token").is_some() {
            return Ok(Some(body));
        }
        return Ok(None);
    }
    let err = body["error"].as_str().unwrap_or("");
    if err == "authorization_pending" || err == "slow_down" {
        return Ok(None);
    }
    Err(err.to_string())
}

fn random_b64url(n: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    b64url(&buf)
}

fn sha256_b64url(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    b64url(&h.finalize())
}

fn b64url(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < data.len() {
        let a = data[i] as u32;
        let b = *data.get(i + 1).unwrap_or(&0) as u32;
        let c = *data.get(i + 2).unwrap_or(&0) as u32;
        let triple = (a << 16) | (b << 8) | c;
        out.push(T[((triple >> 18) & 63) as usize] as char);
        out.push(T[((triple >> 12) & 63) as usize] as char);
        if i + 1 < data.len() {
            out.push(T[((triple >> 6) & 63) as usize] as char);
        }
        if i + 2 < data.len() {
            out.push(T[(triple & 63) as usize] as char);
        }
        i += 3;
    }
    out.replace('+', "-").replace('/', "_")
}
