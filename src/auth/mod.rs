use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};

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

use axum::{
    extract::{FromRequestParts, Request, State},
    http::request::Parts,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

/// API key 的存储形式:hex(sha256(key))。明文只在创建时返回一次,
/// 入库与查询一律用哈希(未迁移的旧明文 key 以 "sk-" 开头,单独兜底)。
pub fn hash_api_key(key: &str) -> String {
    format!("{:x}", Sha256::digest(key.as_bytes()))
}

use crate::config::AppConfig;
use crate::AppState;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    pub sub: String,
    pub username: String,
    pub role: String,
    pub exp: usize,
    pub iat: usize,
    /// 改密时递增;与 DB 不一致即拒绝,使旧 JWT 失效。旧 token 无此字段,默认为 0。
    #[serde(default)]
    pub token_version: i64,
}

/// Allow Claims to be extracted from request extensions (injected by auth middleware).
impl<S> FromRequestParts<S> for Claims
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Claims>().cloned().ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "unauthorized", "message": "Not authenticated"})),
            )
        })
    }
}

pub fn create_token(
    user_id: &str,
    username: &str,
    role: &str,
    token_version: i64,
    config: &AppConfig,
) -> Result<String, jsonwebtoken::errors::Error> {
    let now = chrono::Utc::now();
    let claims = Claims {
        sub: user_id.to_string(),
        username: username.to_string(),
        role: role.to_string(),
        iat: now.timestamp() as usize,
        exp: (now + chrono::Duration::seconds(config.jwt_expires_in)).timestamp() as usize,
        token_version,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(config.jwt_secret.as_bytes()),
    )
}

pub fn validate_token(token: &str, config: &AppConfig) -> Result<Claims, jsonwebtoken::errors::Error> {
    let token_data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(config.jwt_secret.as_bytes()),
        &Validation::default(),
    )?;
    Ok(token_data.claims)
}

/// Extra auth info injected alongside Claims when the request is
/// authenticated with an API key (sk-...) instead of a JWT.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub api_key_id: Option<String>,
    /// Model whitelist of the API key; None = all models allowed.
    pub allowed_models: Option<Vec<String>>,
}

impl<S> FromRequestParts<S> for AuthContext
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<AuthContext>()
            .cloned()
            .unwrap_or(AuthContext {
                api_key_id: None,
                allowed_models: None,
            }))
    }
}

/// Parse an expiry timestamp in common formats (RFC3339, datetime-local, date).
pub fn parse_expiry(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&chrono::Utc));
    }
    for fmt in ["%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(naive.and_utc());
        }
    }
    // Date-only input means midnight UTC (NaiveDateTime cannot parse it).
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return date.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc());
    }
    None
}

/// Look up an API key and build claims for its owner.
/// Returns (claims, auth_context) on success.
/// 入库的是 sha256 哈希;旧明文 key(以 "sk-" 开头)按明文比对兜底,待迁移。
/// 只在 blocking 线程池中调用(见 auth_middleware),不在 executor 上持锁。
fn authenticate_api_key(pool: &crate::db::DbPool, key: &str) -> Option<(Claims, AuthContext)> {
    if key.is_empty() {
        return None;
    }
    // 纯查询走只读连接(每请求热路径);last_used_at 节流写走写连接。
    let conn = pool.read().lock().ok()?;
    let row = conn
        .query_row(
            "SELECT k.id, k.user_id, k.is_active, k.expires_at, k.models,
                    u.username, u.role, u.is_active
             FROM api_keys k JOIN users u ON u.id = k.user_id
             WHERE k.key = ?1 OR k.key = ?2",
            rusqlite::params![hash_api_key(key), key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, bool>(7)?,
                ))
            },
        )
        .ok()?;

    let (key_id, user_id, key_active, expires_at, models_json, username, role, user_active) = row;

    if !key_active || !user_active {
        return None;
    }
    if let Some(exp) = expires_at.as_deref() {
        match parse_expiry(exp) {
            Some(t) if t > chrono::Utc::now() => {}
            _ => return None, // expired or unparseable
        }
    }

    // 空串表示不限制;JSON 损坏时 fail-closed 拒绝认证(而不是当作全放行)。
    let allowed_models = if models_json.trim().is_empty() {
        None
    } else {
        match serde_json::from_str::<Vec<String>>(&models_json) {
            Ok(allowed) if !allowed.is_empty() => Some(allowed),
            Ok(_) => None,
            Err(_) => return None,
        }
    };

    // 节流:距上次写入不足 60s 就不更新,避免每次请求都写库。
    // 写操作换写连接;读锁先释放,避免读写两锁并持。
    drop(conn);
    if let Ok(wconn) = pool.conn.lock() {
        let _ = wconn.execute(
            "UPDATE api_keys SET last_used_at = datetime('now')
             WHERE id = ?1 AND (last_used_at IS NULL OR last_used_at <= datetime('now', '-60 seconds'))",
            rusqlite::params![key_id],
        );
    }

    let now = chrono::Utc::now().timestamp() as usize;
    let claims = Claims {
        sub: user_id,
        username,
        role,
        // API key requests carry no token expiry; claims exp is informational.
        exp: now,
        iat: now,
        token_version: 0,
    };
    Some((
        claims,
        AuthContext {
            api_key_id: Some(key_id),
            allowed_models,
        },
    ))
}

