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
//! **免费窗口**(2026-11-19 用户口径):工作日晚 23:00–次日 09:00
//! 免费额度 **3 亿**;周末窗口周五 23:00 连续至周一 09:00(含周六/
//! 周日全天)**5 亿**。非高峰 1 倍 / 高峰 3 倍消耗系数只影响上游套餐
//! 额度,不改变网关对下游的计费口径。网关侧由 [`upstream_free`]
//! 判定:免费窗口内的 Flash 请求**对下游用户 0 元记账**(tokens 照记、
//! 日志照落;pipeline 在选中渠道时写入 RequestCtx,见 attempt.rs)。
//!
//! 已知裁剪:上游风控验证码(Zcode2Api 的 3007 + Aliyun captcha 流程)
//! 不做——触发时按上游失败处理,由既有故障转移接管。

pub mod billing;
pub mod mcp;

pub use billing::{
    BIGMODEL_HOST, BILLING_CLIENT_VERSION, BILLING_OS_VERSION, BM_QUOTA_PATH, BM_SUBSCRIPTION_PATH,
    CLAIM_PATH, EVENT_REPORT_PATH, PREVIEW_PATH, RESET_STATUS_PATH, RESET_USE_PATH,
    TOUCH_ACTION_PATH, TOUCH_PATH, ZAI_HOST, billing_request_headers, device_mid_for, jwt_sub,
    reset_extra_headers,
};
pub use mcp::{MCP_FORWARDED_HEADERS, MCP_SERVER_PATH_PREFIX, MCP_SERVERS};

use aikun_protocol::Protocol;
use serde_json::Value;

use crate::provider::Provider;

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

/// 免费窗口的小时边界(北京时间,含头不含尾):任意日 23:00 起、次日
/// 09:00 止;周六/周日全天在窗(见 [`is_off_peak`])。
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
         只勾选 Anthropic 协议,Base URL 填 https://zcode.z.ai。国内/国外账号\
         (z.ai / 智谱 BigModel)都打同一 API 域(实测 zcode.bigmodel.cn 的 API\
         全部 301 不可用);凭证互不通用,免费窗口 Flash 零计费"
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

    /// 请求体伪装(3012 内容检查的破解,炼化自 Zcode2Api normalizeBody;
    /// 2026-11-19 国内账号实测:缺失 → 405+3012 "client not supported"):
    /// 官方 3 个 system 身份块(23mac.har,嵌入 system.json;恰好第 4 块
    /// 会 3012)+ metadata.user_id 设备 JSON + string content 转 text
    /// block + 末块 cache_control ephemeral。客户端 system 文本拼进官方
    /// 最后一块(参考实现 splice 口径)。
    fn prepare_request(&self, _provider: &Provider, _protocol: Protocol, body: &mut Value) {
        normalize_body(body);
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
            // 会话族头(2026-11-19 国内账号实测必需:缺失 → 405):
            // query-source/session-type 静态;query-id/request-id 进程级
            // 随机(session-id 由上游按 device 聚合,同进程稳定即可)。
            ("x-zcode-query-source", "main_turn".to_string()),
            ("x-zcode-session-type", "main".to_string()),
            ("x-query-id", mint_hex_uuid(0x51ed_270b)),
            ("x-request-id", mint_hex_uuid(0x0fc1_ce77)),
        ]
    }
}

/// 进程级伪 UUID(时间熵 xorshift;本 crate 无 rand 依赖,上游对这两个
/// 头只要求非空 UUID 形态,进程内稳定即可——session 语义由 device 维聚)。
fn mint_hex_uuid(salt: u64) -> String {
    use std::sync::OnceLock;
    static CACHE: OnceLock<std::collections::HashMap<u64, String>> = OnceLock::new();
    let cache = CACHE.get_or_init(std::collections::HashMap::new);
    if let Some(v) = cache.get(&salt) {
        return v.clone();
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs().wrapping_mul(salt))
        .unwrap_or(0x9e37_79b9 ^ salt);
    let mut x = nanos.rotate_left(32) ^ salt;
    let hex: String = (0..32)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            format!("{:x}", x & 0xf)
        })
        .collect();
    format!(
        "{}-{}-4{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// 官方 system 身份块(23mac.har;恰 3 块——第 4 块 3012)。
const ZCODE_SYSTEM: &str = include_str!("system.json");

/// 会话/设备标识的确定性派生(FNV-1a;上游按 device 聚合 session,
/// 稳定即可,与参考 SessionIDFor 同语义)。
fn derived_hex(seed: &str, salt: u64) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ salt;
    for b in seed.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}{h:016x}")
}

