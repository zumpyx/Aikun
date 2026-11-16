use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use rusqlite::params;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::AppState;
use crate::auth::{Claims, hash_api_key, parse_expiry};
use crate::models::{
    ApiKey, ApiKeyResponse, CreateApiKeyRequest, UpdateApiKeyRequest, UserResponse, row_to_user,
};

// login 已迁 crates/aikun-admin/src/auth.rs(POST /api/auth/login,spec-first
// 新契约;登录限流/防枚举时序均衡/禁用后验密顺序全部随迁)。

/// Mask an API key for list responses: `sk-****` + stored suffix (last 4
/// characters of the plaintext). The full key is only returned once, at
/// creation time.
fn mask_api_key(key_suffix: &str) -> String {
    format!("sk-****{}", key_suffix)
}

pub async fn get_current_user(State(state): State<AppState>, claims: Claims) -> impl IntoResponse {
    let conn = match state.pool.read().lock() {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal_error"})),
            );
        }
    };

    let user = conn.query_row(
        "SELECT id, username, password_hash, display_name, role, is_active, created_at, updated_at, balance
         FROM users WHERE id = ?1",
        params![claims.sub],
        |row| Ok((row_to_user(row)?, row.get::<_, i64>(8)?)),
    );

    match user {
        // 余额对本人可见(只读):扣费是实时发生的,用户需要能自查。
        // 库存整数微元,出参转元。
        Ok((u, balance)) => {
            let mut v = serde_json::to_value(UserResponse::from(u)).unwrap_or_else(|_| json!({}));
            v["balance"] = json!(crate::billing::micro_to_yuan(balance));
            (StatusCode::OK, Json(v))
        }
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "user_not_found"})),
        ),
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
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "invalid_expires_at",
                        "message": "expires_at is not a valid future timestamp"
                    })),
                );
            }
        }
    }

    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal_error"})),
            );
        }
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
    let models_json =
        serde_json::to_string(&req.models.unwrap_or_default()).unwrap_or_else(|_| "[]".to_string());
    let rate_limit_rpm = req.rate_limit_rpm.unwrap_or(0).max(0);
    let quota_daily_tokens = req.quota_daily_tokens.unwrap_or(0).max(0);
    let max_concurrent = req.max_concurrent.unwrap_or(0).max(0);

    match conn.execute(
        "INSERT INTO api_keys (id, user_id, key, key_suffix, name, expires_at, models, rate_limit_rpm, quota_daily_tokens, max_concurrent) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![id, claims.sub, key_hash, key_suffix, name, expires_at, models_json, rate_limit_rpm, quota_daily_tokens, max_concurrent],
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
            max_concurrent,
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

#[derive(Debug, Deserialize)]
pub struct ListApiKeysQuery {
    pub user_id: Option<String>,
}

pub async fn list_api_keys(
    State(state): State<AppState>,
    claims: Claims,
    Query(query): Query<ListApiKeysQuery>,
) -> impl IntoResponse {
    let conn = match state.pool.read().lock() {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal_error"})),
            );
        }
    };

    // admin 带 ?user_id= 时查看指定用户的 key(后台跨用户管理);不带则与
    // 普通用户一样只看自己的,语义不变。
    let target_uid = if claims.role == "admin" {
        query
            .user_id
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| claims.sub.clone())
    } else {
        claims.sub.clone()
    };

    let mut stmt = match conn.prepare(
        "SELECT id, user_id, key_suffix, name, is_active, last_used_at, expires_at, models,
                rate_limit_rpm, quota_daily_tokens, max_concurrent, created_at
         FROM api_keys WHERE user_id = ?1",
    ) {
        Ok(s) => s,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "query_failed"})),
            );
        }
    };

    let keys: Vec<ApiKeyResponse> = match stmt.query_map(params![target_uid], |row| {
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
            max_concurrent: row.get(10)?,
            created_at: row.get(11)?,
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
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "query_failed"})),
            );
        }
    };

    (StatusCode::OK, Json(json!(keys)))
}

/// Update an API key (rename, enable/disable, expiry, model whitelist).
/// 只有 key 的属主可以修改;admin 不受属主限制(后台跨用户管理),
/// 非 admin 操作他人 key 一律 404(不暴露 key 是否存在)。
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
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": "invalid_expires_at",
                            "message": "expires_at is not a valid future timestamp"
                        })),
                    );
                }
            }
        }
    }

    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal_error"})),
            );
        }
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
        params_vec.push(Box::new(if exp.trim().is_empty() {
            None
        } else {
            Some(exp.clone())
        }));
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
    if let Some(mc) = req.max_concurrent {
        updates.push("max_concurrent = ?");
        params_vec.push(Box::new(mc.max(0)));
    }

    if updates.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no_fields_to_update"})),
        );
    }

    let is_admin = claims.role == "admin";
    let sql = if is_admin {
        format!("UPDATE api_keys SET {} WHERE id = ?", updates.join(", "))
    } else {
        format!(
            "UPDATE api_keys SET {} WHERE id = ? AND user_id = ?",
            updates.join(", ")
        )
    };
    let mut params: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(|p| p.as_ref()).collect();
    params.push(&key_id);
    if !is_admin {
        params.push(&claims.sub);
    }

    match conn.execute(&sql, params.as_slice()) {
        Ok(n) if n > 0 => (StatusCode::OK, Json(json!({"message": "updated"}))),
        Ok(_) => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
        Err(e) => {
            tracing::error!("Failed to update API key: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "update_failed",
                    "message": "Internal server error"
                })),
            )
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
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal_error"})),
            );
        }
    };

    match if claims.role == "admin" {
        // admin 可删除任意用户的 key;非 admin 仅限自己的(否则 404)。
        conn.execute("DELETE FROM api_keys WHERE id = ?1", params![key_id])
    } else {
        conn.execute(
            "DELETE FROM api_keys WHERE id = ?1 AND user_id = ?2",
            params![key_id, claims.sub],
        )
    } {
        Ok(n) if n > 0 => {
            // 清掉内存中的 RPM 滑动窗口:窗口只在该 key 自己有请求时才被
            // retain,被删 key 的非空窗口(全是过期时间戳)会成为死条目。
            if let Ok(mut windows) = state.api_key_rate.lock() {
                windows.remove(&key_id);
            }
            (StatusCode::OK, Json(json!({"message": "deleted"})))
        }
        Ok(_) => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
        Err(e) => {
            tracing::error!("Failed to delete API key: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "internal_error",
                    "message": "Internal server error"
                })),
            )
        }
    }
}
