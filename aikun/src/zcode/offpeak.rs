//! ZCode off-peak(闲时排队取号)通道——2026-11-20 上游改版后的免费/
//! Lite credits 唯一 chat 通路(旧 billing 免费桶 + zcode-plan 端点对
//! 非全量订阅同日起一律 1005;preview 已空,claim 通道下线)。
//!
//! 完整链路(炼化自 ZCode 客户端 3.18.2 + 2026-11-20 活体验证):
//! 1. `POST /api/v1/off-peak/ticket {"task_id":<UUID>}` 取号——task_id
//!    必须 UUID 形态(任意串 3001),幂等(同 task_id 返回既有票);
//!    **每日取票有限额**(用尽 → 429 code 3103 + next_take_at=次日零点)。
//! 2. `POST /api/v1/off-peak/ticket/status {"ticket_ids":[...]}` 轮询
//!    (next_poll_after 秒;实测 ~2.5 位/分钟,百人队 ≈ 40 分钟)。
//! 3. state ready/active(ready 窗口 ~5 分钟)后调
//!    `POST /api/v1/off-peak/anthropic/v1/messages`——头:
//!    `authorization: Bearer <JWT>` + **`x-coding-plan-api-key: <JWT 同值>`**
//!    (传 OAuth 业务 token 反而 3001,与 availability 端点口径相反)+
//!    `X-Off-Peak-Ticket-ID: <票>` + 常规伪装头族。
//! 4. 成功后 `POST /api/v1/off-peak/ticket/{id}/settle` 结算(主动结算
//!    会终止窗口——网关为复用不主动 settle,让票自然过期)。
//!
//! 网关策略:每渠道(api_key)缓存一张票,ready/active 窗口内复用;
//! 排队等待受预算约束(AIKUN_ZCODE_OFFPEAK_WAIT_SECS,默认 90s;0=不
//! 等待,取号后立即报排位错)。每日限额/超时等不可恢复错误向上抛,
//! 由调用方(transport)回退 zcode-plan 端点保底。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde_json::Value;

const TICKET_PATH: &str = "/api/v1/off-peak/ticket";
const STATUS_PATH: &str = "/api/v1/off-peak/ticket/status";
const SETTLE_PATH_FMT: &str = "/api/v1/off-peak/ticket/{tid}/settle";
const MESSAGES_PATH: &str = "/api/v1/off-peak/anthropic/v1/messages";

/// 每渠道票上下文(task_id 决定上游幂等;expired/settled 后换新)。
#[derive(Clone)]
struct Ticket {
    ticket_id: String,
    task_id: String,
    state: String,
    /// 最后已知的排队位置(-1=未知)。
    position: i64,
}

