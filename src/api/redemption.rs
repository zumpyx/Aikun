//! 兑换码:管理员批量生成,用户在钱包页兑换充值。
//! 库中只存 sha256(code) 与掩码后缀,明文仅在生成响应中返回一次;
//! 金额口径与 users.balance 一致(整数微元),对外 API 为元。

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use rand::Rng;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::AppState;
use crate::auth::Claims;

/// 码字符集:去除易混淆的 0/O/1/I,32 字符 × 16 位 = 2^80 组合。
const CODE_CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// 兑换失败限流:同一用户 10 分钟内失败 5 次即拒绝(防爆破)。
const REDEEM_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);
const REDEEM_MAX_FAILS: usize = 5;

/// 生成 AK-XXXX-XXXX-XXXX-XXXX 形式的兑换码(明文,仅生成响应可见)。
fn generate_code() -> String {
    let mut rng = rand::rng();
    let pick = |rng: &mut rand::rngs::ThreadRng| {
        CODE_CHARSET[rng.random_range(0..CODE_CHARSET.len())] as char
    };
    let seg = |rng: &mut rand::rngs::ThreadRng| (0..4).map(|_| pick(rng)).collect::<String>();
    format!(
        "AK-{}-{}-{}-{}",
        seg(&mut rng),
        seg(&mut rng),
        seg(&mut rng),
        seg(&mut rng)
    )
}

fn hash_code(code: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(code.as_bytes()))
}

/// 兑换限流计数与登录限流同构(内存滑动窗口,重启清零可接受)。
fn redeem_limited(state: &AppState, user_id: &str) -> bool {
    let now = std::time::Instant::now();
    let mut attempts = state.redeem_attempts.lock().unwrap_or_else(|e| e.into_inner());
    attempts.retain(|_, v| {
        v.retain(|t| now.duration_since(*t) < REDEEM_WINDOW);
        !v.is_empty()
    });
    attempts.get(user_id).is_some_and(|v| v.len() >= REDEEM_MAX_FAILS)
}

fn record_redeem_failure(state: &AppState, user_id: &str) {
    let mut attempts = state.redeem_attempts.lock().unwrap_or_else(|e| e.into_inner());
    attempts
        .entry(user_id.to_string())
        .or_default()
        .push(std::time::Instant::now());
}

fn clear_redeem_failures(state: &AppState, user_id: &str) {
    let mut attempts = state.redeem_attempts.lock().unwrap_or_else(|e| e.into_inner());
    attempts.remove(user_id);
}

#[derive(Debug, Deserialize)]
pub struct CreateCodesRequest {
    pub count: Option<u32>,
    /// 单张面值(元),入库转整数微元。
    pub amount: Option<f64>,
    pub batch: Option<String>,
    /// 过期时间(UTC):"YYYY-MM-DD" 或 "YYYY-MM-DD HH:MM:SS",空 = 永不过期。
    pub expires_at: Option<String>,
    pub note: Option<String>,
}

/// 规范化过期时间为 SQLite datetime 可比格式;空 → None(永不过期)。
/// 只接受日期或"日期 时分秒",拒绝 RFC3339('T' 与空格序位不同,
/// 与 datetime('now') 直接字符串比较会出错)。
fn normalize_expires_at(raw: Option<&str>) -> Result<Option<String>, (StatusCode, Json<Value>)> {
    let bad = || {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_expires_at",
                "message": "expires_at 只支持 YYYY-MM-DD 或 YYYY-MM-DD HH:MM:SS(UTC)"
            })),
        )
    };
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    // 形状之外还要校验取值范围:非法的月/日/时/分/秒会让与
    // datetime('now') 的字符串比较语义错乱(如 "99:99:99" 恒大于
    // now,实际永不过期)。日上限按 31 粗校验,不逐月历精确。
    let num = |r: &str| r.parse::<u32>().unwrap_or(u32::MAX);
    let valid_date = |d: &str| {
        d.len() == 10
            && d.as_bytes()[4] == b'-'
            && d.as_bytes()[7] == b'-'
            && d.bytes()
                .enumerate()
                .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit())
            && (1..=12).contains(&num(&d[5..7]))
            && (1..=31).contains(&num(&d[8..10]))
    };
    let valid_time = |t: &str| {
        t.len() == 8
            && t.as_bytes()[2] == b':'
            && t.as_bytes()[5] == b':'
            && t.bytes()
                .enumerate()
                .all(|(i, b)| matches!(i, 2 | 5) || b.is_ascii_digit())
            && num(&t[0..2]) <= 23
            && num(&t[3..5]) <= 59
            && num(&t[6..8]) <= 59
    };
    if valid_date(s) {
        return Ok(Some(format!("{} 23:59:59", s)));
    }
    if s.len() == 19 && s.as_bytes()[10] == b' ' && valid_date(&s[..10]) && valid_time(&s[11..]) {
        return Ok(Some(s.to_string()));
    }
    Err(bad())
}

