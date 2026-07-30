use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use rusqlite::params;
use serde_json::json;
use uuid::Uuid;

use crate::auth::{generate_random_password, hash_password};
use crate::AppState;
use crate::models::{
    row_to_user, BatchCreateUsersRequest, CreateUserRequest, UpdateUserRequest, UserResponse,
};

fn is_valid_role(role: &str) -> bool {
    role == "admin" || role == "user"
}

/// 是否为唯一约束冲突(username 已存在)。
fn is_unique_violation(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(err, _)
            if err.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

fn bad_request(error: &str, message: &str) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({"error": error, "message": message})))
}

/// username:trim 后 1-64 字符,返回 trim 后的值。
fn validate_username(username: &str) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let u = username.trim();
    if u.is_empty() || u.chars().count() > 64 {
        return Err(bad_request("invalid_username", "username must be 1-64 characters"));
    }
    Ok(u.to_string())
}

/// 密码:8-256 字节且非全空白。
fn validate_password(password: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if password.len() < 8 || password.len() > 256 || password.trim().is_empty() {
        return Err(bad_request(
            "invalid_password",
            "password must be 8-256 bytes and not all whitespace",
        ));
    }
    Ok(())
}

/// display_name:≤64 字符。
fn validate_display_name(name: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if name.chars().count() > 64 {
        return Err(bad_request(
            "invalid_display_name",
            "display_name must be at most 64 characters",
        ));
    }
    Ok(())
}

/// Whether the given user is currently the only active admin. Used to protect
/// the last admin from being demoted or disabled.
fn is_last_active_admin(conn: &rusqlite::Connection, user_id: &str) -> bool {
    let target: Option<(String, bool)> = conn
        .query_row(
            "SELECT role, is_active FROM users WHERE id = ?1",
            params![user_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    match target {
        Some((role, is_active)) if role == "admin" && is_active => {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM users WHERE role = 'admin' AND is_active = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            count <= 1
        }
        _ => false,
    }
}

pub async fn list_users(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let conn = match state.pool.read().lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let mut stmt = match conn.prepare(
        "SELECT id, username, password_hash, display_name, role, is_active, created_at, updated_at
         FROM users ORDER BY created_at DESC"
    ) {
        Ok(s) => s,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "query_failed"}))),
    };

    let users: Vec<UserResponse> = match stmt.query_map([], row_to_user) {
            Ok(rows) => rows
                .filter_map(|r| r.ok())
                .map(UserResponse::from)
                .collect(),
            Err(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "query_failed"})))
            }
        };

    (StatusCode::OK, Json(json!(users)))
}

pub async fn create_user(
    State(state): State<AppState>,
    Json(req): Json<CreateUserRequest>,
) -> impl IntoResponse {
    let role = req.role.unwrap_or_else(|| "user".to_string());
    if !is_valid_role(&role) {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "invalid_role",
            "message": "role must be 'admin' or 'user'"
        })));
    }

    let username = match validate_username(&req.username) {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    if let Err(resp) = validate_password(&req.password) {
        return resp;
    }
    let display_name = match &req.display_name {
        Some(d) => {
            if let Err(resp) = validate_display_name(d) {
                return resp;
            }
            d.clone()
        }
        None => username.clone(),
    };

    // Hash outside the DB lock — Argon2 is expensive and must not block
    // other database users; it runs on the blocking thread pool so the
    // async executor is not stalled either.
    let password = req.password.clone();
    let password_hash = match tokio::task::spawn_blocking(move || hash_password(&password)).await {
        Ok(Ok(h)) => h,
        _ => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "hash_failed"}))),
    };

    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let id = Uuid::new_v4().to_string();

    match conn.execute(
        "INSERT INTO users (id, username, password_hash, display_name, role)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, username, password_hash, display_name, role],
    ) {
        Ok(_) => (StatusCode::CREATED, Json(json!(UserResponse {
            id,
            username,
            display_name,
            role,
            is_active: true,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        }))),
        Err(e) if is_unique_violation(&e) => {
            (StatusCode::CONFLICT, Json(json!({"error": "username_exists"})))
        }
        Err(e) => {
            tracing::error!("Failed to create user: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                "error": "create_failed",
                "message": "Internal server error"
            })))
        }
    }
}

