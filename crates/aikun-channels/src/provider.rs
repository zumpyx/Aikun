use serde::{Deserialize, Serialize};

use aikun_protocol::Protocol;

#[derive(Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    /// OpenAI 协议上游地址。
    pub openai_base_url: String,
    /// Anthropic 协议上游地址;为空时回退 openai_base_url(见 base_url_for)。
    #[serde(default)]
    pub anthropic_base_url: String,
    #[serde(skip_serializing)]
    pub api_key: String,
    /// 出库解密失败的标记(enc:v1: 密文用当前密钥解不开,多半是
    /// AIKUN_ENCRYPTION_KEY/AIKUN_JWT_SECRET 被换)。仅内存使用:不进库;
    /// attempt_loop 据此跳过该渠道,避免空凭证打到上游吃 401。
    #[serde(default, skip_serializing)]
    pub key_decrypt_failed: bool,
    pub models: String, // JSON array
    pub priority: i32,
    pub weight: f64,
    pub is_active: bool,
    pub health_status: String,
    pub latency_ms: f64,
    pub error_rate: f64,
    pub last_health_check: Option<String>,
    pub max_retries: i32,
    pub timeout_secs: i32,
    #[serde(default)]
    pub proxy_url: String,
    #[serde(default)]
    pub model_mapping: String, // JSON object: {"requested_model": "upstream_model"}
    #[serde(default)]
    pub consecutive_failures: i32,
    #[serde(default)]
    pub disabled_reason: String,
    /// Wire protocols this channel can speak, JSON array of
    /// "openai"/"anthropic" (e.g. ["openai","anthropic"]).
    #[serde(default)]
    pub protocols: String,
    /// Protocol used for upstream traffic by default.
    #[serde(default)]
    pub default_protocol: String,
    /// 渠道备注:同一供应商多个帐号时用于区分。
    #[serde(default)]
    pub note: String,
    /// 渠道官网地址,仅用于后台展示。
    #[serde(default)]
    pub website_url: String,
    /// 渠道类型(适配器 kind,见 crate::channel::adapter)。空串 = generic。
    #[serde(default)]
    pub channel_kind: String,
    /// 余额查询地址。空 = 该渠道不支持查余额。
    #[serde(default)]
    pub balance_url: String,
    pub created_at: String,
    pub updated_at: String,
}

// api_key 不出现在 Debug 输出中。
impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provider")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("provider_type", &self.provider_type)
            .field("openai_base_url", &self.openai_base_url)
            .field("anthropic_base_url", &self.anthropic_base_url)
            .field("api_key", &"***")
            .field("models", &self.models)
            .field("priority", &self.priority)
            .field("weight", &self.weight)
            .field("is_active", &self.is_active)
            .field("health_status", &self.health_status)
            .field("latency_ms", &self.latency_ms)
            .field("error_rate", &self.error_rate)
            .field("last_health_check", &self.last_health_check)
            .field("max_retries", &self.max_retries)
            .field("timeout_secs", &self.timeout_secs)
            .field("proxy_url", &self.proxy_url)
            .field("model_mapping", &self.model_mapping)
            .field("consecutive_failures", &self.consecutive_failures)
            .field("disabled_reason", &self.disabled_reason)
            .field("protocols", &self.protocols)
            .field("default_protocol", &self.default_protocol)
            .field("note", &self.note)
            .field("website_url", &self.website_url)
            .field("channel_kind", &self.channel_kind)
            .field("balance_url", &self.balance_url)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// Wire protocols a channel may declare support for.
pub const VALID_PROTOCOLS: [&str; 3] = ["openai", "anthropic", "responses"];

/// Protocols eligible as a channel's default (upstream traffic protocol).
/// responses 与 openai / anthropic 同级，可单独成渠道（DeepSeek 等）。
pub const DEFAULTABLE_PROTOCOLS: [&str; 3] = ["openai", "anthropic", "responses"];

/// A channel's declared protocol list must be non-empty and only contain
/// known protocols.
pub fn valid_protocol_list(list: &[String]) -> bool {
    !list.is_empty() && list.iter().all(|p| VALID_PROTOCOLS.contains(&p.as_str()))
}