pub async fn create_redemption_codes(
    State(state): State<AppState>,
    Json(req): Json<CreateCodesRequest>,
) -> impl IntoResponse {
    let count = req.count.unwrap_or(0);
    if count == 0 || count > 500 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_count", "message": "count 须在 1..=500 之间"})),
        );
    }
    let amount = req.amount.unwrap_or(0.0);
    if !amount.is_finite() || amount <= 0.0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_amount", "message": "amount 必须是正数(元)"})),
        );
    }
    // 面值上限:浮点饱和转换会把 1e20 这类输入静默变成约 92 万亿元面值。
    if amount > 1_000_000.0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_amount", "message": "amount 不能超过 1,000,000 元"})),
        );
    }
    let amount_micro = crate::billing::yuan_to_micro(amount);
    if amount_micro <= 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_amount", "message": "amount 低于最小记账单位(0.000001 元)"})),
        );
    }
    let batch = req.batch.unwrap_or_default().trim().to_string();
    if batch.is_empty() || batch.chars().count() > 64 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_batch", "message": "batch 不能为空且不超过 64 字符"})),
        );
    }
    let expires_at = match normalize_expires_at(req.expires_at.as_deref()) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let note = req.note.unwrap_or_default().trim().to_string();

    // 明文码只在内存中组装进响应;库中只落 sha256 与掩码后缀。
    let codes: Vec<String> = (0..count).map(|_| generate_code()).collect();
    let mut conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal_error"})),
            )
        }
    };
    let result = (|| -> Result<(), rusqlite::Error> {
        // 整批一个事务:中途失败整体回滚。部分提交会把前 N 条的哈希
        // 留在库里而明文从未送达管理员,这批码永远无法兑换。
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "INSERT INTO redemption_codes (id, code_hash, code_suffix, amount, batch, expires_at, note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for code in &codes {
            stmt.execute(params![
                Uuid::new_v4().to_string(),
                hash_code(code),
                // 掩码展示用后缀(AK-XXX-XXX-XXXX 的末 4 位)
                &code[code.len() - 4..],
                amount_micro,
                batch,
                expires_at,
                note,
            ])?;
        }
        drop(stmt);
        tx.commit()
    })();
    match result {
        Ok(()) => (
            StatusCode::CREATED,
            Json(json!({
                "batch": batch,
                "amount": crate::billing::micro_to_yuan(amount_micro),
                "count": codes.len(),
                "codes": codes,
            })),
        ),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            // 2^80 空间下碰撞几乎不可能,真撞上重试一次生成即可,直接报错
            // 让管理员重新提交,不做静默丢弃。
            tracing::error!("Redemption code collision on insert (retry the request)");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "code_collision", "message": "码生成碰撞,请重试"})),
            )
        }
        Err(e) => {
            tracing::error!("Failed to create redemption codes: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "create_failed"})),
            )
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ListCodesQuery {
    pub batch: Option<String>,
    pub status: Option<String>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
}

pub async fn list_redemption_codes(
    State(state): State<AppState>,
    Query(query): Query<ListCodesQuery>,
) -> impl IntoResponse {
    let status = query.status.as_deref().unwrap_or("").trim().to_string();
    if !matches!(status.as_str(), "" | "unused" | "used" | "disabled" | "expired") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_status", "message": "status 只支持 unused/used/disabled/expired"})),
        );
    }
    let batch = query.batch.as_deref().unwrap_or("").trim().to_string();
    let offset = query.offset.unwrap_or(0).max(0);
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let conn = match state.pool.read().lock() {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal_error"})),
            )
        }
    };
    // expired 是显示态:库中 status 仍为 unused,按过期时间与当前时间判定。
    // 下面的状态片段只做白名单内的固定字符串拼接,无注入面。
    let status_sql = match status.as_str() {
        "" => String::new(),
        "expired" => " AND c.status = 'unused' AND c.expires_at IS NOT NULL AND c.expires_at <= datetime('now')".to_string(),
        s => format!(" AND c.status = '{}'", s),
    };
    let where_sql = format!("(?1 = '' OR c.batch = ?1){}", status_sql);
    // 永不出库明文/哈希:列表只给掩码后缀。expired 由库内时间计算,
    // 供前端把"过期未使用"显示为已过期(status 本身保持 unused)。
    let result = (|| -> Result<(i64, Vec<Value>), rusqlite::Error> {
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM redemption_codes c WHERE {}", where_sql),
            params![batch],
            |row| row.get(0),
        )?;
        let mut stmt = conn.prepare(&format!(
            "SELECT c.id, c.code_suffix, c.amount, c.batch, c.status, u.username,
                    c.used_at, c.expires_at, c.note, c.created_at,
                    (c.expires_at IS NOT NULL AND c.expires_at <= datetime('now'))
             FROM redemption_codes c LEFT JOIN users u ON u.id = c.used_by
             WHERE {}
             ORDER BY c.created_at DESC, c.rowid DESC LIMIT ?2 OFFSET ?3",
            where_sql
        ))?;
        let rows = stmt.query_map(params![batch, limit, offset], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "code_masked": format!("AK-****-****-****-{}", row.get::<_, String>(1)?),
                "amount": crate::billing::micro_to_yuan(row.get::<_, i64>(2)?),
                "batch": row.get::<_, String>(3)?,
                "status": row.get::<_, String>(4)?,
                "used_by": row.get::<_, Option<String>>(5)?,
                "used_at": row.get::<_, Option<String>>(6)?,
                "expires_at": row.get::<_, Option<String>>(7)?,
                "note": row.get::<_, String>(8)?,
                "created_at": row.get::<_, String>(9)?,
                "expired": row.get::<_, bool>(10)?,
            }))
        })?;
        Ok((total, rows.filter_map(|r| r.ok()).collect::<Vec<_>>()))
    })();
    match result {
        Ok((total, items)) => (StatusCode::OK, Json(json!({"total": total, "items": items}))),
        Err(e) => {
            tracing::error!("Failed to list redemption codes: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "query_failed"})),
            )
        }
    }
}

