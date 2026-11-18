// 口令/密钥哈希四函数已吸收进 aikun-store(plan 步骤 8,凭据的落库
// 形态归仓储层);此处保留 crate::auth 旧路径 re-export,登录/改密链路
// 行为不变。
pub use aikun_store::password::{
    generate_random_password, hash_api_key, hash_password, verify_password,
};

use axum::{
    Json,
    extract::{FromRequestParts, Request, State},
    http::StatusCode,
    http::request::Parts,
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::config::AppConfig;

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

pub fn validate_token(
    token: &str,
    config: &AppConfig,
) -> Result<Claims, jsonwebtoken::errors::Error> {
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
/// 入库的是 sha256 哈希;Surreal 侧唯一索引点查,属主用户状态与模型
/// 白名单一次投影带回(单往返,语义同旧 JOIN 查询)。
async fn authenticate_api_key(
    identity: &dyn aikun_store::surreal::identity::IdentityStore,
    key: &str,
) -> Option<(Claims, AuthContext)> {
    if key.is_empty() {
        return None;
    }
    let row = identity
        .find_api_key_auth(&crate::auth::hash_api_key(key))
        .await
        .ok()
        .flatten()?;

    if !row.key_active || !row.user_active {
        return None;
    }
    if let Some(exp) = row.expires_at
        && exp <= chrono::Utc::now()
    {
        return None; // expired
    }

    // 空集表示不限制;损坏行 fail-closed 拒绝认证(不当作全放行)。
    // 旧库 models 列是 JSON 文本,坏 JSON 在迁移侧已归一为空集。
    let allowed_models = if row.models.is_empty() {
        None
    } else {
        Some(row.models)
    };

    // 节流:距上次写入不足 60s 不更新(实现内自带,失败尽力而为)。
    let _ = identity.touch_api_key_used(&row.key_id).await;

    let now = chrono::Utc::now().timestamp() as usize;
    let claims = Claims {
        sub: row.user_id,
        username: row.username,
        role: row.role,
        // API key requests carry no token expiry; claims exp is informational.
        exp: now,
        iat: now,
        token_version: 0,
    };
    Some((
        claims,
        AuthContext {
            api_key_id: Some(row.key_id),
            allowed_models,
        },
    ))
}

/// Re-check a JWT-authenticated user against the database: disabled users are
/// rejected, and the live role from the DB overrides the (possibly stale) role
/// embedded in the token.
async fn authenticate_jwt(
    identity: &dyn aikun_store::surreal::identity::IdentityStore,
    mut claims: Claims,
) -> Option<(Claims, AuthContext)> {
    let login = identity.find_login_user_by_id(&claims.sub).await.ok()??;
    // 改密后 token_version 递增,旧 JWT 随之失效。
    if !login.is_active || login.token_version != claims.token_version {
        return None;
    }
    claims.role = login.role;
    Some((
        claims,
        AuthContext {
            api_key_id: None,
            allowed_models: None,
        },
    ))
}

/// 管理面(/api/**)鉴权失败统一 problem+json(spec-first 新契约,
/// RFC 9457):装配层守卫的对外错误体与生成路由对齐(401 invalid_token /
/// unauthorized,403 forbidden);/v1 中继面(OpenAI 兼容)维持旧扁平体,
/// 契约冻结不随管理面迁移变化。
fn auth_error(status: StatusCode, code: &str, detail: &str) -> Response {
    use axum::http::header;
    let mut resp = (
        status,
        Json(json!({
            "code": code,
            "detail": detail,
            "status": status.as_u16(),
            "title": status.canonical_reason().unwrap_or_default(),
            "type": "about:blank",
        })),
    )
        .into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/problem+json"),
    );
    resp
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
    let on_api = req.uri().path().starts_with("/api/");

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
            let identity = state.store.identity.clone();
            let authed = match validate_token(token, config) {
                Ok(claims) => authenticate_jwt(identity.as_ref(), claims).await,
                Err(_) if !on_api => {
                    let key = token.to_string();
                    authenticate_api_key(identity.as_ref(), &key).await
                }
                Err(_) => None,
            };
            match authed {
                Some((claims, ctx)) => {
                    req.extensions_mut().insert(claims);
                    req.extensions_mut().insert(ctx);
                    next.run(req).await
                }
                None if on_api => auth_error(
                    StatusCode::UNAUTHORIZED,
                    "invalid_token",
                    "Invalid or expired token",
                ),
                None => (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({
                        "error": "invalid_token",
                        "message": "Invalid or expired token"
                    })),
                )
                    .into_response(),
            }
        }
        // Anthropic clients send the key as `x-api-key` instead of a Bearer
        // token; accept it on the relay endpoints (never on management APIs).
        _ if !on_api => {
            let key = req
                .headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            // 空 key 直接拒绝,不查库。
            let authed = if key.is_empty() {
                None
            } else {
                let identity = state.store.identity.clone();
                authenticate_api_key(identity.as_ref(), key).await
            };
            match authed {
                Some((claims, ctx)) => {
                    req.extensions_mut().insert(claims);
                    req.extensions_mut().insert(ctx);
                    next.run(req).await
                }
                None => (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({
                        "error": "unauthorized",
                        "message": "Missing or invalid Authorization header"
                    })),
                )
                    .into_response(),
            }
        }
        _ if on_api => auth_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Missing or invalid Authorization header",
        ),
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "unauthorized",
                "message": "Missing or invalid Authorization header"
            })),
        )
            .into_response(),
    }
}

/// Require admin role — run after auth middleware.
pub async fn require_admin(req: Request, next: Next) -> Response {
    let claims = req.extensions().get::<Claims>();

    match claims {
        Some(claims) if claims.role == "admin" => next.run(req).await,
        Some(_) => auth_error(StatusCode::FORBIDDEN, "forbidden", "Admin access required"),
        None => auth_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Authentication required",
        ),
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
