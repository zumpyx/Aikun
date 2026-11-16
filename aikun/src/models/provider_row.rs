//! Provider 的 SQLite 行映射(rusqlite 耦合,留在二进制;crate::models 旧路径 re-export)。

use aikun_channels::{Provider, channel_protocol, channel_protocols};
use serde::{Deserialize, Serialize};

/// providers 表标准查询列(与 [`row_to_provider`] 的下标一一对应)。
pub const PROVIDER_SELECT_COLUMNS: &str = "id, name, provider_type, openai_base_url, anthropic_base_url, api_key, models, priority, weight,
                    is_active, health_status, latency_ms, error_rate, last_health_check,
                    max_retries, timeout_secs, created_at, updated_at, proxy_url,
                    model_mapping, consecutive_failures, disabled_reason,
                    protocols, default_protocol, note, website_url, channel_kind";

pub fn row_to_provider(row: &rusqlite::Row) -> rusqlite::Result<Provider> {
    Ok(Provider {
        id: row.get(0)?,
        name: row.get(1)?,
        provider_type: row.get(2)?,
        openai_base_url: row.get(3)?,
        anthropic_base_url: row.get(4)?,
        api_key: row.get(5)?,
        // 出库时尚未解密,标记在 selector 的解密步骤里设置。
        key_decrypt_failed: false,
        models: row.get(6)?,
        priority: row.get(7)?,
        weight: row.get(8)?,
        is_active: row.get(9)?,
        health_status: row.get(10)?,
        latency_ms: row.get(11)?,
        error_rate: row.get(12)?,
        last_health_check: row.get(13)?,
        max_retries: row.get(14)?,
        timeout_secs: row.get(15)?,
        created_at: row.get(16)?,
        updated_at: row.get(17)?,
        proxy_url: row.get(18)?,
        model_mapping: row.get(19)?,
        consecutive_failures: row.get(20)?,
        disabled_reason: row.get(21)?,
        protocols: row.get(22)?,
        default_protocol: row.get(23)?,
        note: row.get(24)?,
        website_url: row.get(25)?,
        channel_kind: row.get(26)?,
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateProviderRequest {
    pub name: String,
    pub provider_type: Option<String>,
    pub openai_base_url: Option<String>,
    pub anthropic_base_url: Option<String>,
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
    pub note: Option<String>,
    pub website_url: Option<String>,
    /// 渠道类型(适配器 kind);缺省/空串 = generic。
    pub channel_kind: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateProviderRequest {
    pub name: Option<String>,
    pub provider_type: Option<String>,
    pub openai_base_url: Option<String>,
    pub anthropic_base_url: Option<String>,
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
    pub note: Option<String>,
    pub website_url: Option<String>,
    /// 渠道类型(适配器 kind);缺省/空串 = generic。
    pub channel_kind: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProviderResponse {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub openai_base_url: String,
    pub anthropic_base_url: String,
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
    pub note: String,
    pub website_url: String,
    pub channel_kind: String,
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
            openai_base_url: p.openai_base_url,
            anthropic_base_url: p.anthropic_base_url,
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
            note: p.note,
            website_url: p.website_url,
            channel_kind: p.channel_kind,
            created_at: p.created_at,
            updated_at: p.updated_at,
        }
    }
}
