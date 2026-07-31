//! 计费管理:模型价格表 CRUD、用户余额调账、调账流水查询。
//! 全部挂 admin_routes(require_admin)。价格单位:每 1M tokens(元)。

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::AppState;

fn row_to_price(row: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "model": row.get::<_, String>(1)?,
        "prompt_price": row.get::<_, f64>(2)?,
        "completion_price": row.get::<_, f64>(3)?,
        "created_at": row.get::<_, String>(4)?,
        "updated_at": row.get::<_, String>(5)?,
    }))
}

const PRICE_COLS: &str = "id, model, prompt_price, completion_price, created_at, updated_at";

pub async fn list_prices(State(state): State<AppState>) -> impl IntoResponse {
    let conn = match state.pool.read().lock() {
        Ok(c) => c,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"})))
        }
    };
    let result = conn
        .prepare(&format!(
            "SELECT {} FROM model_prices ORDER BY model",
            PRICE_COLS
        ))
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], row_to_price)?;
            Ok(rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
        });
    match result {
        Ok(prices) => (StatusCode::OK, Json(json!(prices))),
        Err(e) => {
            tracing::error!("Failed to list prices: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "query_failed"})))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UpsertPriceRequest {
    pub model: Option<String>,
    pub prompt_price: Option<f64>,
    pub completion_price: Option<f64>,
}

/// 校验:model 非空(创建时必填),价格有限且非负;通配条目的 *
/// 只允许出现在末尾且前缀非空(否则永远不命中,属于配置错误)。
fn valid_price_fields(req: &UpsertPriceRequest, creating: bool) -> Result<(), (StatusCode, Json<Value>)> {
    let model = req.model.as_deref().map(|m| m.trim());
    if creating && model.unwrap_or("").is_empty() {
        return Err((StatusCode::BAD_REQUEST, Json(json!({
            "error": "invalid_model",
            "message": "model 不能为空"
        }))));
    }
    if let Some(m) = model {
        if m.is_empty() {
            return Err((StatusCode::BAD_REQUEST, Json(json!({
                "error": "invalid_model",
                "message": "model 不能为空"
            }))));
        }
        if m.contains('*') {
            let prefix = m.trim_end_matches('*');
            if !m.ends_with('*') || prefix.contains('*') || prefix.is_empty() {
                return Err((StatusCode::BAD_REQUEST, Json(json!({
                    "error": "invalid_model",
                    "message": "通配只支持末尾单个 *,且前缀非空(如 gpt-*)"
                }))));
            }
        }
    }
    for v in [req.prompt_price, req.completion_price].into_iter().flatten() {
        if !v.is_finite() || v < 0.0 {
            return Err((StatusCode::BAD_REQUEST, Json(json!({
                "error": "invalid_price",
                "message": "价格必须是非负有限数"
            }))));
        }
    }
    Ok(())
}

pub async fn create_price(
    State(state): State<AppState>,
    Json(req): Json<UpsertPriceRequest>,
) -> impl IntoResponse {
    if let Err(resp) = valid_price_fields(&req, true) {
        return resp;
    }
    let model = req.model.unwrap_or_default().trim().to_string();
    let id = Uuid::new_v4().to_string();
    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"})))
        }
    };
    let result = conn.execute(
        "INSERT INTO model_prices (id, model, prompt_price, completion_price) VALUES (?1, ?2, ?3, ?4)",
        params![
            id,
            model,
            req.prompt_price.unwrap_or(0.0),
            req.completion_price.unwrap_or(0.0)
        ],
    );
    match result {
        Ok(_) => {
            let row = conn.query_row(
                &format!("SELECT {} FROM model_prices WHERE id = ?1", PRICE_COLS),
                params![id],
                row_to_price,
            );
            match row {
                Ok(v) => (StatusCode::CREATED, Json(v)),
                Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "query_failed"}))),
            }
        }
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            (StatusCode::CONFLICT, Json(json!({
                "error": "duplicate_model",
                "message": "该模型(或通配)的价格已存在,请直接编辑"
            })))
        }
        Err(e) => {
            tracing::error!("Failed to create price: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "create_failed"})))
        }
    }
}

pub async fn update_price(
    State(state): State<AppState>,
    Path(price_id): Path<String>,
    Json(req): Json<UpsertPriceRequest>,
) -> impl IntoResponse {
    if let Err(resp) = valid_price_fields(&req, false) {
        return resp;
    }
    let mut updates = Vec::new();
    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(model) = &req.model {
        let m = model.trim().to_string();
        if m.is_empty() {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "error": "invalid_model",
                "message": "model 不能为空"
            })));
        }
        updates.push("model = ?");
        params_vec.push(Box::new(m));
    }
    if let Some(p) = req.prompt_price {
        updates.push("prompt_price = ?");
        params_vec.push(Box::new(p));
    }
    if let Some(c) = req.completion_price {
        updates.push("completion_price = ?");
        params_vec.push(Box::new(c));
    }
    if updates.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "no_fields_to_update"})));
    }
    updates.push("updated_at = datetime('now')");

    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"})))
        }
    };
    let sql = format!("UPDATE model_prices SET {} WHERE id = ?", updates.join(", "));
    let mut params: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(|p| p.as_ref()).collect();
    params.push(&price_id);
    match conn.execute(&sql, params.as_slice()) {
        Ok(n) if n > 0 => {
            let row = conn.query_row(
                &format!("SELECT {} FROM model_prices WHERE id = ?1", PRICE_COLS),
                params![price_id],
                row_to_price,
            );
            match row {
                Ok(v) => (StatusCode::OK, Json(v)),
                Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "query_failed"}))),
            }
        }
        Ok(_) => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            (StatusCode::CONFLICT, Json(json!({
                "error": "duplicate_model",
                "message": "该模型(或通配)的价格已存在"
            })))
        }
        Err(e) => {
            tracing::error!("Failed to update price: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "update_failed"})))
        }
    }
}