/// Re-check a JWT-authenticated user against the database: disabled users are
/// rejected, and the live role from the DB overrides the (possibly stale) role
/// embedded in the token.
fn authenticate_jwt(pool: &crate::db::DbPool, mut claims: Claims) -> Option<(Claims, AuthContext)> {
    let conn = pool.read().lock().ok()?;
    let (role, is_active, token_version) = conn
        .query_row(
            "SELECT role, is_active, token_version FROM users WHERE id = ?1",
            rusqlite::params![claims.sub],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .ok()?;
    // 改密后 token_version 递增,旧 JWT 随之失效。
    if !is_active || token_version != claims.token_version {
        return None;
    }
    claims.role = role;
    Some((
        claims,
        AuthContext {
            api_key_id: None,
            allowed_models: None,
        },
    ))
}

/// Auth middleware — validates JWT or API key from Authorization header.
/// Injects Claims and AuthContext into request extensions for downstream handlers.
/// Uses `from_fn_with_state` to access AppState.
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let config = &state.config;

    let auth_header = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    match auth_header {
        Some(header) if header.starts_with("Bearer ") => {
            let token = &header[7..];
            // Try JWT first, then fall back to API key lookup.
            // API keys are only valid for /v1/* relay endpoints, not management APIs.
            // DB 访问全部派发到 blocking 线程池,不在 async executor 上持锁。
            let authed = match validate_token(token, config) {
                Ok(claims) => {
                    let pool = state.pool.clone();
                    tokio::task::spawn_blocking(move || authenticate_jwt(&pool, claims))
                        .await
                        .ok()
                        .flatten()
                }
                Err(_) if !req.uri().path().starts_with("/api/") => {
                    let pool = state.pool.clone();
                    let key = token.to_string();
                    tokio::task::spawn_blocking(move || authenticate_api_key(&pool, &key))
                        .await
                        .ok()
                        .flatten()
                }
                Err(_) => None,
            };
            match authed {
                Some((claims, ctx)) => {
                    req.extensions_mut().insert(claims);
                    req.extensions_mut().insert(ctx);
                    next.run(req).await
                }
                None => (StatusCode::UNAUTHORIZED, Json(json!({
                    "error": "invalid_token",
                    "message": "Invalid or expired token"
                }))).into_response(),
            }
        }
        // Anthropic clients send the key as `x-api-key` instead of a Bearer
        // token; accept it on the relay endpoints (never on management APIs).
        _ if !req.uri().path().starts_with("/api/") => {
            let key = req
                .headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            // 空 key 直接拒绝,不查库。
            let authed = if key.is_empty() {
                None
            } else {
                let pool = state.pool.clone();
                let key = key.to_string();
                tokio::task::spawn_blocking(move || authenticate_api_key(&pool, &key))
                    .await
                    .ok()
                    .flatten()
            };
            match authed {
                Some((claims, ctx)) => {
                    req.extensions_mut().insert(claims);
                    req.extensions_mut().insert(ctx);
                    next.run(req).await
                }
                None => (StatusCode::UNAUTHORIZED, Json(json!({
                    "error": "unauthorized",
                    "message": "Missing or invalid Authorization header"
                }))).into_response(),
            }
        }
        _ => {
            (StatusCode::UNAUTHORIZED, Json(json!({
                "error": "unauthorized",
                "message": "Missing or invalid Authorization header"
            }))).into_response()
        }
    }
}

