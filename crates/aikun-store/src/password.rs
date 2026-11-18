//! 口令与 API key 的落库哈希(迁自二进制 `aikun/src/auth/mod.rs`,plan
//! 步骤 8):凭据的"存储形态"归仓储层;二进制 `crate::auth` 保留旧路径
//! re-export,登录/改密链路行为不变。
//!
//! * 用户口令:Argon2id(PHC 字符串落库)。
//! * API key:hex(sha256(key))——明文只在创建时返回一次,入库与查询
//!   一律用哈希(未迁移的旧明文 key 以 "sk-" 开头,由 schema 迁移兜底)。

use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use sha2::{Digest, Sha256};

pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    // rand 0.9 provides the RNG; argon2's re-exported rand_core has no
    // getrandom feature enabled in this dependency tree.
    let mut salt_bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes)?;
    let argon2 = Argon2::default();
    let hash = argon2.hash_password(password.as_bytes(), &salt)?;
    Ok(hash.to_string())
}

pub fn verify_password(password: &str, hash: &str) -> Result<bool, argon2::password_hash::Error> {
    let parsed_hash = PasswordHash::new(hash)?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok())
}

/// 16-char alphanumeric password from the CSPRNG (used for seeded admin and
/// batch-created users without an explicit password).
pub fn generate_random_password() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
    let mut rng = rand::rng();
    (0..16)
        .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
        .collect()
}

/// API key 的存储形式:hex(sha256(key))。
pub fn hash_api_key(key: &str) -> String {
    format!("{:x}", Sha256::digest(key.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_roundtrip_and_reject() {
        let hash = hash_password("correct horse").unwrap();
        assert!(verify_password("correct horse", &hash).unwrap());
        assert!(!verify_password("wrong", &hash).unwrap());
        // 非 PHC 字符串是错误而非 false
        assert!(verify_password("x", "not-a-hash").is_err());
    }

    #[test]
    fn random_passwords_unique_and_charset() {
        let p1 = generate_random_password();
        let p2 = generate_random_password();
        assert_eq!(p1.len(), 16);
        assert_ne!(p1, p2);
        assert!(p1.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn api_key_hash_stable_and_distinct() {
        let h = hash_api_key("sk-test");
        assert_eq!(h.len(), 64); // sha256 hex
        assert_eq!(h, hash_api_key("sk-test"));
        assert_ne!(h, hash_api_key("sk-other"));
    }
}