pub async fn delete_price(
    State(state): State<AppState>,
    Path(price_id): Path<String>,
) -> impl IntoResponse {
    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"})))
        }
    };
    match conn.execute("DELETE FROM model_prices WHERE id = ?1", params![price_id]) {
        Ok(n) if n > 0 => (StatusCode::OK, Json(json!({"message": "deleted"}))),
        Ok(_) => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
        Err(e) => {
            tracing::error!("Failed to delete price: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "delete_failed"})))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AdjustBalanceRequest {
    pub amount: f64,
    pub note: Option<String>,
}

/// 管理员手工调账:amount 正=充值负=扣减。UPDATE 余额与 INSERT 流水
/// 同在一把写锁内,不会只改一边。
pub async fn adjust_balance(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    Json(req): Json<AdjustBalanceRequest>,
) -> impl IntoResponse {
    if !req.amount.is_finite() || req.amount == 0.0 {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "invalid_amount",
            "message": "amount 必须是非零有限数"
        })));
    }
    let mut conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"})))
        }
    };
    // UPDATE 余额与 INSERT 流水包在同一事务:失败整体回滚,
    // 不会留下"余额变了但没流水"的中间态。
    let result = (|| -> Result<(f64, &'static str), rusqlite::Error> {
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE users SET balance = balance + ?1, updated_at = datetime('now') WHERE id = ?2",
            params![req.amount, user_id],
        )?;
        if updated == 0 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        let balance: f64 = tx.query_row(
            "SELECT balance FROM users WHERE id = ?1",
            params![user_id],
            |row| row.get(0),
        )?;
        let kind = if req.amount > 0.0 { "recharge" } else { "adjust" };
        tx.execute(
            "INSERT INTO billing_transactions (id, user_id, amount, balance_after, kind, note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                Uuid::new_v4().to_string(),
                user_id,
                req.amount,
                balance,
                kind,
                req.note.unwrap_or_default().trim().to_string()
            ],
        )?;
        tx.commit()?;
        Ok((balance, kind))
    })();
    match result {
        Ok((balance, kind)) => (StatusCode::OK, Json(json!({
            "balance": balance,
            "kind": kind
        }))),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"})))
        }
        Err(e) => {
            tracing::error!("Failed to adjust balance: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "update_failed"})))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TransactionQuery {
    pub user_id: Option<String>,
    pub offset: Option<i64>,
}

pub async fn list_transactions(
    State(state): State<AppState>,
    Query(query): Query<TransactionQuery>,
) -> impl IntoResponse {
    let conn = match state.pool.read().lock() {
        Ok(c) => c,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"})))
        }
    };
    let uid = query.user_id.as_deref().unwrap_or("").trim();
    // 分页:limit 固定 200,offset 防负(前端"加载更多"按页累加)。
    let offset = query.offset.unwrap_or(0).max(0);
    let (sql, p): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = if uid.is_empty() {
        (
            "SELECT t.id, t.user_id, u.username, t.amount, t.balance_after, t.kind, t.note, t.created_at
             FROM billing_transactions t LEFT JOIN users u ON u.id = t.user_id
             ORDER BY t.created_at DESC, t.rowid DESC LIMIT 200 OFFSET ?"
                .to_string(),
            vec![Box::new(offset)],
        )
    } else {
        (
            "SELECT t.id, t.user_id, u.username, t.amount, t.balance_after, t.kind, t.note, t.created_at
             FROM billing_transactions t LEFT JOIN users u ON u.id = t.user_id
             WHERE t.user_id = ? ORDER BY t.created_at DESC, t.rowid DESC LIMIT 200 OFFSET ?"
                .to_string(),
            vec![Box::new(uid.to_string()), Box::new(offset)],
        )
    };
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = p.iter().map(|x| x.as_ref()).collect();
    let result = conn.prepare(&sql).and_then(|mut stmt| {
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "user_id": row.get::<_, String>(1)?,
                "username": row.get::<_, Option<String>>(2)?,
                "amount": row.get::<_, f64>(3)?,
                "balance_after": row.get::<_, f64>(4)?,
                "kind": row.get::<_, String>(5)?,
                "note": row.get::<_, String>(6)?,
                "created_at": row.get::<_, String>(7)?,
            }))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
    });
    match result {
        Ok(list) => (StatusCode::OK, Json(json!(list))),
        Err(e) => {
            tracing::error!("Failed to list billing transactions: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "query_failed"})))
        }
    }
}