/// One validated+hashed entry ready for insertion (batch creation phase 1).
struct PreparedBatchUser {
    username: String,
    display_name: String,
    role: String,
    password_hash: String,
    generated_password: Option<String>,
}

/// Batch phase 1 — validate entries and hash passwords. Argon2 is CPU-bound,
/// so this is designed to run on the blocking thread pool, off the async
/// executor. Returns (prepared entries, per-item results, indexes into the
/// results vec for prepared entries).
fn prepare_batch_users(
    users: Vec<crate::models::BatchCreateUserItem>,
) -> (Vec<PreparedBatchUser>, Vec<serde_json::Value>, Vec<usize>) {
    let mut prepared: Vec<PreparedBatchUser> = Vec::with_capacity(users.len());
    let mut results: Vec<serde_json::Value> = Vec::with_capacity(users.len());
    // Indexes into `results` that correspond to prepared entries.
    let mut prepared_result_idx: Vec<usize> = Vec::with_capacity(users.len());

    for item in users {
        let username = item.username.trim().to_string();
        let role = item.role.unwrap_or_else(|| "user".to_string());
        // 与单个创建同一套规则(validate_username / validate_display_name /
        // validate_password),批量接口不放弱校验的账号进来。
        if username.is_empty() || username.chars().count() > 64 {
            results.push(json!({"username": username, "ok": false, "error": "invalid_username"}));
            continue;
        }
        if !is_valid_role(&role) {
            results.push(json!({"username": username, "ok": false, "error": "invalid_role"}));
            continue;
        }
        let display_name = item.display_name.unwrap_or_else(|| username.clone());
        if display_name.chars().count() > 64 {
            results.push(json!({"username": username, "ok": false, "error": "invalid_display_name"}));
            continue;
        }
        let (password, generated) = match item.password {
            Some(p) if !p.is_empty() => {
                if p.len() < 8 || p.len() > 256 || p.trim().is_empty() {
                    results.push(json!({"username": username, "ok": false, "error": "invalid_password"}));
                    continue;
                }
                (p, None)
            }
            _ => {
                let p = generate_random_password();
                (p.clone(), Some(p))
            }
        };
        let password_hash = match hash_password(&password) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("Failed to hash password in batch create: {}", e);
                results.push(json!({"username": username, "ok": false, "error": "hash_failed"}));
                continue;
            }
        };
        prepared_result_idx.push(results.len());
        results.push(serde_json::Value::Null); // placeholder, filled in phase 2
        prepared.push(PreparedBatchUser {
            display_name,
            username,
            role,
            password_hash,
            generated_password: generated,
        });
    }

    (prepared, results, prepared_result_idx)
}

/// Batch-create users (admin only). Each item may omit the password — a random
/// one is generated then and returned exactly once in the per-item result.
/// Items are independent: one bad entry does not abort the rest.
pub async fn create_users_batch(
    State(state): State<AppState>,
    Json(req): Json<BatchCreateUsersRequest>,
) -> impl IntoResponse {
    const MAX_BATCH: usize = 100;
    if req.users.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "empty_batch",
            "message": "users must not be empty"
        })));
    }
    if req.users.len() > MAX_BATCH {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "batch_too_large",
            "message": format!("at most {} users per batch", MAX_BATCH)
        })));
    }

    // Phase 1 — validate and hash outside the DB lock, on the blocking pool
    // (Argon2 is expensive and must not stall the async executor).
    let (prepared, mut results, prepared_result_idx) =
        match tokio::task::spawn_blocking(move || prepare_batch_users(req.users)).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Batch prepare task failed: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal_error"})),
                );
            }
        };

    // Phase 2 — insert under one lock.
    {
        let conn = match state.pool.conn.lock() {
            Ok(c) => c,
            Err(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"})))
            }
        };
        for (p, ridx) in prepared.iter().zip(prepared_result_idx.iter()) {
            let id = Uuid::new_v4().to_string();
            let outcome = match conn.execute(
                "INSERT INTO users (id, username, password_hash, display_name, role)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, p.username, p.password_hash, p.display_name, p.role],
            ) {
                Ok(_) => {
                    let mut entry = json!({
                        "username": p.username,
                        "ok": true,
                        "id": id,
                        "role": p.role,
                    });
                    if let Some(gp) = &p.generated_password {
                        entry["password"] = json!(gp);
                    }
                    entry
                }
                Err(e) if is_unique_violation(&e) => {
                    json!({"username": p.username, "ok": false, "error": "username_exists"})
                }
                Err(e) => {
                    tracing::error!("Failed to batch-create user {}: {}", p.username, e);
                    json!({"username": p.username, "ok": false, "error": "create_failed"})
                }
            };
            results[*ridx] = outcome;
        }
    }

    let created = results.iter().filter(|r| r["ok"] == true).count();
    (StatusCode::OK, Json(json!({
        "created": created,
        "failed": results.len() - created,
        "results": results,
    })))
}

