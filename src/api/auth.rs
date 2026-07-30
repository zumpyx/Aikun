use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use rusqlite::params;
use serde_json::json;
use uuid::Uuid;

use crate::auth::{create_token, hash_api_key, hash_password, parse_expiry, verify_password, Claims};
use crate::AppState;
use crate::models::{
    row_to_user, ApiKey, ApiKeyResponse, CreateApiKeyRequest, LoginRequest, LoginResponse,
    UpdateApiKeyRequest, UserResponse,
};

/// A valid Argon2 hash used to equalize response timing when the requested
/// username does not exist (prevents account-enumeration via timing).
fn dummy_password_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| {
        hash_password("dummy-password").unwrap_or_else(|_| {
            // Valid PHC string for "password"; only used for timing equalization.
            "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$2hV3t0s4rVdXKZ2dKjSZUw".to_string()
        })
    })
}

/// Mask an API key for list responses: `sk-****` + stored suffix (last 4
/// characters of the plaintext). The full key is only returned once, at
/// creation time.
fn mask_api_key(key_suffix: &str) -> String {
    format!("sk-****{}", key_suffix)
}

/// Sliding-window login rate limiter state (lives in AppState).
/// Per-username limit stops targeted brute-force; per-IP limit stops
/// spraying across usernames.
const LOGIN_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);
const MAX_FAILS_PER_USER: usize = 5;
const MAX_FAILS_PER_IP: usize = 30;

/// 锁中毒时取回内部数据继续工作,不静默 fail-open/fail-closed。
fn lock_login_attempts(
    state: &AppState,
) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Vec<std::time::Instant>>> {
    state.login_attempts.lock().unwrap_or_else(|e| e.into_inner())
}

/// True if either key has exhausted its failure budget within the window.
fn login_limited(state: &AppState, keys: &[String]) -> bool {
    let now = std::time::Instant::now();
    let mut attempts = lock_login_attempts(state);
    // Opportunistic cleanup of expired entries.
    attempts.retain(|_, v| {
        v.retain(|t| now.duration_since(*t) < LOGIN_WINDOW);
        !v.is_empty()
    });
    keys.iter().any(|k| {
        let limit = if k.starts_with("ip:") { MAX_FAILS_PER_IP } else { MAX_FAILS_PER_USER };
        attempts.get(k).map_or(false, |v| v.len() >= limit)
    })
}

fn record_login_failure(state: &AppState, keys: &[String]) {
    let mut attempts = lock_login_attempts(state);
    let now = std::time::Instant::now();
    for k in keys {
        attempts.entry(k.clone()).or_default().push(now);
    }
}

fn clear_login_failures(state: &AppState, keys: &[String]) {
    let mut attempts = lock_login_attempts(state);
    for k in keys {
        attempts.remove(k);
    }
}

/// 登录限流用的客户端 IP:配置开启 trust_x_forwarded_for 时取
/// X-Forwarded-For 首个 IP(部署在可信反代之后),否则用对端 socket 地址。
fn client_ip(headers: &axum::http::HeaderMap, addr: &std::net::SocketAddr, trust_xff: bool) -> String {
    if trust_xff {
        if let Some(first) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            return first.to_string();
        }
    }
    addr.ip().to_string()
}

