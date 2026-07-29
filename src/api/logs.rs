use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use rusqlite::params;
use serde::Deserialize;
use serde_json::json;
use crate::auth::Claims;
use crate::AppState;
use crate::models::{RequestLog, RequestLogResponse};

#[derive(Debug, Deserialize)]
pub struct LogQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub model: Option<String>,
    pub success: Option<String>,
    pub user_id: Option<String>,
    pub since: Option<String>,
}

/// Get request logs. Admin sees all, regular users see only their own.
pub async fn list_logs(
    State(state): State<AppState>,
    claims: Claims,
    Query(query): Query<LogQuery>,
) -> impl IntoResponse {
    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0);

    let is_admin = claims.role == "admin";

    // success 参数只接受 0/1，其余一律 400（不再静默按 1 处理）
    let success_filter: Option<i32> = match query.success.as_deref() {
        None | Some("") => None,
        Some("0") => Some(0),
        Some("1") => Some(1),
        Some(_) => {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "error": "invalid_success",
                "message": "success 仅接受 0 或 1"
            })));
        }
    };

    let mut conditions = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    // 非管理员强制只看自己的日志，model/success/since 过滤同样可用
    if !is_admin {
        conditions.push("user_id = ?");
        params.push(Box::new(claims.sub.clone()));
    }
    if let Some(ref model) = query.model {
        if !model.is_empty() {
            conditions.push("model = ?");
            params.push(Box::new(model.clone()));
        }
    }
    if let Some(s) = success_filter {
        conditions.push("success = ?");
        params.push(Box::new(s));
    }
    if is_admin {
        if let Some(ref uid) = query.user_id {
            if !uid.is_empty() {
                conditions.push("user_id = ?");
                params.push(Box::new(uid.clone()));
            }
        }
    }
    if let Some(ref since) = query.since {
        if !since.is_empty() {
            conditions.push("created_at >= ?");
            params.push(Box::new(since.clone()));
        }
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {} ", conditions.join(" AND "))
    };

    params.push(Box::new(limit));
    params.push(Box::new(offset));

    let sql = format!(
        "SELECT id, user_id, api_key_id, provider_id, model, request_type,
                prompt_tokens, completion_tokens, total_tokens, latency_ms,
                status_code, success, error_message, created_at
         FROM request_logs
         {}ORDER BY created_at DESC LIMIT ? OFFSET ?",
        where_clause
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to prepare log query: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                "error": "query_failed",
                "message": "Internal server error"
            })));
        }
    };

    let logs: Vec<RequestLogResponse> = match stmt.query_map(param_refs.as_slice(), |row| {
        Ok(RequestLog {
            id: row.get(0)?,
            user_id: row.get(1)?,
            api_key_id: row.get(2)?,
            provider_id: row.get(3)?,
            model: row.get(4)?,
            request_type: row.get(5)?,
            prompt_tokens: row.get(6)?,
            completion_tokens: row.get(7)?,
            total_tokens: row.get(8)?,
            latency_ms: row.get(9)?,
            status_code: row.get(10)?,
            success: row.get(11)?,
            error_message: row.get(12)?,
            created_at: row.get(13)?,
        })
    }) {
        Ok(rows) => rows.filter_map(|r| match r {
            Ok(log) => Some(RequestLogResponse::from(log)),
            Err(e) => {
                tracing::warn!("Skipping malformed request log row: {}", e);
                None
            }
        }).collect(),
        Err(e) => {
            tracing::error!("Failed to query logs: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                "error": "query_failed",
                "message": "Internal server error"
            })));
        }
    };

    (StatusCode::OK, Json(json!(logs)))
}

/// Get log stats. Admin sees global stats, users see personal stats.
pub async fn log_stats(
    State(state): State<AppState>,
    claims: Claims,
) -> impl IntoResponse {
    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let is_admin = claims.role == "admin";

    let (sql, user_param): (&str, Option<String>) = if is_admin {
        (
            "SELECT COUNT(*), COALESCE(SUM(total_tokens), 0), COALESCE(AVG(latency_ms), 0),
                    COALESCE(SUM(CASE WHEN success = 1 THEN 1 ELSE 0 END) * 100.0 / NULLIF(COUNT(*), 0), 0)
             FROM request_logs",
            None,
        )
    } else {
        (
            "SELECT COUNT(*), COALESCE(SUM(total_tokens), 0), COALESCE(AVG(latency_ms), 0),
                    COALESCE(SUM(CASE WHEN success = 1 THEN 1 ELSE 0 END) * 100.0 / NULLIF(COUNT(*), 0), 0)
             FROM request_logs WHERE user_id = ?1",
            Some(claims.sub),
        )
    };

    let result = if let Some(uid) = user_param {
        conn.query_row(sql, params![uid], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, f64>(3)?,
            ))
        })
    } else {
        conn.query_row(sql, [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, f64>(3)?,
            ))
        })
    };

    match result {
        Ok((total_requests, total_tokens, avg_latency, success_rate)) => {
            (StatusCode::OK, Json(json!({
                "total_requests": total_requests,
                "total_tokens": total_tokens,
                "avg_latency_ms": avg_latency.round() as i64,
                "success_rate": (success_rate * 100.0).round() / 100.0
            })))
        }
        Err(e) => {
            tracing::error!("Failed to query log stats: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                "error": "query_failed",
                "message": "Internal server error"
            })))
        }
    }
}