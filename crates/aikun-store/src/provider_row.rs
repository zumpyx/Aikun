//! Provider 的 SQLite 行映射与渠道拓扑读取(迁自二进制
//! `aikun/src/models/provider_row.rs`,plan 步骤 8)。
//!
//! `crate::models` 旧路径由二进制 re-export 保持稳定。

use aikun_channels::Provider;
use rusqlite::Connection;

use crate::{ChannelTopology, StoreError};

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
        // 出库时尚未解密,标记在选路选中该渠道后设置(密钥保留密文,
        // 选中后解一次,roadmap 决策 4)。
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

impl ChannelTopology for crate::pool::DbPool {
    fn active_channels(&self) -> Result<Vec<Provider>, StoreError> {
        let conn = self.read().lock().map_err(|_| StoreError::Unavailable)?;
        query_active_channels(&conn).map_err(|e| StoreError::Failed(e.to_string()))
    }
}

/// 全量启用渠道(密钥保持库内形态)。自由的 crate 内入口:快照重建与
/// 测试复用同一 SQL。
pub(crate) fn query_active_channels(conn: &Connection) -> Result<Vec<Provider>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PROVIDER_SELECT_COLUMNS} FROM providers WHERE is_active = 1"
    ))?;
    let providers = stmt
        .query_map([], row_to_provider)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(providers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::KeyCipher;

    fn pool() -> crate::pool::DbPool {
        crate::pool::DbPool::open(":memory:", KeyCipher::from_secret("test")).unwrap()
    }

    #[test]
    fn active_channels_returns_only_enabled_rows() {
        let pool = pool();
        let conn = pool.conn.lock().unwrap();
        for (id, active) in [("p-on", 1), ("p-off", 0)] {
            conn.execute(
                "INSERT INTO providers (id, name, provider_type, openai_base_url, api_key, models, is_active)
                 VALUES (?1, ?1, 'openai', 'http://x', 'k', '[\"m\"]', ?2)",
                rusqlite::params![id, active],
            )
            .unwrap();
        }
        drop(conn);
        let channels = ChannelTopology::active_channels(&pool).unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].id, "p-on");
        assert_eq!(channels[0].models, "[\"m\"]");
        // 密钥保持库内形态(明文原样兼容,密文不解)
        assert_eq!(channels[0].api_key, "k");
        assert!(!channels[0].key_decrypt_failed);
    }
}