pub async fn login(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(req): Json<LoginRequest>,
) -> impl IntoResponse {
    let user_key = format!("user:{}", req.username.to_lowercase());
    let limit_keys = [format!("ip:{}", client_ip(&headers, &addr, state.config.trust_x_forwarded_for)), user_key.clone()];
    if login_limited(&state, &limit_keys) {
        return (StatusCode::TOO_MANY_REQUESTS, Json(json!({
            "error": "too_many_attempts",
            "message": "Too many failed login attempts, try again later"
        })));
    }

    // Look up the user inside the lock; password verification happens outside.
    let user = {
        let conn = match state.pool.read().lock() {
            Ok(c) => c,
            Err(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                    "error": "internal_error"
                })))
            }
        };
        conn.query_row(
            "SELECT id, username, password_hash, display_name, role, is_active, created_at, updated_at, token_version
             FROM users WHERE username = ?1",
            params![req.username],
            |row| Ok((row_to_user(row)?, row.get::<_, i64>(8)?)),
        )
        .ok()
    }; // lock released

    // Verify the password regardless of whether the user exists, so unknown
    // usernames cost the same Argon2 run (no timing oracle). Argon2 is
    // CPU-bound, so it runs on the blocking thread pool.
    let (user, password_ok) = match user {
        Some(u) => {
            let password = req.password.clone();
            let hash = u.0.password_hash.clone();
            let ok = tokio::task::spawn_blocking(move || {
                verify_password(&password, &hash).unwrap_or(false)
            })
            .await
            .unwrap_or(false);
            (Some(u), ok)
        }
        None => {
            let password = req.password.clone();
            let dummy = dummy_password_hash();
            let _ = tokio::task::spawn_blocking(move || verify_password(&password, dummy)).await;
            (None, false)
        }
    };

    if !password_ok {
        record_login_failure(&state, &limit_keys);
        return (StatusCode::UNAUTHORIZED, Json(json!({
            "error": "invalid_credentials",
            "message": "Invalid username or password"
        })));
    }

    let (user, token_version) = match user {
        Some(u) => u,
        None => {
            return (StatusCode::UNAUTHORIZED, Json(json!({
                "error": "invalid_credentials",
                "message": "Invalid username or password"
            })))
        }
    };

    // Check account status only after the password has passed, so disabled
    // state cannot be probed without valid credentials.
    if !user.is_active {
        return (StatusCode::FORBIDDEN, Json(json!({
            "error": "account_disabled",
            "message": "This account has been disabled"
        })));
    }

    match create_token(&user.id, &user.username, &user.role, token_version, &state.config) {
        Ok(token) => {
            // 只清该用户的失败记录;ip: 键保留,防止借成功登录重置 IP 限额。
            clear_login_failures(&state, &[user_key]);
            let resp = LoginResponse {
                token,
                user: UserResponse::from(user),
            };
            (StatusCode::OK, Json(json!(resp)))
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
            "error": "token_creation_failed"
        }))),
    }
}

pub async fn get_current_user(
    State(state): State<AppState>,
    claims: Claims,
) -> impl IntoResponse {
    let conn = match state.pool.read().lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let user = conn.query_row(
        "SELECT id, username, password_hash, display_name, role, is_active, created_at, updated_at
         FROM users WHERE id = ?1",
        params![claims.sub],
        row_to_user,
    );

    match user {
        Ok(u) => (StatusCode::OK, Json(json!(UserResponse::from(u)))),
        Err(_) => (StatusCode::NOT_FOUND, Json(json!({"error": "user_not_found"}))),
    }
}

pub async fn create_api_key(
    State(state): State<AppState>,
    claims: Claims,
    Json(req): Json<CreateApiKeyRequest>,
) -> impl IntoResponse {
    let expires_at = req.expires_at.filter(|s| !s.trim().is_empty());
    if let Some(ref exp) = expires_at {
        // Must be parseable and in the future.
        match parse_expiry(exp) {
            Some(t) if t > chrono::Utc::now() => {}
            _ => {
                return (StatusCode::BAD_REQUEST, Json(json!({
                    "error": "invalid_expires_at",
                    "message": "expires_at is not a valid future timestamp"
                })))
            }
        }
    }

    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let id = Uuid::new_v4().to_string();
    let key = format!("sk-{:x}{:x}", Uuid::new_v4(), Uuid::new_v4());
    // 入库 sha256 哈希与明文后 4 位;完整明文只在本次响应返回一次。
    let key_hash = hash_api_key(&key);
    let key_suffix: String = key
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let name = req.name.unwrap_or_default();
    let models_json = serde_json::to_string(&req.models.unwrap_or_default())
        .unwrap_or_else(|_| "[]".to_string());
    let rate_limit_rpm = req.rate_limit_rpm.unwrap_or(0).max(0);
    let quota_daily_tokens = req.quota_daily_tokens.unwrap_or(0).max(0);

    match conn.execute(
        "INSERT INTO api_keys (id, user_id, key, key_suffix, name, expires_at, models, rate_limit_rpm, quota_daily_tokens) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![id, claims.sub, key_hash, key_suffix, name, expires_at, models_json, rate_limit_rpm, quota_daily_tokens],
    ) {
        Ok(_) => (StatusCode::CREATED, Json(json!(ApiKeyResponse {
            id,
            user_id: claims.sub,
            key,
            name,
            is_active: true,
            last_used_at: None,
            expires_at,
            models: serde_json::from_str(&models_json).unwrap_or_default(),
            rate_limit_rpm,
            quota_daily_tokens,
            created_at: chrono::Utc::now().to_rfc3339(),
        }))),
        Err(e) => {
            tracing::error!("Failed to create API key: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                "error": "create_failed",
                "message": "Internal server error"
            })))
        }
    }
}

