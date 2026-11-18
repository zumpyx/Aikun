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

pub mod billing;
pub mod mcp;

pub use billing::{
    BILLING_CLIENT_VERSION, BILLING_OS_VERSION, CLAIM_PATH, EVENT_REPORT_PATH, PREVIEW_PATH,
    TOUCH_ACTION_PATH, TOUCH_PATH, billing_request_headers, device_mid_for, jwt_sub,
};
pub use mcp::{MCP_FORWARDED_HEADERS, MCP_SERVER_PATH_PREFIX, MCP_SERVERS};

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
         只勾选 Anthropic 协议。国内/国外双环境:国际(Z.ai 账号)Base 填\
         https://zcode.z.ai;国内(智谱 BigModel 账号)Base 填\
         https://zcode.bigmodel.cn(域名实测存在,路径族同构),两侧凭证互不通用。\
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
