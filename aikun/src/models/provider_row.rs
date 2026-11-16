//! Provider 的 SQLite 行映射(rusqlite 耦合,留在二进制;crate::models 旧路径 re-export)。

use aikun_channels::Provider;

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
