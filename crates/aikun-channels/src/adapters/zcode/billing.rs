//! ZCode billing / 免费额度领取家族:端点常量、请求头与 JWT 派生工具。
//!
//! 领取序列(激活事件 → preview/touch 清单 → 逐项 claim → campaign
//! confirm)的编排 在 aikun-admin(`ZcodeClaimPort` 端口)与二进制
//! 侧实现;本模块只提供纯数据与纯函数(HTTP 无关,crate 纯净度)。

/// billing 家族(balance/preview/claim + marketing/touch)的客户端版本:
/// Zcode2Api 与聊天伪装基线分开维护(3.18.2 略新),跟随。
pub const BILLING_CLIENT_VERSION: &str = "3.18.2";

/// `X-Os-Version` 值(os.release() 语义,非营销名;billing 头与激活事件
/// 共用同一字符串)。
pub const BILLING_OS_VERSION: &str = "Darwin Kernel Version 21.6.0; Mon Jun 24 00:56:10 PDT 2024; \
     root:xnu-8020.240.18.709.2~1/RELEASE_X86_64";

// ---- 免费额度领取(claim)家族 -------------------------------------------
// 炼化自 Zcode2Api warm.go:官方免费/体验额度**不会自动到账**,需用登录态
// JWT 调 billing/claim 领取;可领清单来自 billing/preview(按 priority
// 排序)与 marketing/touch 横幅按钮,已持有的(balance 的 plan_id 列表)
// 跳过(claim 回 1003 也视为成功)。campaign 活动领取后需 POST
// touch/action confirm。领取前发两条激活事件(app_launch /
// app_daily_active,上游按 device_mid+日期去重)。

/// 激活事件上报(领取前置;上游按 device_mid+date 去重)。
pub const EVENT_REPORT_PATH: &str = "/api/v1/event/report";

/// 可领套餐/额度清单(GET):`{code, data:{plans:[{plan_id,name,priority}]}}`。
pub const PREVIEW_PATH: &str = "/api/v1/zcode-plan/billing/preview";

/// 领取(POST `{"plan_id"}`):code 0 = 领取成功,1003 = 已领取,3007 = 风控。
pub const CLAIM_PATH: &str = "/api/v1/zcode-plan/billing/claim";

/// 营销投放(GET `?seq=0`):横幅按钮 `action.args.plan_id` + campaign_id。
pub const TOUCH_PATH: &str = "/api/v1/marketing/touch";

/// 投放确认(POST `{campaign_id, action_type:"confirm"}`)。
pub const TOUCH_ACTION_PATH: &str = "/api/v1/marketing/touch/action";

// ---- 订阅 / 额度重置(coding-plan/reset)家族 ----------------------------
// 端点与认证形态炼化自 ZCode 桌面客户端 3.18.2(2026-11-20 活体验证):
// reset 家族挂 zcode.z.ai(跟渠道 base 走),要求**双头认证**——
// `Authorization: Bearer <JWT>` + `X-Bigmodel-Authorization: <JWT>`(上游
// 接受同值 JWT;官方客户端传的是 OAuth 业务 token,两处都过)+ 必带
// `Bigmodel-Target-Type: PERSONAL`,缺任一 3001 parameter error。

/// 可用重置次数与最近使用记录(GET;零消耗,账号信息数据源)。
pub const RESET_STATUS_PATH: &str = "/api/v1/coding-plan/reset/status";

/// 使用一次重置(POST `{idempotency_key, reset_type}`;**消耗性动作**,
/// reset_type = "FIVE_HOUR" | "WEEK",响应 `{data:{used:true}}`)。
pub const RESET_USE_PATH: &str = "/api/v1/coding-plan/reset/use";

/// reset 家族在 billing 头之上的增量头(实测缺 `Bigmodel-Target-Type`
/// 即 3001;`X-Bigmodel-Authorization` 传 JWT 本身即过)。
pub fn reset_extra_headers(jwt: &str) -> Vec<(&'static str, String)> {
    vec![
        ("X-Bigmodel-Authorization", jwt.to_string()),
        ("Bigmodel-Target-Type", "PERSONAL".to_string()),
    ]
}

// ---- 订阅 / 5h·周额度(bigmodel monitor 家族;国内 Coding Plan) ---------
// 付费订阅(GLM Coding Plan Lite 等)的订阅信息与 5 小时/每周滚动额度
// 不在 zcode.z.ai billing 里(balance 只见免费桶),而在账号家族的
// monitor 域:国内 bigmodel.cn、国际 api.z.ai。鉴权用 OAuth 业务 token
// (poll 响应 `data.bigmodel.access_token` / `data.zai.access_token`;
// 官方客户端缺此字段即判授权响应无效,故在线登录渠道必有)。

/// 国内(bigmodel 家族)monitor 域。
pub const BIGMODEL_HOST: &str = "https://bigmodel.cn";
/// 国际(zai 家族)monitor 域。
pub const ZAI_HOST: &str = "https://api.z.ai";

/// 订阅清单(GET;`data` 为订阅数组或对象,active 项含 productId /
/// productName / billingCycle / expireTime)。
pub const BM_SUBSCRIPTION_PATH: &str = "/api/biz/subscription/list";

