use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    #[serde(skip_serializing)]
    pub api_key: String,
    pub models: String,  // JSON array
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
            .field("base_url", &self.base_url)
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
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// Wire protocols a channel may declare support for.
pub const VALID_PROTOCOLS: [&str; 2] = ["openai", "anthropic"];

/// A channel's declared protocol list must be non-empty and only contain
/// known protocols.
pub fn valid_protocol_list(list: &[String]) -> bool {
    !list.is_empty() && list.iter().all(|p| VALID_PROTOCOLS.contains(&p.as_str()))
}

/// The protocol used to talk to this channel upstream: its configured
/// default, falling back to the legacy provider_type mapping.
pub fn channel_protocol(p: &Provider) -> &'static str {
    match p.default_protocol.as_str() {
        "anthropic" => crate::proxy::convert::PROTOCOL_ANTHROPIC,
        "openai" => crate::proxy::convert::PROTOCOL_OPENAI,
        _ => crate::proxy::convert::provider_protocol(&p.provider_type),
    }
}

/// Parse the channel's supported protocol list, with the legacy
/// provider_type fallback for unmigrated rows.
pub fn channel_protocols(p: &Provider) -> Vec<String> {
    let list: Vec<String> = serde_json::from_str(&p.protocols).unwrap_or_default();
    if valid_protocol_list(&list) {
        return list;
    }
    vec![crate::proxy::convert::provider_protocol(&p.provider_type).to_string()]
}

/// Map a row of the standard providers-table select order (id, name,
/// provider_type, base_url, api_key, models, priority, weight, is_active,
/// health_status, latency_ms, error_rate, last_health_check, max_retries,
/// timeout_secs, created_at, updated_at, proxy_url, model_mapping,
/// consecutive_failures, disabled_reason, protocols, default_protocol) to a
/// Provider.
pub fn row_to_provider(row: &rusqlite::Row) -> rusqlite::Result<Provider> {
    Ok(Provider {
        id: row.get(0)?,
        name: row.get(1)?,
        provider_type: row.get(2)?,
        base_url: row.get(3)?,
        api_key: row.get(4)?,
        models: row.get(5)?,
        priority: row.get(6)?,
        weight: row.get(7)?,
        is_active: row.get(8)?,
        health_status: row.get(9)?,
        latency_ms: row.get(10)?,
        error_rate: row.get(11)?,
        last_health_check: row.get(12)?,
        max_retries: row.get(13)?,
        timeout_secs: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
        proxy_url: row.get(17)?,
        model_mapping: row.get(18)?,
        consecutive_failures: row.get(19)?,
        disabled_reason: row.get(20)?,
        protocols: row.get(21)?,
        default_protocol: row.get(22)?,
    })
}

#[derive(Debug, Deserialize)]
pub struct CreateProviderRequest {
    pub name: String,
    pub provider_type: Option<String>,
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    pub models: Vec<String>,
    pub priority: Option<i32>,
    pub weight: Option<f64>,
    pub max_retries: Option<i32>,
    pub timeout_secs: Option<i32>,
    pub proxy_url: Option<String>,
    pub model_mapping: Option<std::collections::HashMap<String, String>>,
    pub protocols: Option<Vec<String>>,
    pub default_protocol: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateProviderRequest {
    pub name: Option<String>,
    pub provider_type: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub models: Option<Vec<String>>,
    pub priority: Option<i32>,
    pub weight: Option<f64>,
    pub is_active: Option<bool>,
    pub max_retries: Option<i32>,
    pub timeout_secs: Option<i32>,
    pub proxy_url: Option<String>,
    pub model_mapping: Option<std::collections::HashMap<String, String>>,
    pub protocols: Option<Vec<String>>,
    pub default_protocol: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProviderResponse {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    pub models: Vec<String>,
    pub priority: i32,
    pub weight: f64,
    pub is_active: bool,
    pub health_status: String,
    pub latency_ms: f64,
    pub error_rate: f64,
    pub last_health_check: Option<String>,
    pub max_retries: i32,
    pub timeout_secs: i32,
    pub proxy_url: String,
    pub model_mapping: std::collections::HashMap<String, String>,
    pub disabled_reason: String,
    pub protocols: Vec<String>,
    pub default_protocol: String,
    pub created_at: String,
    pub updated_at: String,
}

impl From<Provider> for ProviderResponse {
    fn from(p: Provider) -> Self {
        let models: Vec<String> = serde_json::from_str(&p.models).unwrap_or_default();
        let model_mapping: std::collections::HashMap<String, String> =
            serde_json::from_str(&p.model_mapping).unwrap_or_default();
        let protocols = channel_protocols(&p);
        let default_protocol = channel_protocol(&p).to_string();
        Self {
            id: p.id,
            name: p.name,
            provider_type: p.provider_type,
            base_url: p.base_url,
            models,
            priority: p.priority,
            weight: p.weight,
            is_active: p.is_active,
            health_status: p.health_status,
            latency_ms: p.latency_ms,
            error_rate: p.error_rate,
            last_health_check: p.last_health_check,
            max_retries: p.max_retries,
            timeout_secs: p.timeout_secs,
            proxy_url: p.proxy_url,
            model_mapping,
            disabled_reason: p.disabled_reason,
            protocols,
            default_protocol,
            created_at: p.created_at,
            updated_at: p.updated_at,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn bare_provider() -> Provider {
        Provider {
            id: "p".into(),
            name: "p".into(),
            provider_type: "openai".into(),
            base_url: "http://x".into(),
            api_key: "k".into(),
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
    fn legacy_rows_fall_back_to_provider_type() {
        let mut p = bare_provider();
        assert_eq!(channel_protocol(&p), "openai");
        assert_eq!(channel_protocols(&p), vec!["openai".to_string()]);
        p.provider_type = "anthropic".into();
        assert_eq!(channel_protocol(&p), "anthropic");
        assert_eq!(channel_protocols(&p), vec!["anthropic".to_string()]);
    }

    #[test]
    fn configured_protocols_win() {
        let mut p = bare_provider();
        p.protocols = "[\"openai\",\"anthropic\"]".into();
        p.default_protocol = "anthropic".into();
        assert_eq!(channel_protocol(&p), "anthropic");
        assert_eq!(
            channel_protocols(&p),
            vec!["openai".to_string(), "anthropic".to_string()]
        );
        // Garbage in the JSON column falls back to provider_type.
        p.protocols = "not-json".into();
        assert_eq!(channel_protocols(&p), vec!["openai".to_string()]);
    }
}
