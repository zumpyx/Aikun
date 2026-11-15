//! 记账:请求日志 + 计费扣款。日志插入、价格折算、余额扣减在同一个
//! SQLite 事务里完成(整数微元,无浮点漂移)。

use std::sync::Arc;

use rusqlite::params;
use tracing::warn;
use uuid::Uuid;

use crate::db::DbPool;

/// Fire-and-forget 版本：异步上下文写请求日志时投递到 blocking 线程池，
/// 避免持 DB mutex 的同步写阻塞 async executor。用于失败路径与流中途
/// 断连(StreamAbortGuard)等无法 await 的场景;成功计费路径必须改用
/// request_log_sync 等待落库,进程退出时 fire-and-forget 会丢账。
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_request_log(
    pool: &Arc<DbPool>,
    user_id: String,
    api_key_id: Option<String>,
    provider_id: Option<String>,
    model: String,
    request_type: &str,
    prompt_tokens: i32,
    completion_tokens: i32,
    total_tokens: i32,
    cached_tokens: i32,
    latency_ms: i32,
    status_code: i32,
    success: bool,
    error_message: Option<String>,
) {
    let pool = pool.clone();
    let request_type = request_type.to_string();
    tokio::task::spawn_blocking(move || {
        insert_request_log(
            &pool, &user_id, api_key_id.as_deref(), provider_id.as_deref(),
            &model, &request_type, prompt_tokens, completion_tokens, total_tokens,
            cached_tokens, latency_ms, status_code, success, error_message,
        );
    });
}

/// 计费级同步版本:调用方 await,函数返回时日志与扣费已落库。
/// 用于非流式成功路径——响应返回给用户前费用必须已记账。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn request_log_sync(
    pool: &Arc<DbPool>,
    user_id: String,
    api_key_id: Option<String>,
    provider_id: Option<String>,
    model: String,
    request_type: &str,
    prompt_tokens: i32,
    completion_tokens: i32,
    total_tokens: i32,
    cached_tokens: i32,
    latency_ms: i32,
    status_code: i32,
    success: bool,
    error_message: Option<String>,
) {
    let pool = pool.clone();
    let request_type = request_type.to_string();
    if let Err(e) = tokio::task::spawn_blocking(move || {
        insert_request_log(
            &pool, &user_id, api_key_id.as_deref(), provider_id.as_deref(),
            &model, &request_type, prompt_tokens, completion_tokens, total_tokens,
            cached_tokens, latency_ms, status_code, success, error_message,
        );
    })
    .await
    {
        // JoinError 意味着 blocking 任务 panic/被取消:日志与扣费可能未落库。
        warn!("request log blocking task failed to join: {}", e);
    }
}