pub async fn disable_redemption_code(
    State(state): State<AppState>,
    Path(code_id): Path<String>,
) -> impl IntoResponse {
    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal_error"})),
            )
        }
    };
    // 只有未使用的码可禁用;used/disabled 返回 409,不存在返回 404。
    match conn.execute(
        "UPDATE redemption_codes SET status = 'disabled' WHERE id = ?1 AND status = 'unused'",
        params![code_id],
    ) {
        Ok(1) => (StatusCode::OK, Json(json!({"message": "disabled"}))),
        Ok(_) => {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM redemption_codes WHERE id = ?1)",
                    params![code_id],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if exists {
                (
                    StatusCode::CONFLICT,
                    Json(json!({"error": "not_unused", "message": "只有未使用的兑换码可以禁用"})),
                )
            } else {
                (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"})))
            }
        }
        Err(e) => {
            tracing::error!("Failed to disable redemption code: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "update_failed"})),
            )
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RedeemRequest {
    pub code: String,
}

/// 用户兑换:标定码 → 加余额 → 记流水,三步在同一事务。
/// 码校验与核销用 UPDATE...WHERE status='unused' 的影响行数判竞争:
/// 并发兑换同一码只有一方生效,失败方按"已使用"处理。
pub async fn redeem_code(
    State(state): State<AppState>,
    claims: Claims,
    Json(req): Json<RedeemRequest>,
) -> impl IntoResponse {
    if redeem_limited(&state, &claims.sub) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "rate_limited", "message": "失败次数过多,请 10 分钟后再试"})),
        );
    }
    // 归一化:去空白、统一大写(输入允许小写/多空格)
    let code = req.code.split_whitespace().collect::<String>().to_uppercase();
    let invalid = |state: &AppState| {
        record_redeem_failure(state, &claims.sub);
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_code", "message": "兑换码无效、已使用或已过期"})),
        )
    };
    if !code.starts_with("AK-") || code.len() > 32 {
        return invalid(&state);
    }
    let code_hash = hash_code(&code);

    let mut conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal_error"})),
            )
        }
    };
    let result = (|| -> Result<Option<(i64, i64)>, rusqlite::Error> {
        let tx = conn.transaction()?;
        // 核销与加余额同事务:UPDATE 的 WHERE 同时覆盖状态与过期校验,
        // 影响行数 = 0 即"无效/已用/已禁用/已过期",不细分(防枚举)。
        let n = tx.execute(
            "UPDATE redemption_codes SET status = 'used', used_by = ?1, used_at = datetime('now')
             WHERE code_hash = ?2 AND status = 'unused'
               AND (expires_at IS NULL OR expires_at > datetime('now'))",
            params![claims.sub, code_hash],
        )?;
        if n == 0 {
            return Ok(None);
        }
        let amount: i64 = tx.query_row(
            "SELECT amount FROM redemption_codes WHERE code_hash = ?1",
            params![code_hash],
            |row| row.get(0),
        )?;
        tx.execute(
            "UPDATE users SET balance = balance + ?1, updated_at = datetime('now') WHERE id = ?2",
            params![amount, claims.sub],
        )?;
        let balance: i64 = tx.query_row(
            "SELECT balance FROM users WHERE id = ?1",
            params![claims.sub],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO billing_transactions (id, user_id, amount, balance_after, kind, note)
             VALUES (?1, ?2, ?3, ?4, 'recharge', '兑换码充值')",
            params![Uuid::new_v4().to_string(), claims.sub, amount, balance],
        )?;
        tx.commit()?;
        Ok(Some((amount, balance)))
    })();
    match result {
        Ok(Some((amount, balance))) => {
            clear_redeem_failures(&state, &claims.sub);
            (
                StatusCode::OK,
                Json(json!({
                    "amount": crate::billing::micro_to_yuan(amount),
                    "balance": crate::billing::micro_to_yuan(balance),
                })),
            )
        }
        Ok(None) => invalid(&state),
        Err(e) => {
            tracing::error!("Failed to redeem code: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "redeem_failed"})),
            )
        }
    }
}