pub async fn list_api_keys(
    State(state): State<AppState>,
    claims: Claims,
) -> impl IntoResponse {
    let conn = match state.pool.read().lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let mut stmt = match conn.prepare(
        "SELECT id, user_id, key_suffix, name, is_active, last_used_at, expires_at, models,
                rate_limit_rpm, quota_daily_tokens, created_at
         FROM api_keys WHERE user_id = ?1"
    ) {
        Ok(s) => s,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "query_failed"}))),
    };

    let keys: Vec<ApiKeyResponse> = match stmt.query_map(params![claims.sub], |row| {
            Ok(ApiKey {
                id: row.get(0)?,
                user_id: row.get(1)?,
                key: row.get(2)?,
                name: row.get(3)?,
                is_active: row.get(4)?,
                last_used_at: row.get(5)?,
                expires_at: row.get(6)?,
                models: row.get(7)?,
                rate_limit_rpm: row.get(8)?,
                quota_daily_tokens: row.get(9)?,
                created_at: row.get(10)?,
            })
        }) {
            Ok(rows) => rows
                .filter_map(|r| r.ok())
                .map(ApiKeyResponse::from)
                .map(|mut k| {
                    // Never expose the full key in list responses — the `key`
                    // field here carries key_suffix, not the stored hash.
                    k.key = mask_api_key(&k.key);
                    k
                })
                .collect(),
            Err(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "query_failed"})))
            }
        };

    (StatusCode::OK, Json(json!(keys)))
}

/// Update an API key (rename, enable/disable, expiry, model whitelist).
/// Only the owner can update their own keys.
pub async fn update_api_key(
    State(state): State<AppState>,
    claims: Claims,
    Path(key_id): Path<String>,
    Json(req): Json<UpdateApiKeyRequest>,
) -> impl IntoResponse {
    if let Some(ref exp) = req.expires_at {
        // Empty string clears the expiry; anything else must be a valid
        // future timestamp.
        if !exp.trim().is_empty() {
            match parse_expiry(exp) {
                Some(t) if t > chrono::Utc::now() => {}
                _ => {
                    return (StatusCode::BAD_REQUEST, Json(json!({
                        "error": "invalid_expires_at",
                        "message": "expires_at is not a valid future timestamp"
                    })))
                }
            }
        }
    }

    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let mut updates = Vec::new();
    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(name) = &req.name {
        updates.push("name = ?");
        params_vec.push(Box::new(name.clone()));
    }
    if let Some(active) = req.is_active {
        updates.push("is_active = ?");
        params_vec.push(Box::new(active as i32));
    }
    if let Some(exp) = &req.expires_at {
        updates.push("expires_at = ?");
        // Empty string clears the expiry.
        params_vec.push(Box::new(if exp.trim().is_empty() { None } else { Some(exp.clone()) }));
    }
    if let Some(models) = &req.models {
        updates.push("models = ?");
        params_vec.push(Box::new(
            serde_json::to_string(models).unwrap_or_else(|_| "[]".to_string()),
        ));
    }
    if let Some(rpm) = req.rate_limit_rpm {
        updates.push("rate_limit_rpm = ?");
        params_vec.push(Box::new(rpm.max(0)));
    }
    if let Some(quota) = req.quota_daily_tokens {
        updates.push("quota_daily_tokens = ?");
        params_vec.push(Box::new(quota.max(0)));
    }

    if updates.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "no_fields_to_update"})));
    }

    let sql = format!(
        "UPDATE api_keys SET {} WHERE id = ? AND user_id = ?",
        updates.join(", ")
    );
    let mut params: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
    params.push(&key_id);
    params.push(&claims.sub);

    match conn.execute(&sql, params.as_slice()) {
        Ok(n) if n > 0 => (StatusCode::OK, Json(json!({"message": "updated"}))),
        Ok(_) => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
        Err(e) => {
            tracing::error!("Failed to update API key: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                "error": "update_failed",
                "message": "Internal server error"
            })))
        }
    }
}

pub async fn delete_api_key(
    State(state): State<AppState>,
    claims: Claims,
    Path(key_id): Path<String>,
) -> impl IntoResponse {
    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    match conn.execute(
        "DELETE FROM api_keys WHERE id = ?1 AND user_id = ?2",
        params![key_id, claims.sub],
    ) {
        Ok(n) if n > 0 => (StatusCode::OK, Json(json!({"message": "deleted"}))),
        Ok(_) => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
        Err(e) => {
            tracing::error!("Failed to delete API key: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                "error": "internal_error",
                "message": "Internal server error"
            })))
        }
    }
}
