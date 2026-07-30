//! providers.api_key 的静态加密:AES-256-GCM,密钥由 AIKUN_JWT_SECRET
//! 经 SHA-256 派生。密文格式 `enc:v1:` + base64(nonce(12B) ‖ ciphertext)。
//! 未带前缀的值按明文原样处理,兼容加密迁移完成前的旧行。
//!
//! 注意:加密密钥派生自 JWT secret,更换 AIKUN_JWT_SECRET 会使已加密
//! 的渠道 key 无法解密(与 JWT 失效同理,属于部署级变更)。

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

/// 读取侧统一入口:明文原样返回;密文解密失败时返回空串并告警——
/// 上游会因空凭证 401,日志里能看到明确根因(多半是 JWT secret 被换)。
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
}
