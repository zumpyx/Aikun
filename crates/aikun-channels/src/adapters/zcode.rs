//! ZCode 渠道适配器(GLM Coding Plan:个人订阅 + 官方闲时免费额度)。
//!
//! 协议细节炼化自社区实现 D3-vin/Zcode2Api(MIT 级公开仓库,Go)与
//! zcode.z.ai 官方文档(2026-11-18 调研):
//! * 上游为 **Anthropic Messages 形态**:`POST {base}/api/v1/zcode-plan/
//!   anthropic/v1/messages`,base = `https://zcode.z.ai`;
//! * 鉴权:**JWT(ZCode 客户端登录态,渠道的 api_key 字段)同时放
//!   `x-api-key` 与 `Authorization: Bearer`**——与 generic 的 Anthropic
//!   默认鉴权头一致,无需覆写;
//! * 客户端伪装头(`anthropic-beta` / `HTTP-Referer` / `User-Agent` /
//!   `X-ZCode-*` 等)经 [`ChannelAdapter::extra_headers`] 注入(炼化自
//!   Zcode2Api chatheaders.go,自 ZCode 3.16.3 抓包);
//! * 健康 / 额度探测:GET `{base}/api/v1/zcode-plan/billing/balance`
//!   (2xx 即健康,**零消耗**,不打聊天端点烧额度);
//! * 模型:GLM-5.3 / GLM-5.3-Flash(以渠道配置为准,可从 ZCode 客户端
//!   或账单页抄)。
//!
//! **闲时免费**(官方规则,https://zcode.z.ai/cn/docs):每晚 23:00 至
//! 次日 09:00(北京时间)套餐内 GLM-5.3-Flash 额度消耗为 0;非高峰
//! 1 倍 / 高峰 3 倍消耗系数只影响上游套餐额度,不改变网关对下游的
//! 计费口径。网关侧由 [`upstream_free`] 判定:闲时窗口内的 Flash
//! 请求**对下游用户 0 元记账**(tokens 照记、日志照落;pipeline 在
//! 选中渠道时写入 RequestCtx,见 pipeline/attempt.rs)。
//!
//! 已知裁剪:上游风控验证码(Zcode2Api 的 3007 + Aliyun captcha 流程)
//! 不做——触发时按上游失败处理,由既有故障转移接管。

use aikun_protocol::Protocol;

use crate::ChannelAdapter;

pub struct ZcodeAdapter;

pub const KIND: &str = "zcode";

/// 上游聊天端点(Anthropic 形态;ZCode 计划只有这一条通道,渠道应只勾选
/// anthropic 协议——其他协议返回同一路径,误配时请求形状不符会被上游拒绝)。
pub const MESSAGES_PATH: &str = "/api/v1/zcode-plan/anthropic/v1/messages";

/// 零消耗健康/额度探测端点(GET,2xx = healthy;响应含 entitlements
/// 的 remaining/limit/period_end,可用于人工核对剩余额度)。
pub const BALANCE_PATH: &str = "/api/v1/zcode-plan/billing/balance";

/// 伪装客户端版本(与抓包基线一致;上游按此校验 X-ZCode-App-Version)。
pub const CLIENT_VERSION: &str = "3.16.3";

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

// ---- MCP 远程工具 --------------------------------------------------------
// ZCode 官方插件 `.mcp.json` 揭示的远程 MCP 服务形态(2026-11-18 本地
// 插件缓存实测):`{ZCODE_BASE_URL}/api/v1/mcp/server/{server_key}`,HTTP
// 传输(MCP Streamable HTTP,JSON-RPC),`auth: zcode_official /
// jwt_token` = 同一 ZCode 登录态 JWT,`requiresPaidPlan: true`(消耗套餐
// 权限)。网关按此把 MCP 请求透传给 zcode 渠道(见 aikun/src/mcp.rs)。

/// MCP 远程服务路径前缀;拼接规则 `join_absolute(base, prefix + server_key)`。
pub const MCP_SERVER_PATH_PREFIX: &str = "/api/v1/mcp/server/";

/// 已知 MCP 服务目录(本地官方插件枚举;目录仅用于文档展示,网关不封死
/// ——任意 server_key 照常透传,存在性由上游裁决)。
pub const MCP_SERVERS: &[(&str, &str)] = &[
    (
        "finance_tianyancha",
        "天眼查企业信息查询(工商/股权/涉诉/舆情)",
    ),
    ("image_search", "图片搜索"),
];