fn normalize_body(body: &mut Value) {
    use serde_json::json;

    // 1) messages:string content → text block
    if let Some(msgs) = body["messages"].as_array_mut() {
        for m in msgs.iter_mut() {
            if let Some(text) = m["content"].as_str().map(str::to_string) {
                m["content"] = json!([{ "type": "text", "text": text }]);
            }
        }
    }

    // 2) system:官方 3 块;客户端 system 文本拼进最后一块文本尾
    let client_sys: Vec<String> = match &body["system"] {
        Value::String(t) if !t.trim().is_empty() => vec![t.trim().to_string()],
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .map(str::trim)
            .filter(|t| !t.is_empty() && !t.starts_with("x-anthropic-billing-header:"))
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    if let Ok(mut official) = serde_json::from_str::<Vec<Value>>(ZCODE_SYSTEM) {
        if !client_sys.is_empty()
            && let Some(last) = official.last_mut()
            && let Some(t) = last["text"].as_str().map(str::to_string)
        {
            last["text"] = Value::String(format!("{t}\n\n{}", client_sys.join("\n\n")));
        }
        body["system"] = Value::Array(official);
    }

    // 3) cache_control ephemeral:最后一条非 system 消息的最后一个
    //    content block
    if let Some(msgs) = body["messages"].as_array_mut()
        && let Some(last) = msgs.last_mut()
        && let Some(blocks) = last["content"].as_array_mut()
        && let Some(cb) = blocks.last_mut()
    {
        cb["cache_control"] = json!({ "type": "ephemeral" });
    }

    // 4) metadata.user_id = 设备 JSON(参考 23mac.har:字符串而非 sub)
    // 设备 ID 在此层拿不到 JWT(发送层才有);用进程级稳定派生
    let mid = mint_hex_uuid(0x6d69_6400);
    let sid = derived_hex(&mid, 0x7369_6400);
    body["metadata"] = json!({
        "user_id": format!(
            "{{\"device_id\":\"{mid}\",\"account_uuid\":\"\",\"session_id\":\"{sid}\"}}"
        )
    });
}

/// 绝对路径拼接:base 去尾斜杠后直接接 path(保留 base 自带 query 的场景
/// 不适用——zcode 的 base 是纯 origin)。
fn join_absolute(base_url: &str, path: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), path)
}

/// 北京时间(UTC+8)的(星期, 小时);星期 1=周一 … 7=周日。
/// 1970-01-01 是周四:day=0 → (0+3)%7+1 = 4 ✓。
fn beijing_weekday_hour(unix_now: i64) -> (u32, u32) {
    let beijing = unix_now + 8 * 3600;
    let day = beijing.div_euclid(86_400);
    let secs_into_day = beijing.rem_euclid(86_400);
    let weekday = (day + 3).rem_euclid(7) + 1;
    (weekday as u32, (secs_into_day / 3600) as u32)
}

/// 是否处于官方免费窗口(2026-11-19 用户口径,北京时间):
/// * **工作日窗口**:工作日晚 23:00 – 次日 09:00,免费额度 **3 亿**;
/// * **周末窗口**:周五 23:00 连续至周一 09:00(含周六/周日全天),**5 亿**。
///
/// 归一:h<9 或 h>=23 恒在窗(周一凌晨是周末窗口尾、周一 23 点起是工作日
/// 窗口);周六/周日全天在窗。
pub fn is_off_peak(unix_now: i64) -> bool {
    let (weekday, h) = beijing_weekday_hour(unix_now);
    !(OFF_PEAK_END_HOUR..OFF_PEAK_START_HOUR).contains(&h) || weekday >= 6
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

    // 北京时间指定(星期, 时, 分)的 unix 秒。锚:day 20_000 是周五
    // ((20000+3)%7+1 = 5),按星期偏移取同刻。
    fn unix_at_beijing(weekday: u32, hour: u32, minute: u32) -> i64 {
        assert!((1..=7).contains(&weekday), "weekday 1=周一…7=周日");
        let day: i64 = 20_000 + i64::from((weekday + 7 - 5) % 7);
        day * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 - 28_800
    }

    #[test]
    fn off_peak_window_truth_table() {
        // ---- 任意日:23:00(含)入窗、凌晨在窗 ----
        for wd in 1..=7 {
            assert!(is_off_peak(unix_at_beijing(wd, 23, 0)), "wd{wd} 23:00 入窗");
            assert!(is_off_peak(unix_at_beijing(wd, 3, 30)), "wd{wd} 03:30 在窗");
            assert!(is_off_peak(unix_at_beijing(wd, 8, 59)), "wd{wd} 08:59 在窗");
        }
        // ---- 工作日白天出窗(09:00–22:59)----
        for wd in 1..=5 {
            assert!(!is_off_peak(unix_at_beijing(wd, 9, 0)), "wd{wd} 09:00 出窗");
            assert!(
                !is_off_peak(unix_at_beijing(wd, 12, 0)),
                "wd{wd} 中午不在窗"
            );
            assert!(
                !is_off_peak(unix_at_beijing(wd, 22, 59)),
                "wd{wd} 22:59 未入窗"
            );
        }
        // ---- 工作日白天不在窗(工作日窗口 = 晚 23–09,3 亿) ----
        assert!(!is_off_peak(unix_at_beijing(1, 12, 0)), "周一中午");
        assert!(!is_off_peak(unix_at_beijing(3, 18, 0)), "周三傍晚");
        assert!(!is_off_peak(unix_at_beijing(5, 12, 0)), "周五中午");
        // ---- 周末全天在窗(周五 23 – 周一 09 连续,5 亿) ----
        assert!(is_off_peak(unix_at_beijing(6, 12, 0)), "周六中午");
        assert!(is_off_peak(unix_at_beijing(6, 18, 0)), "周六傍晚");
        assert!(is_off_peak(unix_at_beijing(7, 12, 0)), "周日中午");
        assert!(
            is_off_peak(unix_at_beijing(7, 23, 30)),
            "周日晚(仍在周末窗)"
        );
        assert!(is_off_peak(unix_at_beijing(1, 0, 0)), "周一凌晨(周末窗尾)");
        // 周五 23 点 = 周末窗口起点
        assert!(
            is_off_peak(unix_at_beijing(5, 23, 0)),
            "周五 23:00 周末窗口开始"
        );
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
        let night = unix_at_beijing(2, 2, 0);
        let day = unix_at_beijing(3, 14, 0);
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
        // 国内版:同一端点族,仅 base 不同(2026-11-18 双环境支持)
        assert_eq!(
            a.models_url("https://zcode.bigmodel.cn", Protocol::Anthropic),
            "https://zcode.bigmodel.cn/api/v1/zcode-plan/billing/balance?app_version=3.16.3"
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
}