pub async fn get_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> impl IntoResponse {
    let conn = match state.pool.read().lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let user = conn.query_row(
        "SELECT id, username, password_hash, display_name, role, is_active, created_at, updated_at
         FROM users WHERE id = ?1",
        params![user_id],
        row_to_user,
    );

    match user {
        Ok(u) => (StatusCode::OK, Json(json!(UserResponse::from(u)))),
        Err(_) => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
    }
}

pub async fn update_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    Json(req): Json<UpdateUserRequest>,
) -> impl IntoResponse {
    if let Some(ref role) = req.role {
        if !is_valid_role(role) {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "error": "invalid_role",
                "message": "role must be 'admin' or 'user'"
            })));
        }
    }
    if let Some(ref password) = req.password {
        if let Err(resp) = validate_password(password) {
            return resp;
        }
    }
    if let Some(ref name) = req.display_name {
        if let Err(resp) = validate_display_name(name) {
            return resp;
        }
    }

    // Hash a new password outside the DB lock, on the blocking thread pool.
    let new_password_hash = match req.password.clone() {
        Some(password) => match tokio::task::spawn_blocking(move || hash_password(&password)).await {
            Ok(Ok(h)) => Some(h),
            _ => {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "hash_failed"})))
            }
        },
        None => None,
    };

    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    // Protect the last active admin from being demoted or disabled.
    let demotes_or_disables =
        req.role.as_deref().is_some_and(|r| r != "admin") || req.is_active == Some(false);
    if demotes_or_disables && is_last_active_admin(&conn, &user_id) {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "last_admin",
            "message": "Cannot demote or disable the last active admin"
        })));
    }

    let mut updates = Vec::new();
    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(name) = &req.display_name {
        updates.push("display_name = ?");
        params_vec.push(Box::new(name.clone()));
    }
    if let Some(hash) = new_password_hash {
        updates.push("password_hash = ?");
        params_vec.push(Box::new(hash));
        // 改密后递增 token_version,该用户已签发的 JWT 全部失效。
        updates.push("token_version = token_version + 1");
    }
    if let Some(active) = req.is_active {
        updates.push("is_active = ?");
        params_vec.push(Box::new(active as i32));
    }
    if let Some(role) = &req.role {
        updates.push("role = ?");
        params_vec.push(Box::new(role.clone()));
    }

    if updates.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "no_fields_to_update"})));
    }

    updates.push("updated_at = datetime('now')");
    let sql = format!(
        "UPDATE users SET {} WHERE id = ?",
        updates.join(", ")
    );

    let mut params: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
    params.push(&user_id);

    match conn.execute(&sql, params.as_slice()) {
        Ok(n) if n > 0 => (StatusCode::OK, Json(json!({"message": "updated"}))),
        Ok(_) => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
        Err(e) => {
            tracing::error!("Failed to update user: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                "error": "update_failed",
                "message": "Internal server error"
            })))
        }
    }
}

pub async fn delete_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> impl IntoResponse {
    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    // Protect the last active admin from being disabled.
    if is_last_active_admin(&conn, &user_id) {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "last_admin",
            "message": "Cannot demote or disable the last active admin"
        })));
    }

    match conn.execute(
        "UPDATE users SET is_active = 0, updated_at = datetime('now') WHERE id = ?1",
        params![user_id],
    ) {
        Ok(n) if n > 0 => (StatusCode::OK, Json(json!({"message": "disabled"}))),
        Ok(_) => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
        Err(e) => {
            tracing::error!("Failed to delete user: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                "error": "delete_failed",
                "message": "Internal server error"
            })))
        }
    }
}