/// 官方闲时窗口(北京时间):每晚 23:00 至次日 09:00,含头不含尾。
/// ZCode 夜间畅用:窗口内 GLM-5.3-Flash 套餐额度消耗为 0。
const OFF_PEAK_START_HOUR: u32 = 23;
const OFF_PEAK_END_HOUR: u32 = 9;

impl ChannelAdapter for ZcodeAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn label(&self) -> &'static str {
        "ZCode(GLM Coding Plan)"
    }

    fn description(&self) -> &'static str {
        "智谱 ZCode 订阅渠道:API Key 填 ZCode 客户端 JWT(登录后从客户端复制),\
         只勾选 Anthropic 协议,Base URL 填 https://zcode.z.ai;\
         个人订阅额度与免费每日额度共用同一凭证,闲时(23:00-09:00)Flash 零计费"
    }

    fn endpoint_path(&self, _protocol: Protocol) -> &'static str {
        MESSAGES_PATH
    }

    /// 端点路径是完整绝对路径(/api/v1/...),不经 `build_api_url` 的
    /// `/v1` 追加逻辑(base 无版本段时会被补 /v1 打坏路径)。
    fn chat_url(&self, provider: &crate::provider::Provider, _protocol: Protocol) -> String {
        join_absolute(
            crate::provider::base_url_for(provider, _protocol),
            MESSAGES_PATH,
        )
    }

    fn models_url(&self, base_url: &str, _protocol: Protocol) -> String {
        join_absolute(
            base_url,
            &format!("{BALANCE_PATH}?app_version={CLIENT_VERSION}"),
        )
    }

    fn extra_headers(&self, protocol: Protocol) -> Vec<(&'static str, String)> {
        if protocol != Protocol::Anthropic {
            return Vec::new();
        }
        // 炼化自 Zcode2Api chatheaders.go(静态部分;anthropic-version 由
        // 默认鉴权头携带,不重复)。per-request 的 UUID 头(query/trace/
        // request/session id)不注入——上游非硬性要求,网关自身已有
        // request_id 贯穿(9.6)。
        vec![
            (
                "anthropic-beta",
                "mid-conversation-system-2026-04-07".to_string(),
            ),
            ("HTTP-Referer", "https://zcode.z.ai".to_string()),
            ("User-Agent", format!("ZCode/{CLIENT_VERSION} electron")),
            ("X-Title", "Z Code@electron".to_string()),
            ("X-ZCode-Agent", "glm".to_string()),
            ("X-ZCode-App-Version", CLIENT_VERSION.to_string()),
            ("X-Platform", "darwin".to_string()),
            ("X-Release-Channel", "production".to_string()),
            ("X-Client-Language", "en-US".to_string()),
            ("X-Client-Timezone", "Asia/Shanghai".to_string()),
            ("X-Os-Category", "Mac".to_string()),
        ]
    }
}

/// 绝对路径拼接:base 去尾斜杠后直接接 path(保留 base 自带 query 的场景
/// 不适用——zcode 的 base 是纯 origin)。
fn join_absolute(base_url: &str, path: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), path)
}

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
pub const MCP_FORWARDED_HEADERS: &[&str] = &["mcp-session-id", "mcp-protocol-version", "accept"];

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

/// 北京时间(UTC+8)的当前小时。
fn beijing_hour(unix_now: i64) -> u32 {
    let secs_into_day = (unix_now + 8 * 3600).rem_euclid(86_400);
    (secs_into_day / 3600) as u32
}

/// 是否处于官方闲时窗口(北京时间 23:00–09:00,含头不含尾)。
/// 跨零点区间:`h >= 23 || h < 9`。
pub fn is_off_peak(unix_now: i64) -> bool {
    let h = beijing_hour(unix_now);
    !(OFF_PEAK_END_HOUR..OFF_PEAK_START_HOUR).contains(&h)
}

/// 模型是否为闲时零消耗档(名称含 "flash",大小写与分隔符不敏感)。
pub fn is_flash_model(model: &str) -> bool {
    let norm: String = model
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    norm.contains("flash")
}