/// responses 复用 openai 地址与 Bearer 鉴权，可单独成渠道。
pub fn has_defaultable_protocol(list: &[String]) -> bool {
    list.iter()
        .any(|p| DEFAULTABLE_PROTOCOLS.contains(&p.as_str()))
}

/// default_protocol 合法性:必须已勾选,且属于可作默认的协议。
pub fn valid_default_protocol(default: &str, protocols: &[String]) -> bool {
    DEFAULTABLE_PROTOCOLS.contains(&default) && protocols.iter().any(|p| p == default)
}

/// 协议列表中第一个可作默认的协议(调用前须经 has_defaultable_protocol 校验)。
pub fn first_defaultable_protocol(protocols: &[String]) -> String {
    protocols
        .iter()
        .find(|p| DEFAULTABLE_PROTOCOLS.contains(&p.as_str()))
        .cloned()
        .unwrap_or_else(|| "openai".to_string())
}

/// The protocol used to talk to this channel upstream: its configured
/// default, falling back to the legacy provider_type mapping.
pub fn channel_protocol(p: &Provider) -> Protocol {
    match Protocol::parse(&p.default_protocol) {
        Some(proto) => proto,
        None => Protocol::for_provider_type(&p.provider_type),
    }
}

/// 决定与上游通话的协议:客户端为 Responses API 且渠道声明支持 responses
/// 时原生直通;否则回退渠道默认协议,由 convert 层做协议转换降级。
/// responses 可作默认协议。
pub fn upstream_protocol_for(p: &Provider, client_protocol: Protocol) -> Protocol {
    if client_protocol == Protocol::Responses
        && channel_protocols(p)
            .iter()
            .any(|x| x == Protocol::Responses.as_str())
    {
        return Protocol::Responses;
    }
    channel_protocol(p)
}

/// 按上游协议选择对应的 base URL:有些供应商对 OpenAI / Anthropic 两种协议
/// 提供不同路由。协议专属地址为空时回退另一个,保证只配了一个地址的
/// 渠道照常工作。
pub fn base_url_for(p: &Provider, protocol: Protocol) -> &str {
    pick_base_url(protocol, &p.openai_base_url, &p.anthropic_base_url)
}

/// [`base_url_for`] 的拆列版本:健康检查 / 管理端单测等只查了两个地址列、
/// 没有完整 Provider 的路径共用同一回退口径。
pub fn pick_base_url<'a>(
    protocol: Protocol,
    openai_url: &'a str,
    anthropic_url: &'a str,
) -> &'a str {
    let (primary, fallback) = match protocol {
        Protocol::Anthropic => (anthropic_url, openai_url),
        // Responses 走 OpenAI 地址(同一家 OpenAI 兼容上游的 /responses 路由)。
        Protocol::OpenAi | Protocol::Responses => (openai_url, anthropic_url),
    };
    if primary.is_empty() {
        fallback
    } else {
        primary
    }
}