/// 命中响应缓存时的记账:provider_id 为 NULL(未触达任何上游)、
/// status 200。`billable = false`(AIKUN_CACHE_HIT_BILLING=free)时只记日志
/// 与用量、不扣费。调用方 await,返回时已落库。
pub(crate) async fn record_cache_hit(
    pool: &Arc<DbPool>,
    user_id: String,
    api_key_id: Option<String>,
    model: String,
    request_type: &str,
    usage: (i32, i32, i32, i32),
    billable: bool,
) {
    let pool = pool.clone();
    let request_type = request_type.to_string();
    let (p, c, t, k) = usage;
    if let Err(e) = tokio::task::spawn_blocking(move || {
        insert_request_log_ext(
            &pool, &user_id, api_key_id.as_deref(), None, &model, &request_type,
            p, c, t, k, 0, 200, true, None, billable,
        );
    })
    .await
    {
        warn!("cache-hit log blocking task failed to join: {}", e);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_request_log(
    pool: &DbPool,
    user_id: &str,
    api_key_id: Option<&str>,
    provider_id: Option<&str>,
    model: &str,
    request_type: &str,
    prompt_tokens: i32,
    completion_tokens: i32,
    total_tokens: i32,
    cached_tokens: i32,
    latency_ms: i32,
    status_code: i32,
    success: bool,
    error_message: Option<String>,
) {
    insert_request_log_ext(
        pool, user_id, api_key_id, provider_id, model, request_type, prompt_tokens,
        completion_tokens, total_tokens, cached_tokens, latency_ms, status_code, success,
        error_message, true,
    )
}

#[allow(clippy::too_many_arguments)]
fn insert_request_log_ext(
    pool: &DbPool,
    user_id: &str,
    api_key_id: Option<&str>,
    provider_id: Option<&str>,
    model: &str,
    request_type: &str,
    prompt_tokens: i32,
    completion_tokens: i32,
    total_tokens: i32,
    cached_tokens: i32,
    latency_ms: i32,
    status_code: i32,
    success: bool,
    error_message: Option<String>,
    billable: bool,
) {
    let mut conn = match pool.conn.lock() {
        Ok(conn) => conn,
        Err(e) => {
            // 锁中毒:日志与扣费整体丢失,必须可见。
            warn!("Failed to lock DB connection for request log: {}", e);
            return;
        }
    };
    {
        // 计费:有用量即按价格表折算费用(整数微元)。上游 token 已真实消耗,
        // 客户端中途断连(499)或流中途错误(502)时已捕获的 usage 照扣;
        // 429/400/上游错误路径 total_tokens 本就为 0,不受影响。
        // 价格查询、日志插入、余额扣减包在同一个显式事务里——任何一步
        // 失败整体回滚,不会留下"记了日志没扣费"或"扣了费没日志"的中间态。
        let result = (|| -> Result<(), rusqlite::Error> {
            let cost = if billable && total_tokens > 0 {
                match crate::billing::find_price(&conn, model) {
                    Some(prices) => crate::billing::compute_cost(prompt_tokens, cached_tokens, completion_tokens, prices),
                    None => {
                        // 有用量但无价格配置:静默免费必须可见。
                        warn!(
                            "No price configured for model '{}' — {} tokens billed at 0",
                            model, total_tokens
                        );
                        0
                    }
                }
            } else {
                0
            };
            let tx = conn.transaction()?;
            // 渠道/API key 可能在长请求(尤其流式)进行中被硬删除:dangling
            // 引用会让 INSERT 触发 FK 约束、整个记账事务回滚——日志与扣费全丢。
            // 落库前把已不存在的引用降级为 NULL,只丢失维度归属。
            let fk_downgrade = |table: &str, id: Option<&str>| -> Option<String> {
                id.and_then(|v| {
                    let exists: bool = tx
                        .query_row(
                            &format!("SELECT EXISTS(SELECT 1 FROM {} WHERE id = ?1)", table),
                            params![v],
                            |row| row.get(0),
                        )
                        .unwrap_or(true);
                    if exists {
                        Some(v.to_string())
                    } else {
                        warn!(
                            "Request log: {} {} was deleted mid-request — storing NULL",
                            table, v
                        );
                        None
                    }
                })
            };
            let provider_id = fk_downgrade("providers", provider_id);
            let api_key_id = fk_downgrade("api_keys", api_key_id);
            tx.execute(
                "INSERT INTO request_logs (id, user_id, api_key_id, provider_id, model, request_type,
                 prompt_tokens, completion_tokens, total_tokens, cached_tokens, latency_ms, status_code, success, error_message, cost, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    Uuid::new_v4().to_string(),
                    Some(user_id),
                    api_key_id,
                    provider_id,
                    model,
                    request_type,
                    prompt_tokens,
                    completion_tokens,
                    total_tokens,
                    cached_tokens,
                    latency_ms,
                    status_code,
                    success as i32,
                    error_message,
                    cost,
                    chrono::Utc::now().to_rfc3339(),
                ],
            )?;
            // 余额不足在请求入口已预检拦截(402);此处允许单请求扣成负数
            // (用量先发生、费用后结算,不做预留金),整数微元扣减无浮点误差。
            if cost > 0 {
                tx.execute(
                    "UPDATE users SET balance = balance - ?1 WHERE id = ?2",
                    params![cost, user_id],
                )?;
            }
            tx.commit()
        })();
        // 记账失败必须可见:此前静默吞错曾导致迁移中间态下日志全丢。
        if let Err(e) = result {
            warn!("Failed to record request log / billing: {}", e);
        }
    }
}