/// 该请求的上游消耗是否为零(闲时窗口 × Flash 模型)。
/// pipeline 在选中 zcode 渠道时调用,写入 RequestCtx 决定下游计费口径。
pub fn upstream_free(model: &str, unix_now: i64) -> bool {
    is_flash_model(model) && is_off_peak(unix_now)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 北京时间整点对应的 unix 秒(2026-11-18 为任意同日锚点即可,只关心小时)
    fn unix_at_beijing(hour: u32, minute: u32) -> i64 {
        // 1970-01-01 00:00 北京时间 = 1969-12-31T16:00:00Z(unix = -28800)
        // 取一个已知的对齐锚:0 = 北京 08:00(1970-01-01 08:00 CST)
        // 直接构造:任意一天,北京 hour:minute → unix = day*86400 + hour*3600 + minute*60 - 28800
        let day: i64 = 20_000;
        day * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 - 28_800
    }

    #[test]
    fn off_peak_window_truth_table() {
        // 23:00(含)进入窗口
        assert!(is_off_peak(unix_at_beijing(23, 0)));
        assert!(is_off_peak(unix_at_beijing(23, 59)));
        assert!(is_off_peak(unix_at_beijing(0, 0)), "次日 00:00 在窗口内");
        assert!(is_off_peak(unix_at_beijing(3, 30)));
        assert!(is_off_peak(unix_at_beijing(8, 59)), "09:00 前最后一分钟");
        // 09:00(不含)出窗口
        assert!(!is_off_peak(unix_at_beijing(9, 0)));
        assert!(!is_off_peak(unix_at_beijing(12, 0)));
        assert!(!is_off_peak(unix_at_beijing(22, 59)), "23:00 前最后一分钟");
    }

    #[test]
    fn flash_model_matching() {
        assert!(is_flash_model("GLM-5.3-Flash"));
        assert!(is_flash_model("glm-5.3-flash"));
        assert!(is_flash_model("GLM_5.3_Flash"));
        assert!(!is_flash_model("GLM-5.3"));
        assert!(!is_flash_model(""));
    }

    #[test]
    fn upstream_free_combines_window_and_model() {
        let night = unix_at_beijing(2, 0);
        let day = unix_at_beijing(14, 0);
        assert!(upstream_free("GLM-5.3-Flash", night));
        assert!(!upstream_free("GLM-5.3-Flash", day), "白天 Flash 照常计费");
        assert!(!upstream_free("GLM-5.3", night), "闲时仅 Flash 免费");
    }

    #[test]
    fn urls_and_headers_shape() {
        let a = ZcodeAdapter;
        assert_eq!(
            a.endpoint_path(Protocol::Anthropic),
            "/api/v1/zcode-plan/anthropic/v1/messages"
        );
        assert_eq!(
            a.models_url("https://zcode.z.ai", Protocol::Anthropic),
            "https://zcode.z.ai/api/v1/zcode-plan/billing/balance?app_version=3.16.3"
        );
        let mut p = crate::provider::tests::bare_provider();
        p.openai_base_url = "https://zcode.z.ai".into();
        p.anthropic_base_url = "https://zcode.z.ai".into();
        assert_eq!(
            a.chat_url(&p, Protocol::Anthropic),
            "https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages",
            "完整绝对路径,不被追加 /v1"
        );
        let headers = a.extra_headers(Protocol::Anthropic);
        let names: Vec<_> = headers.iter().map(|(k, _)| *k).collect();
        assert!(names.contains(&"anthropic-beta"));
        assert!(names.contains(&"X-ZCode-Agent"));
        assert!(
            !names.contains(&"anthropic-version"),
            "由默认鉴权头携带,不重复"
        );
        assert!(a.extra_headers(Protocol::OpenAi).is_empty());
    }

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

    #[test]
    fn mcp_catalog_and_forwarded_headers() {
        assert!(MCP_SERVERS.iter().any(|(k, _)| *k == "finance_tianyancha"));
        assert!(MCP_FORWARDED_HEADERS.contains(&"mcp-session-id"));
        assert_eq!(
            format!("{MCP_SERVER_PATH_PREFIX}finance_tianyancha"),
            "/api/v1/mcp/server/finance_tianyancha"
        );
    }
}
