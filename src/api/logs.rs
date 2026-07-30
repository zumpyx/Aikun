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

/// 列表与统计接口共用的筛选条件构建:非管理员强制只看自己,
/// success 仅接受 0/1(其余 400),model/user_id/since 非空才生效。
/// 返回 (WHERE 子句, 参数);参数顺序与占位符一致。
#[allow(clippy::type_complexity)]
fn build_log_filters(
    is_admin: bool,
    caller_uid: &str,
    query: &LogQuery,
) -> Result<(String, Vec<Box<dyn rusqlite::types::ToSql>>), (StatusCode, Json<serde_json::Value>)> {
    let success_filter: Option<i32> = match query.success.as_deref() {
        None | Some("") => None,
        Some("0") => Some(0),
        Some("1") => Some(1),
        Some(_) => {
            return Err((StatusCode::BAD_REQUEST, Json(json!({
                "error": "invalid_success",
                "message": "success 仅接受 0 或 1"
            }))));
        }
    };

    let mut conditions = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if !is_admin {
        conditions.push("user_id = ?");
        params.push(Box::new(caller_uid.to_string()));
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
    Ok((where_clause, params))
}

/// Get request logs. Admin sees all, regular users see only their own.
pub async fn list_logs(
    State(state): State<AppState>,
    claims: Claims,
    Query(query): Query<LogQuery>,
) -> impl IntoResponse {
    let conn = match state.pool.read().lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0);

    let is_admin = claims.role == "admin";

    let (where_clause, mut params) = match build_log_filters(is_admin, &claims.sub, &query) {
        Ok(v) => v,
        Err(resp) => return resp,
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
/// Accepts the same filters as list_logs (model/success/user_id/since) so the
/// numbers on the logs page always reflect the active filter set.
pub async fn log_stats(
    State(state): State<AppState>,
    claims: Claims,
    Query(query): Query<LogQuery>,
) -> impl IntoResponse {
    let conn = match state.pool.read().lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))).into_response(),
    };

    let is_admin = claims.role == "admin";

    let (where_clause, params) = match build_log_filters(is_admin, &claims.sub, &query) {
        Ok(v) => v,
        Err((code, body)) => return (code, body).into_response(),
    };

    let sql = format!(
        "SELECT COUNT(*), COALESCE(SUM(total_tokens), 0), COALESCE(AVG(latency_ms), 0),
                COALESCE(SUM(CASE WHEN success = 1 THEN 1 ELSE 0 END) * 100.0 / NULLIF(COUNT(*), 0), 0)
         FROM request_logs {}",
        where_clause
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let result = conn.query_row(&sql, param_refs.as_slice(), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, f64>(3)?,
        ))
    });

    match result {
        Ok((total_requests, total_tokens, avg_latency, success_rate)) => {
            (StatusCode::OK, Json(json!({
                "total_requests": total_requests,
                "total_tokens": total_tokens,
                "avg_latency_ms": avg_latency.round() as i64,
                "success_rate": (success_rate * 100.0).round() / 100.0
            }))).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to query log stats: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                "error": "query_failed",
                "message": "Internal server error"
            }))).into_response()
        }
    }
}

/// Admin-only usage aggregation for the dashboard: today / last-7d / last-30d
/// totals, a 30-day daily series (missing days zero-filled), and the top
/// models by request count over the same 30 days.
pub async fn usage_stats(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let conn = match state.pool.read().lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))).into_response(),
    };

    // created_at 前 10 位即 YYYY-MM-DD(RFC3339 与 SQLite datetime 格式通用),
    // 按天比较可避免两种时间格式混存时的字符串比较问题。
    let today = chrono::Utc::now().date_naive();
    let fmt = |d: chrono::NaiveDate| d.format("%Y-%m-%d").to_string();
    let today_s = fmt(today);
    let week_s = fmt(today - chrono::Duration::days(6));
    let month_s = fmt(today - chrono::Duration::days(29));

    // 一次查询拿到 today/week/month 三组计数:WHERE 限定近 30 天,
    // 今日与近 7 天用条件求和拆出。
    let totals = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(total_tokens), 0),
                COALESCE(SUM(CASE WHEN substr(created_at, 1, 10) = ?1 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN substr(created_at, 1, 10) = ?1 THEN total_tokens ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN substr(created_at, 1, 10) >= ?2 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN substr(created_at, 1, 10) >= ?2 THEN total_tokens ELSE 0 END), 0)
         FROM request_logs
         WHERE substr(created_at, 1, 10) >= ?3",
        params![today_s, week_s, month_s],
        |row| {
            Ok((
                row.get::<_, i64>(0)?, row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?, row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?, row.get::<_, i64>(5)?,
            ))
        },
    );

    // 每日序列:请求数、Token、平均延迟、成功率
    let daily_rows = conn
        .prepare(
            "SELECT substr(created_at, 1, 10) AS d, COUNT(*), COALESCE(SUM(total_tokens), 0),
                    COALESCE(AVG(latency_ms), 0),
                    COALESCE(SUM(CASE WHEN success = 1 THEN 1 ELSE 0 END) * 100.0 / NULLIF(COUNT(*), 0), 0)
             FROM request_logs
             WHERE substr(created_at, 1, 10) >= ?1
             GROUP BY d",
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![month_s], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, f64>(4)?,
                ))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
        });

    // Top 模型(近 30 天,按请求数排序)
    let model_rows = conn
        .prepare(
            "SELECT model, COUNT(*) AS c, COALESCE(SUM(total_tokens), 0)
             FROM request_logs
             WHERE substr(created_at, 1, 10) >= ?1
             GROUP BY model ORDER BY c DESC LIMIT 8",
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![month_s], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
        });

    let (Ok((month_req, month_tok, today_req, today_tok, week_req, week_tok)), Ok(daily_rows), Ok(model_rows)) =
        (totals, daily_rows, model_rows)
    else {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
            "error": "query_failed",
            "message": "Internal server error"
        }))).into_response();
    };

    // 补齐缺失日期为 0,保证图表是完整的 30 天序列
    let daily_map: std::collections::HashMap<String, (i64, i64, f64, f64)> =
        daily_rows.into_iter().map(|(d, r, t, lat, sr)| (d, (r, t, lat, sr))).collect();
    let daily: Vec<serde_json::Value> = (0..30)
        .map(|i| {
            let d = fmt(today - chrono::Duration::days(29 - i));
            let (r, t, lat, sr) = daily_map.get(&d).copied().unwrap_or((0, 0, 0.0, 0.0));
            json!({
                "date": d,
                "requests": r,
                "tokens": t,
                "avg_latency_ms": lat.round() as i64,
                "success_rate": (sr * 100.0).round() / 100.0,
            })
        })
        .collect();

    let top_models: Vec<serde_json::Value> = model_rows
        .into_iter()
        .map(|(model, requests, tokens)| json!({
            "model": model,
            "requests": requests,
            "tokens": tokens,
        }))
        .collect();

    (StatusCode::OK, Json(json!({
        "today": {"requests": today_req, "tokens": today_tok},
        "week": {"requests": week_req, "tokens": week_tok},
        "month": {"requests": month_req, "tokens": month_tok},
        "daily": daily,
        "top_models": top_models,
    }))).into_response()
}