/// 用量额度(GET;`data:{level, limits:[{type, unit, number, usage,
/// remaining, percentage, nextResetTime, ...}]}`,5 小时窗与周窗各一条)。
pub const BM_QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";

/// billing/claim 家族的完整请求头(炼化自 Zcode2Api billingHeaders,版本
/// 3.18.2)。与聊天伪装头(extra_headers)的差异:带 `Authorization`、
/// `X-Device-Mid`、`X-Os-Version`(os.release() 语义,非营销名)与
/// `x-request-id`/`sec-fetch-mode`。
pub fn billing_request_headers(
    jwt: &str,
    device_mid: &str,
    request_id: &str,
) -> Vec<(&'static str, String)> {
    vec![
        ("Authorization", format!("Bearer {jwt}")),
        ("Accept", "*/*".to_string()),
        ("Accept-Language", "*".to_string()),
        ("User-Agent", format!("ZCode/{BILLING_CLIENT_VERSION}")),
        ("HTTP-Referer", "https://zcode.z.ai".to_string()),
        ("X-Title", "Z Code@electron".to_string()),
        ("X-ZCode-App-Version", BILLING_CLIENT_VERSION.to_string()),
        ("X-Os-Category", "macos".to_string()),
        ("X-Os-Version", BILLING_OS_VERSION.to_string()),
        ("X-Platform", "darwin-x64".to_string()),
        ("X-Release-Channel", "production".to_string()),
        ("X-Client-Language", "en-US".to_string()),
        ("X-Client-Timezone", "Asia/Shanghai".to_string()),
        ("X-Device-Mid", device_mid.to_string()),
        ("x-request-id", request_id.to_string()),
        ("sec-fetch-mode", "cors".to_string()),
    ]
}

/// MCP 透传请求的客户端侧头(转发到上游的请求头白名单):会话与协议版本
/// 由 MCP 客户端持有,Accept 决定 JSON/SSE 响应形态,均须原样送达上游。
/// 从 JWT payload 段解出 `sub`(激活事件的 user_id)。JWT 无 sub / 载荷
/// 不可解析返回 None——调用方按"跳过事件"处理(与 Zcode2Api 口径一致)。
pub fn jwt_sub(jwt: &str) -> Option<String> {
    let payload_b64 = jwt.split('.').nth(1)?;
    let raw = base64url_decode(payload_b64)?;
    let claims: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    claims.get("sub")?.as_str().map(str::to_string)
}

/// 渠道的稳定 device_mid:由 JWT 派生(双轮 FNV-1a → 32 hex,UUID 形态)。
/// 稳定是硬要求——上游按 device_mid+日期对激活事件去重,随机值会让每次
/// 领取都发新"设备"的事件。
pub fn device_mid_for(jwt: &str) -> String {
    fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
        bytes.iter().fold(seed, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3)
        })
    }
    let a = fnv1a(0xcbf2_9ce4_8422_2325, jwt.as_bytes());
    let b = fnv1a(a, b"zcode-device-mid-salt");
    let hex = format!("{a:016x}{b:016x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// 无依赖的 base64url(RFC 4648 §5,无填充)解码;非法字符返回 None。
fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s
        .bytes()
        .filter(|b| !matches!(b, b'\n' | b'\r' | b' ' | b'='))
        .collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut acc: u32 = 0;
        for (i, c) in chunk.iter().enumerate() {
            acc |= u32::from(val(*c)?) << (18 - 6 * i);
        }
        out.push((acc >> 16) as u8);
        if chunk.len() > 2 {
            out.push((acc >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(acc as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billing_headers_shape() {
        let h = billing_request_headers("jwt-x", "mid-1", "req-1");
        let get = |k: &str| {
            h.iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("Authorization"), "Bearer jwt-x");
        assert_eq!(get("X-Device-Mid"), "mid-1");
        assert_eq!(get("x-request-id"), "req-1");
        assert_eq!(get("User-Agent"), "ZCode/3.18.2", "billing 家族独立版本");
        assert_eq!(get("X-ZCode-App-Version"), "3.18.2");
        assert!(
            get("X-Os-Version").starts_with("Darwin Kernel Version"),
            "os.release() 语义"
        );
        assert!(get("sec-fetch-mode") == "cors");
    }

    #[test]
    fn jwt_sub_and_malformed() {
        // payload {"sub":"u-123"} 的 base64url(无填充)
        assert_eq!(
            jwt_sub("h.eyJzdWIiOiJ1LTEyMyJ9.s"),
            Some("u-123".to_string())
        );
        assert_eq!(jwt_sub("not-a-jwt"), None, "无 payload 段");
        assert_eq!(jwt_sub("h.!!!.s"), None, "载荷非法 base64url");
        // payload 无 sub
        assert_eq!(jwt_sub("h.e30.s"), None);
    }

    #[test]
    fn device_mid_stable_and_uuid_shaped() {
        let m1 = device_mid_for("jwt-a");
        assert_eq!(m1, device_mid_for("jwt-a"), "同一凭证派生稳定");
        assert_ne!(m1, device_mid_for("jwt-b"));
        let parts: Vec<&str> = m1.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "UUID 形态"
        );
        assert!(m1.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }
}