static TICKETS: LazyLock<Mutex<HashMap<String, Ticket>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 排队等待预算(秒;0=取号后不等)。默认 90。
fn wait_budget_secs() -> u64 {
    std::env::var("AIKUN_ZCODE_OFFPEAK_WAIT_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(90)
}

/// 业务 token → PAT(进程内缓存;签发失败返回 None 走 JWT 兜底)。
/// PAT 与 JWT 的对应由调用侧在 ensure 时解析,这里只查缓存。
static PAT_BY_JWT: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn pat_for(jwt: &str) -> Option<String> {
    PAT_BY_JWT.lock().unwrap().get(jwt).cloned()
}

fn origin_of(base: &str) -> String {
    let mut b = base.trim_end_matches('/').to_string();
    if let Some(s) = b.strip_suffix("/api/v1") {
        b = s.to_string();
    }
    b
}

/// off-peak 家族头(auth + x-coding-plan-api-key=**PAT** 官方真值 + 伪装族)。
/// PAT 签发需要业务 token(业务 token 过期则兜底 JWT 同值——实测
/// zcode.z.ai 域宽松接受,但订阅额度路由可能只认 PAT)。
#[allow(clippy::too_many_arguments)]
async fn request_json(
    method: reqwest::Method,
    base: &str,
    path: &str,
    jwt: &str,
    body: Option<&Value>,
    ticket: Option<&str>,
    query: Option<&str>,
) -> Result<(u16, Value), String> {
    use aikun_channels::adapters::zcode::CLIENT_VERSION;
    let url = format!("{}{path}{}", origin_of(base), query.unwrap_or(""));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;
    let mut req = client.request(method, &url);
    // 头族:billing 风格基础 + anthropic 伪装 + off-peak 三头(chat 用)。
    for (k, v) in aikun_channels::adapters::zcode::billing_request_headers(
        jwt,
        &aikun_channels::adapters::zcode::device_mid_for(jwt),
        &uuid::Uuid::new_v4().to_string(),
    ) {
        req = req.header(k, v.as_str());
    }
    // PAT:从 zcode_oauth 侧表的业务 token 签发(调用方注入 jwt 指纹
    // 对应的业务 token;签发失败兜底 JWT 同值)。
    let cpak = pat_for(jwt).unwrap_or_else(|| jwt.to_string());
    req = req
        .header("x-coding-plan-api-key", cpak)
        .header("User-Agent", format!("ZCode/{CLIENT_VERSION} electron"))
        .header("X-Title", "Z Code@electron")
        .header("X-ZCode-Agent", "glm");
    if let Some(tid) = ticket {
        req = req.header("X-Off-Peak-Ticket-ID", tid);
    }
    if let Some(b) = body {
        req = req.header("content-type", "application/json").json(b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("请求失败: {}", e.without_url()))?;
    let status = resp.status().as_u16();
    let parsed: Value = resp.json().await.unwrap_or(Value::Null);
    Ok((status, parsed))
}

/// 取号/复用(task_id 幂等;过期票换新 task)。`allow_take=false` 时只用
/// 渠道已持有的票(诊断类调用不烧每日取号限额)。
async fn ensure_ticket(
    base: &str,
    jwt: &str,
    cache_key: &str,
    allow_take: bool,
) -> Result<Ticket, String> {
    if let Some(t) = TICKETS.lock().unwrap().get(cache_key)
        && matches!(t.state.as_str(), "queued" | "ready" | "active")
    {
        return Ok(t.clone());
    }
    if !allow_take {
        return Err(
            "off-peak 无可用票(诊断模式不取新号——取号每日有限额,留给真实流量;由真实请求或手动触发取号)"
                .to_string(),
        );
    }
    // 新 task(UUID 形态是硬要求,任意串 3001)
    let task_id = uuid::Uuid::new_v4().to_string();
    let (status, body) = request_json(
        reqwest::Method::POST,
        base,
        TICKET_PATH,
        jwt,
        Some(&serde_json::json!({ "task_id": task_id })),
        None,
        None,
    )
    .await?;
    let code = body["code"].as_i64().unwrap_or(-1);
    if status != 200 || code != 0 {
        // 3103 每日限额:next_take_at 透传给调用方
        let next = body["data"]["next_take_at"]
            .as_i64()
            // 毫秒口径(实测 1795190400000 = 次日北京零点)
            .map(|n| if n > 1_000_000_000_000 { n / 1000 } else { n });
        return Err(format!(
            "off-peak 取号失败(code {code} {}){}",
            body["msg"].as_str().unwrap_or_default(),
            next.map(|n| format!(
                ":今日取号限额已用完,北京时间 {} 后可再取",
                chrono::DateTime::from_timestamp(n, 0)
                    .map(|d| d
                        .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
                        .format("%m-%d %H:%M")
                        .to_string())
                    .unwrap_or_else(|| n.to_string())
            ))
            .unwrap_or_default()
        ));
    }
    let d = &body["data"];
    let t = Ticket {
        ticket_id: d["ticket_id"].as_str().unwrap_or_default().to_string(),
        task_id,
        state: d["state"].as_str().unwrap_or("queued").to_string(),
        position: d["position"].as_i64().unwrap_or(-1),
    };
    if t.ticket_id.is_empty() {
        return Err("off-peak 取号响应缺 ticket_id".to_string());
    }
    TICKETS
        .lock()
        .unwrap()
        .insert(cache_key.to_string(), t.clone());
    Ok(t)
}

/// 单次状态刷新(不轮询)。
async fn refresh_ticket(base: &str, jwt: &str, t: &mut Ticket) -> Result<(), String> {
    let (_status, body) = request_json(
        reqwest::Method::POST,
        base,
        STATUS_PATH,
        jwt,
        Some(&serde_json::json!({ "ticket_ids": [t.ticket_id] })),
        None,
        None,
    )
    .await?;
    if body["code"].as_i64() != Some(0) {
        return Err(format!(
            "off-peak 状态查询失败(code {})",
            body["code"].as_i64().unwrap_or(-1)
        ));
    }
    if let Some(tk) = body["data"]["tickets"].as_array().and_then(|a| a.first()) {
        if let Some(s) = tk["state"].as_str() {
            t.state = s.to_string();
        }
        if let Some(pos) = tk["position"].as_i64() {
            t.position = pos;
        }
    }
    Ok(())
}

/// off-peak chat 编排:取号(可选)→ (等待预算内)排队 → ready 后调用。
/// 返回 (HTTP 响应, 排队耗时秒);上游 HTTP 状态原样透传由调用方判定。
///
/// `allow_take`:是否允许取**新**号。取号每日有限额(实测烧完次日北京
/// 零点恢复)——真实转发流量传 true;诊断类调用(周期模型测试)传
/// false 只用已有票,避免后台任务把一天的限额烧光(2026-11-21 实录:
/// 30 分钟一轮 × 2 模型,当天即耗尽,三条车道全失败)。
pub async fn chat(
    base: &str,
    jwt: &str,
    body: &Value,
    allow_take: bool,
    bm_token: Option<&str>,
) -> Result<(reqwest::Response, u64), String> {
    let cache_key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        jwt.hash(&mut h);
        h.finish().to_string()
    };
    // 官方口径:x-coding-plan-api-key = PAT(业务 token 三步签发;短期
    // PAT,每次请求前刷新——官方「每次模型请求刷新 PAT」同款语义)。
    if let Some(bm) = bm_token.filter(|t| !t.trim().is_empty()) {
        match super::pat::ensure_pat(bm).await {
            Ok(pat) => {
                PAT_BY_JWT.lock().unwrap().insert(jwt.to_string(), pat);
            }
            Err(e) => {
                // 过期/签发失败:兜底 JWT 同值并留痕(订阅路由可能拒绝)
                tracing::warn!("off-peak PAT 签发失败,兜底 JWT 同值:{e}");
                PAT_BY_JWT.lock().unwrap().remove(jwt);
            }
        }
    }
    let started = std::time::Instant::now();
    let mut ticket = ensure_ticket(base, jwt, &cache_key, allow_take).await?;

    // 排队等待(预算内;0=不等待)
    let budget = wait_budget_secs();
    while ticket.state == "queued" && started.elapsed().as_secs() < budget {
        let (status, body) = request_json(
            reqwest::Method::POST,
            base,
            STATUS_PATH,
            jwt,
            Some(&serde_json::json!({ "ticket_ids": [ticket.ticket_id] })),
            None,
            None,
        )
        .await?;
        let poll = body["data"]["next_poll_after"]
            .as_i64()
            .unwrap_or(30)
            .clamp(5, 90) as u64;
        if status == 200
            && let Some(tk) = body["data"]["tickets"].as_array().and_then(|a| a.first())
            && let Some(s) = tk["state"].as_str()
        {
            ticket.state = s.to_string();
            if let Some(pos) = tk["position"].as_i64() {
                ticket.position = pos;
            }
            TICKETS
                .lock()
                .unwrap()
                .insert(cache_key.clone(), ticket.clone());
        }
        if ticket.state != "queued" {
            break;
        }
        tokio::time::sleep(Duration::from_secs(poll)).await;
    }

    match ticket.state.as_str() {
        "ready" | "active" => {
            // 调用前刷新一次:ready 窗口仅 ~5 分钟,缓存里的 ready 可能已
            // 过期——不刷新会烧一次 400 才发现。
            refresh_ticket(base, jwt, &mut ticket).await?;
            TICKETS
                .lock()
                .unwrap()
                .insert(cache_key.clone(), ticket.clone());
            if !matches!(ticket.state.as_str(), "ready" | "active") {
                TICKETS.lock().unwrap().remove(&cache_key);
                return Err(format!(
                    "off-peak 票已失效(state={}),已丢弃待重取",
                    ticket.state
                ));
            }
        }
        "queued" => {
            let (_status, body) = request_json(
                reqwest::Method::POST,
                base,
                STATUS_PATH,
                jwt,
                Some(&serde_json::json!({ "ticket_ids": [ticket.ticket_id] })),
                None,
                None,
            )
            .await?;
            let pos = body["data"]["tickets"][0]["position"]
                .as_i64()
                .unwrap_or(-1);
            return Err(format!(
                "off-peak 免费通道排队中(第 {pos} 位,已取号;等待预算 {budget}s 内未出队——稍后重试或提高 AIKUN_ZCODE_OFFPEAK_WAIT_SECS)"
            ));
        }
        other => {
            // expired/settled:丢弃缓存票,下次重取
            TICKETS.lock().unwrap().remove(&cache_key);
            return Err(format!("off-peak 票不可用(state={other}),已丢弃待重取"));
        }
    }

    // 调用(metadata 补 off_peak_task_id——与官方任务语义对齐)
    let mut b = body.clone();
    let mid = b["metadata"]["user_id"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| "{}".into());
    if let Ok(mut m) = serde_json::from_str::<Value>(&mid) {
        m["off_peak_task_id"] = Value::String(ticket.task_id.clone());
        b["metadata"]["user_id"] = Value::String(m.to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;
    use aikun_channels::adapters::zcode::CLIENT_VERSION;
    let url = format!("{}{MESSAGES_PATH}", origin_of(base));
    let mut req = client.post(&url).json(&b);
    for (k, v) in aikun_channels::adapters::zcode::billing_request_headers(
        jwt,
        &aikun_channels::adapters::zcode::device_mid_for(jwt),
        &uuid::Uuid::new_v4().to_string(),
    ) {
        req = req.header(k, v.as_str());
    }
    req = req
        .header("x-coding-plan-api-key", jwt)
        .header("x-api-key", jwt)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "mid-conversation-system-2026-04-07")
        .header("User-Agent", format!("ZCode/{CLIENT_VERSION} electron"))
        .header("X-Title", "Z Code@electron")
        .header("X-ZCode-Agent", "glm")
        .header("x-zcode-query-source", "main_turn")
        .header("x-zcode-session-type", "main")
        .header("X-Off-Peak-Ticket-ID", &ticket.ticket_id);
    let resp = req
        .send()
        .await
        .map_err(|e| format!("off-peak 调用失败: {}", e.without_url()))?;
    let waited = started.elapsed().as_secs();
    // 票失效类错误:丢弃缓存供下次重取
    if resp.status().as_u16() >= 400 {
        TICKETS.lock().unwrap().remove(&cache_key);
    }
    Ok((resp, waited))
}

/// 显式结算(目前仅诊断用;网关正常路径不 settle——保持窗口可复用)。
#[allow(dead_code)]
pub async fn settle(base: &str, jwt: &str, ticket_id: &str) -> Result<(), String> {
    let path = SETTLE_PATH_FMT.replace("{tid}", ticket_id);
    request_json(
        reqwest::Method::POST,
        base,
        &path,
        jwt,
        Some(&Value::Null),
        None,
        None,
    )
    .await?;
    Ok(())
}