/// Require admin role — run after auth middleware.
pub async fn require_admin(
    req: Request,
    next: Next,
) -> Response {
    let claims = req.extensions().get::<Claims>();

    match claims {
        Some(claims) if claims.role == "admin" => {
            next.run(req).await
        }
        Some(_) => {
            (StatusCode::FORBIDDEN, Json(json!({
                "error": "forbidden",
                "message": "Admin access required"
            }))).into_response()
        }
        None => {
            (StatusCode::UNAUTHORIZED, Json(json!({
                "error": "unauthorized",
                "message": "Authentication required"
            }))).into_response()
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_roundtrip() {
        let hash = hash_password("correct horse").unwrap();
        assert!(verify_password("correct horse", &hash).unwrap());
        assert!(!verify_password("wrong", &hash).unwrap());
    }

    #[test]
    fn verify_rejects_malformed_hash() {
        assert!(verify_password("x", "not-a-hash").is_err());
    }

    #[test]
    fn random_password_shape_and_uniqueness() {
        let p1 = generate_random_password();
        let p2 = generate_random_password();
        assert_eq!(p1.len(), 16);
        assert!(p1.chars().all(|c| c.is_ascii_alphanumeric()));
        // Ambiguous characters are excluded by the charset.
        assert!(!p1.contains(['0', 'O', '1', 'l', 'I']));
        assert_ne!(p1, p2);
    }

    #[test]
    fn expiry_parsing_formats() {
        assert!(parse_expiry("2030-01-02T03:04:05Z").is_some());
        assert!(parse_expiry("2030-01-02T03:04").is_some());
        assert!(parse_expiry("2030-01-02 03:04:05").is_some());
        // Date-only means midnight UTC.
        let d = parse_expiry("2030-01-02").unwrap();
        assert_eq!(d.to_rfc3339(), "2030-01-02T00:00:00+00:00");
        assert!(parse_expiry("").is_none());
        assert!(parse_expiry("not a date").is_none());
    }

    #[test]
    fn api_key_hash_shape() {
        let h = hash_api_key("sk-test");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(h, hash_api_key("sk-test"));
        assert_ne!(h, hash_api_key("sk-other"));
    }

    #[test]
    fn jwt_roundtrip_and_secret_isolation() {
        let config = crate::config::AppConfig {
            jwt_secret: "test-secret".to_string(),
            jwt_expires_in: 3600,
            ..Default::default()
        };
        let token = create_token("uid", "alice", "admin", 0, &config).unwrap();
        let claims = validate_token(&token, &config).unwrap();
        assert_eq!(claims.sub, "uid");
        assert_eq!(claims.username, "alice");
        assert_eq!(claims.role, "admin");

        // A token signed with another secret must not validate.
        let other = crate::config::AppConfig {
            jwt_secret: "other-secret".to_string(),
            ..Default::default()
        };
        assert!(validate_token(&token, &other).is_err());
    }

    #[test]
    fn jwt_expired_token_rejected() {
        let config = crate::config::AppConfig {
            jwt_secret: "test-secret".to_string(),
            jwt_expires_in: 1, // 1 second lifetime
            ..Default::default()
        };
        // Forge an already-expired token by building claims directly.
        let claims = super::Claims {
            sub: "uid".into(),
            username: "bob".into(),
            role: "user".into(),
            iat: 1_000_000,
            exp: 1_000_001, // long past
            token_version: 0,
        };
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(config.jwt_secret.as_bytes()),
        )
        .unwrap();
        assert!(validate_token(&token, &config).is_err());
    }
}
