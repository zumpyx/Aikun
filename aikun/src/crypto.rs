//! providers.api_key 静态加密的实现已吸收进 `aikun-store`(plan 步骤 8);
//! 本文件保留 `crate::crypto::*` 旧路径与配置适配
//! (AppConfig 是二进制类型,不进存储 crate)。

pub use aikun_store::crypto::{ENC_PREFIX, KeyCipher, decrypt_or_plain};

/// 部署入口:优先独立的 AIKUN_ENCRYPTION_KEY;未设置时回退为
/// SHA-256(jwt_secret) 派生,与既有部署的密文保持兼容。
pub fn cipher_from_config(config: &crate::config::AppConfig) -> KeyCipher {
    KeyCipher::from_secret(
        config
            .encryption_key
            .as_deref()
            .unwrap_or(&config.jwt_secret),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn cipher_from_config_prefers_encryption_key_and_falls_back_to_jwt() {
        let base = AppConfig {
            jwt_secret: "jwt-secret".to_string(),
            ..Default::default()
        };
        // 缺省:与 from_secret(jwt_secret) 一致(能解既有密文)。
        let fallback = cipher_from_config(&base);
        let legacy = KeyCipher::from_secret("jwt-secret");
        let stored = legacy.encrypt("k");
        assert_eq!(fallback.decrypt(&stored).as_deref(), Some("k"));

        // 设置独立密钥后:解得开新密文,解不开 JWT secret 派生的旧密文。
        let config = AppConfig {
            encryption_key: Some("enc-key".to_string()),
            ..base
        };
        let dedicated = cipher_from_config(&config);
        let stored2 = dedicated.encrypt("k");
        assert_eq!(dedicated.decrypt(&stored2).as_deref(), Some("k"));
        assert_eq!(dedicated.decrypt(&stored), None);
    }
}
