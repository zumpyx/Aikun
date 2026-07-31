//! providers.api_key 的静态加密:AES-256-GCM。密文格式 `enc:v1:` +
//! base64(nonce(12B) ‖ ciphertext)。未带前缀的值按明文原样处理,兼容
//! 加密迁移完成前的旧行。
//!
//! 密钥来源:优先独立的 AIKUN_ENCRYPTION_KEY;未设置时回退为
//! SHA-256(AIKUN_JWT_SECRET) 派生(兼容既有部署)。注意:回退模式下
//! 更换 JWT secret 会使已加密的渠道 key 无法解密;固定
//! AIKUN_ENCRYPTION_KEY 后即可自由轮换 JWT secret,渠道 key 不受影响。

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use sha2::{Digest, Sha256};

pub const ENC_PREFIX: &str = "enc:v1:";

#[derive(Clone)]
pub struct KeyCipher {
    key: [u8; 32],
}

impl KeyCipher {
    /// 从部署密钥派生 256 位加密密钥(同一 secret 派生结果确定)。
    pub fn from_secret(secret: &str) -> Self {
        let digest = Sha256::digest(secret.as_bytes());
        let mut key = [0u8; 32];
        key.copy_from_slice(&digest);
        Self { key }
    }

    /// 部署入口:优先独立的 AIKUN_ENCRYPTION_KEY;未设置时回退为
    /// SHA-256(jwt_secret) 派生,与既有部署的密文保持兼容。
    pub fn from_config(config: &crate::config::AppConfig) -> Self {
        Self::from_secret(config.encryption_key.as_deref().unwrap_or(&config.jwt_secret))
    }

    pub fn encrypt(&self, plaintext: &str) -> String {
        let cipher = Aes256Gcm::new_from_slice(&self.key).expect("32-byte key");
        let mut nonce = [0u8; 12];
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut nonce);
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
            .expect("aes-gcm encrypt is infallible for in-memory inputs");
        let mut buf = Vec::with_capacity(nonce.len() + ct.len());
        buf.extend_from_slice(&nonce);
        buf.extend_from_slice(&ct);
        format!("{}{}", ENC_PREFIX, B64.encode(buf))
    }

    /// 解密带前缀的密文;密钥不匹配或数据损坏返回 None。
    pub fn decrypt(&self, stored: &str) -> Option<String> {
        let body = stored.strip_prefix(ENC_PREFIX)?;
        let raw = B64.decode(body).ok()?;
        if raw.len() <= 12 {
            return None;
        }
        let (nonce, ct) = raw.split_at(12);
        let cipher = Aes256Gcm::new_from_slice(&self.key).expect("32-byte key");
        let pt = cipher.decrypt(Nonce::from_slice(nonce), ct).ok()?;
        String::from_utf8(pt).ok()
    }
}

/// 读取侧统一入口:明文原样返回;密文解密失败时返回空串并告警(多半是
/// AIKUN_ENCRYPTION_KEY/AIKUN_JWT_SECRET 被换)。代理选路(selector)会
/// 另行标记解密失败的渠道并跳过,不会把空凭证发给上游。
pub fn decrypt_or_plain(cipher: &KeyCipher, stored: &str) -> String {
    if !stored.starts_with(ENC_PREFIX) {
        return stored.to_string();
    }
    match cipher.decrypt(stored) {
        Some(v) => v,
        None => {
            tracing::error!(
                "Failed to decrypt a provider api_key — AIKUN_JWT_SECRET changed since it was stored?"
            );
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let c = KeyCipher::from_secret("test-secret");
        let stored = c.encrypt("sk-upstream-123");
        assert!(stored.starts_with(ENC_PREFIX));
        assert!(!stored.contains("sk-upstream-123"));
        assert_eq!(c.decrypt(&stored).as_deref(), Some("sk-upstream-123"));
        // 每次加密随机 nonce,同明文密文不同
        assert_ne!(c.encrypt("sk-upstream-123"), stored);
    }

    #[test]
    fn wrong_key_fails_closed() {
        let a = KeyCipher::from_secret("secret-a");
        let b = KeyCipher::from_secret("secret-b");
        assert_eq!(b.decrypt(&a.encrypt("k")), None);
    }

    #[test]
    fn plaintext_passthrough() {
        let c = KeyCipher::from_secret("s");
        assert_eq!(decrypt_or_plain(&c, "plain-key"), "plain-key");
        assert_eq!(decrypt_or_plain(&c, ""), "");
    }

    #[test]
    fn corrupted_ciphertext_fails_closed() {
        let c = KeyCipher::from_secret("s");
        assert_eq!(c.decrypt("enc:v1:not-valid-base64!!"), None);
        assert_eq!(c.decrypt(&format!("{}{}", ENC_PREFIX, B64.encode([1u8, 2, 3]))), None);
    }

    #[test]
    fn from_config_prefers_encryption_key_and_falls_back_to_jwt() {
        let base = crate::config::AppConfig {
            jwt_secret: "jwt-secret".to_string(),
            ..Default::default()
        };
        // 缺省:与 from_secret(jwt_secret) 一致(能解既有密文)。
        let fallback = KeyCipher::from_config(&base);
        let legacy = KeyCipher::from_secret("jwt-secret");
        let stored = legacy.encrypt("k");
        assert_eq!(fallback.decrypt(&stored).as_deref(), Some("k"));

        // 设置独立密钥后:解得开新密文,解不开 JWT secret 派生的旧密文。
        let config = crate::config::AppConfig {
            encryption_key: Some("enc-key".to_string()),
            ..base
        };
        let dedicated = KeyCipher::from_config(&config);
        let stored2 = dedicated.encrypt("k");
        assert_eq!(dedicated.decrypt(&stored2).as_deref(), Some("k"));
        assert_eq!(dedicated.decrypt(&stored), None);
    }
}