/// Parse the channel's supported protocol list, with the legacy
/// provider_type fallback for unmigrated rows.
pub fn channel_protocols(p: &Provider) -> Vec<String> {
    let list: Vec<String> = serde_json::from_str(&p.protocols).unwrap_or_default();
    if valid_protocol_list(&list) {
        return list;
    }
    vec![
        Protocol::for_provider_type(&p.provider_type)
            .as_str()
            .to_string(),
    ]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn bare_provider() -> Provider {
        Provider {
            id: "p".into(),
            name: "p".into(),
            provider_type: "openai".into(),
            openai_base_url: "http://x".into(),
            anthropic_base_url: String::new(),
            api_key: "k".into(),
            key_decrypt_failed: false,
            models: "[]".into(),
            priority: 0,
            weight: 1.0,
            is_active: true,
            health_status: "healthy".into(),
            latency_ms: 0.0,
            error_rate: 0.0,
            last_health_check: None,
            max_retries: 3,
            timeout_secs: 120,
            proxy_url: String::new(),
            model_mapping: String::new(),
            consecutive_failures: 0,
            disabled_reason: String::new(),
            protocols: String::new(),
            default_protocol: String::new(),
            note: String::new(),
            website_url: String::new(),
            channel_kind: String::new(),
            balance_url: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn protocol_list_validation() {
        assert!(!valid_protocol_list(&[]));
        assert!(valid_protocol_list(&["openai".to_string()]));
        assert!(valid_protocol_list(&[
            "openai".to_string(),
            "anthropic".to_string()
        ]));
        assert!(!valid_protocol_list(&["azure".to_string()]));
    }

    #[test]
    fn base_url_for_picks_protocol_url_with_fallback() {
        let mut p = bare_provider();
        // 未配 anthropic 地址时两种协议都落到 openai 地址
        assert_eq!(base_url_for(&p, Protocol::OpenAi), "http://x");
        assert_eq!(base_url_for(&p, Protocol::Anthropic), "http://x");
        // 配了独立的 anthropic 地址后按协议分开
        p.anthropic_base_url = "http://y".into();
        assert_eq!(base_url_for(&p, Protocol::Anthropic), "http://y");
        assert_eq!(base_url_for(&p, Protocol::OpenAi), "http://x");
        // openai 地址为空时回退 anthropic 地址(anthropic-only 渠道)
        p.openai_base_url = String::new();
        assert_eq!(base_url_for(&p, Protocol::OpenAi), "http://y");
    }

    #[test]
    fn legacy_rows_fall_back_to_provider_type() {
        let mut p = bare_provider();
        assert_eq!(channel_protocol(&p), Protocol::OpenAi);
        assert_eq!(channel_protocols(&p), vec!["openai".to_string()]);
        p.provider_type = "anthropic".into();
        assert_eq!(channel_protocol(&p), Protocol::Anthropic);
        assert_eq!(channel_protocols(&p), vec!["anthropic".to_string()]);
    }

    #[test]
    fn configured_protocols_win() {
        let mut p = bare_provider();
        p.protocols = "[\"openai\",\"anthropic\"]".into();
        p.default_protocol = "anthropic".into();
        assert_eq!(channel_protocol(&p), Protocol::Anthropic);
        assert_eq!(
            channel_protocols(&p),
            vec!["openai".to_string(), "anthropic".to_string()]
        );
        // Garbage in the JSON column falls back to provider_type.
        p.protocols = "not-json".into();
        assert_eq!(channel_protocols(&p), vec!["openai".to_string()]);
    }

    #[test]
    fn upstream_protocol_prefers_native_responses() {
        let mut p = bare_provider();
        // 未声明 responses:responses 客户端走渠道默认协议(转换降级)
        assert_eq!(
            upstream_protocol_for(&p, Protocol::Responses),
            Protocol::OpenAi
        );
        // 声明后:responses 客户端原生直通
        p.protocols = "[\"openai\",\"responses\"]".into();
        assert_eq!(
            upstream_protocol_for(&p, Protocol::Responses),
            Protocol::Responses
        );
        // 其他客户端协议不受 responses 声明影响
        assert_eq!(
            upstream_protocol_for(&p, Protocol::OpenAi),
            Protocol::OpenAi
        );
        // anthropic 默认协议渠道:responses 客户端也走 anthropic(组合转换)
        p.default_protocol = "anthropic".into();
        p.protocols = "[\"anthropic\"]".into();
        assert_eq!(
            upstream_protocol_for(&p, Protocol::Responses),
            Protocol::Anthropic
        );
    }

    #[test]
    fn defaultable_protocol_rules() {
        let openai = || "openai".to_string();
        let responses = || "responses".to_string();
        let anthropic = || "anthropic".to_string();

        // responses 可单独成渠道，也可作默认协议
        assert!(has_defaultable_protocol(&[responses()]));
        assert!(valid_default_protocol(
            "responses",
            &[responses()]
        ));
        assert!(valid_default_protocol("openai", &[openai(), responses()]));
        // 默认必须已勾选
        assert!(!valid_default_protocol(
            "anthropic",
            &[openai(), responses()]
        ));
        // 兜底取第一个可作默认的协议
        assert_eq!(
            first_defaultable_protocol(&[responses(), anthropic()]),
            "anthropic"
        );
        assert_eq!(first_defaultable_protocol(&[openai()]), "openai");
    }
}
