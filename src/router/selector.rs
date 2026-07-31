use std::collections::HashSet;
use std::sync::Arc;

use rand::Rng;
use rusqlite::{Connection, params};

use crate::db::DbPool;
use crate::models::{row_to_provider, Provider};

/// Check whether a provider can serve the requested model. The models list
/// only matches exactly; model_mapping keys (requested → upstream) match by
/// prefix, consistent with how the mapping is applied later.
fn provider_supports_model(p: &Provider, model: &str) -> bool {
    if let Ok(models) = serde_json::from_str::<Vec<String>>(&p.models) {
        if models.iter().any(|m| m == model) {
            return true;
        }
    }
    if let Ok(mapping) =
        serde_json::from_str::<std::collections::HashMap<String, String>>(&p.model_mapping)
    {
        if mapping.keys().any(|k| !k.is_empty() && model.starts_with(k.as_str())) {
            return true;
        }
    }
    false
}

/// 非有限值防御：持久化数据或计算结果中的 inf/NaN 一律回退安全值，
/// 避免 random_range(0.0..inf) panic。
fn finite_or(v: f64, fallback: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}

/// Score a candidate provider: lower latency, higher priority/weight, better
/// health and lower error rate all increase the score.
fn score_provider(p: &Provider, max_latency: f64) -> f64 {
    let latency = finite_or(p.latency_ms, 0.0);
    let latency_factor = if latency > 0.0 {
        // 比值 clamp：避免极端延迟差把评分拉爆
        (max_latency / latency).min(10.0)
    } else {
        1.0
    };
    let priority_factor = (p.priority.clamp(0, 100) + 10) as f64; // priority 0-100, shift to avoid zero
    let weight_factor = finite_or(p.weight, 0.1).clamp(0.1, 1000.0);
    let health_factor = if p.health_status == "healthy" {
        1.0
    } else if p.health_status == "degraded" {
        0.5
    } else {
        0.2
    };
    let error_factor = (1.0 - finite_or(p.error_rate, 1.0)).clamp(0.0, 1.0);
    latency_factor * priority_factor * weight_factor * health_factor * error_factor
}

/// Weighted random selection among candidates based on `score_provider`.
fn weighted_pick(candidates: &[&Provider]) -> Option<Provider> {
    if candidates.is_empty() {
        return None;
    }
    let max_latency = candidates
        .iter()
        .map(|p| finite_or(p.latency_ms, 0.0))
        .fold(0.0f64, f64::max)
        .max(1.0);

    let scores: Vec<f64> = candidates
        .iter()
        .map(|p| score_provider(p, max_latency))
        .collect();

    let total_score: f64 = scores.iter().sum();
    if total_score <= 0.0 || !total_score.is_finite() {
        return Some(candidates[0].clone());
    }

    // Weighted random selection
    let mut rng = rand::rng();
    let mut pick = rng.random_range(0.0..total_score);
    for (i, score) in scores.iter().enumerate() {
        pick -= score;
        if pick <= 0.0 {
            return Some(candidates[i].clone());
        }
    }

    Some(candidates[candidates.len() - 1].clone())
}

/// Select the best provider for a given model, excluding already-tried ones.
/// Uses weighted random selection from healthy providers, preferring lower latency.
pub fn select_provider(
    pool: &DbPool,
    model: &str,
    exclude: &HashSet<String>,
) -> Option<Provider> {
    let conn = pool.read().lock().ok()?;
    let providers = find_providers_for_model(&conn, &pool.cipher, model)?;

    let mut candidates: Vec<&Provider> = providers
        .iter()
        .filter(|p| !exclude.contains(&p.id))
        .collect();
    if candidates.is_empty() {
        // All matching providers were tried — allow retrying any of them.
        candidates = providers.iter().collect();
    }
    if candidates.is_empty() {
        return None;
    }

    // Filter to only healthy/active providers
    let healthy: Vec<&Provider> = candidates
        .iter()
        .copied()
        .filter(|p| p.is_active && p.health_status != "unhealthy")
        .collect();

    if healthy.is_empty() {
        // Fall back to any active provider even if unhealthy — still using
        // weighted scoring instead of always picking the first one.
        let active: Vec<&Provider> = candidates
            .iter()
            .copied()
            .filter(|p| p.is_active)
            .collect();
        return weighted_pick(&active);
    }

    weighted_pick(&healthy)
}

/// Find all providers that support the given model.
/// api_key 在出库时解密(密文以 enc:v1: 为前缀,明文原样兼容)。
fn find_providers_for_model(
    conn: &Connection,
    cipher: &crate::crypto::KeyCipher,
    model: &str,
) -> Option<Vec<Provider>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, provider_type, openai_base_url, anthropic_base_url, api_key, models, priority, weight,
                    is_active, health_status, latency_ms, error_rate, last_health_check,
                    max_retries, timeout_secs, created_at, updated_at, proxy_url,
                    model_mapping, consecutive_failures, disabled_reason,
                    protocols, default_protocol, note, website_url
             FROM providers WHERE is_active = 1",
        )
        .ok()?;

    let providers: Vec<Provider> = stmt
        .query_map([], row_to_provider)
        .ok()?
        .filter_map(|r| r.ok())
        .map(|mut p| {
            p.api_key = crate::crypto::decrypt_or_plain(cipher, &p.api_key);
            p
        })
        .filter(|p| provider_supports_model(p, model))
        .collect();

    Some(providers)
}

/// List all unique models available across all active providers.
pub fn list_available_models(pool: &DbPool) -> Vec<String> {
    let conn = match pool.read().lock() {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let mut stmt = match conn.prepare(
        "SELECT models FROM providers WHERE is_active = 1 AND health_status != 'unhealthy'"
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    let mut models: Vec<String> = vec![];
    if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
        for row in rows.flatten() {
            if let Ok(ms) = serde_json::from_str::<Vec<String>>(&row) {
                for m in ms {
                    if !models.contains(&m) {
                        models.push(m);
                    }
                }
            }
        }
    }
    models.sort();
    models
}

/// Record a successful request: update latency/error-rate EMAs and reset the
/// consecutive failure counter.
pub fn record_request_result(
    pool: &DbPool,
    provider_id: &str,
    latency_ms: f64,
    success: bool,
) {
    let conn = match pool.conn.lock() {
        Ok(c) => c,
        Err(_) => return,
    };

    // Update moving average latency (exponential moving average)
    let _ = conn.execute(
        "UPDATE providers SET
            latency_ms = CASE
                WHEN latency_ms = 0 THEN ?1
                ELSE latency_ms * 0.9 + ?1 * 0.1
            END,
            error_rate = CASE
                WHEN ?2 = 1 THEN error_rate * 0.95
                ELSE error_rate * 0.95 + 0.05
            END,
            consecutive_failures = CASE WHEN ?2 = 1 THEN 0 ELSE consecutive_failures END
         WHERE id = ?3",
        params![latency_ms, success as i32, provider_id],
    );
}

/// Record a failed upstream attempt. Auto-disables the provider when:
/// - the upstream returned 401/403 (bad credentials — always dead), or
/// - consecutive failures reach `threshold`.
///
/// 健康口径:
/// - 传输错误(0)、408、429、5xx 视为上游失败,推高 error_rate;
/// - 其中除 429(上游限流,渠道本身可能健康)外累计 consecutive_failures;
/// - 其余 4xx(400/404/422)是客户端请求的错,与渠道健康无关——只更新
///   延迟、按成功方向衰减 error_rate,不计 consecutive_failures,避免
///   恶意/乱配客户端的坏请求把健康渠道打到自动禁用。
///
/// `count_consecutive`:同一请求内因重试反复打到同一渠道时,后续尝试传
/// false——latency/error_rate 照记,consecutive_failures 不重复累加,
/// 防止失败计数被重试放大导致误禁用。
pub fn record_failure(
    pool: &DbPool,
    provider_id: &str,
    latency_ms: f64,
    status: u16,
    threshold: u32,
    count_consecutive: bool,
) {
    let conn = match pool.conn.lock() {
        Ok(c) => c,
        Err(_) => return,
    };

    let is_upstream_failure = status == 0 || status == 408 || status == 429 || status >= 500;
    let counts_as_failure =
        (count_consecutive && is_upstream_failure && status != 429) as i32;
    let _ = conn.execute(
        "UPDATE providers SET
            latency_ms = CASE
                WHEN latency_ms = 0 THEN ?1
                ELSE latency_ms * 0.9 + ?1 * 0.1
            END,
            error_rate = CASE
                WHEN ?2 = 1 THEN error_rate * 0.95 + 0.05
                ELSE error_rate * 0.95
            END,
            consecutive_failures = consecutive_failures + ?3
         WHERE id = ?4",
        params![
            latency_ms,
            is_upstream_failure as i32,
            counts_as_failure,
            provider_id
        ],
    );

    let reason = if status == 401 || status == 403 {
        Some(format!("上游返回 {} 认证失败,自动禁用", status))
    } else if counts_as_failure == 1 {
        let failures: i32 = conn
            .query_row(
                "SELECT consecutive_failures FROM providers WHERE id = ?1",
                params![provider_id],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if failures >= threshold as i32 {
            Some(format!("连续失败 {} 次,自动禁用", failures))
        } else {
            None
        }
    } else {
        None
    };

    if let Some(reason) = reason {
        tracing::warn!("Auto-disabling provider {}: {}", provider_id, reason);
        let _ = conn.execute(
            "UPDATE providers SET is_active = 0, disabled_reason = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![reason, provider_id],
        );
    }
}
// ---- 异步包装:热路径(每请求的鉴权/选路/记账)不得在 async executor 上
// 同步持锁访问 SQLite,一律派发到 blocking 线程池(M35)。----

/// select_provider 的异步包装。
pub async fn select_provider_async(
    pool: Arc<DbPool>,
    model: String,
    exclude: HashSet<String>,
) -> Option<Provider> {
    tokio::task::spawn_blocking(move || select_provider(&pool, &model, &exclude))
        .await
        .ok()
        .flatten()
}

/// record_request_result 的 fire-and-forget 异步包装。
pub fn spawn_record_result(pool: Arc<DbPool>, provider_id: String, latency_ms: f64, success: bool) {
    tokio::task::spawn_blocking(move || {
        record_request_result(&pool, &provider_id, latency_ms, success);
    });
}

/// record_failure 的 fire-and-forget 异步包装。
pub fn spawn_record_failure(
    pool: Arc<DbPool>,
    provider_id: String,
    latency_ms: f64,
    status: u16,
    threshold: u32,
    count_consecutive: bool,
) {
    tokio::task::spawn_blocking(move || {
        record_failure(&pool, &provider_id, latency_ms, status, threshold, count_consecutive);
    });
}

/// list_available_models 的异步包装。
pub async fn list_available_models_async(pool: Arc<DbPool>) -> Vec<String> {
    tokio::task::spawn_blocking(move || list_available_models(&pool))
        .await
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Provider;

    fn provider(models: &str, mapping: &str) -> Provider {
        Provider {
            id: "p".into(),
            name: "p".into(),
            provider_type: "openai".into(),
            openai_base_url: "http://x".into(),
            anthropic_base_url: "http://x".into(),
            api_key: "k".into(),
            models: models.into(),
            priority: 0,
            weight: 1.0,
            is_active: true,
            health_status: "healthy".into(),
            latency_ms: 0.0,
            error_rate: 0.0,
            last_health_check: None,
            max_retries: 3,
            timeout_secs: 120,
            proxy_url: String::new(),
            model_mapping: mapping.into(),
            consecutive_failures: 0,
            disabled_reason: String::new(),
            protocols: "[\"openai\"]".into(),
            default_protocol: "openai".into(),
            note: String::new(),
            website_url: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn supports_model_exact_list_match() {
        let p = provider(r#"["gpt-4","claude-3"]"#, "");
        assert!(provider_supports_model(&p, "gpt-4"));
        assert!(!provider_supports_model(&p, "gpt-4o"));
    }

    #[test]
    fn supports_model_mapping_prefix() {
        let p = provider("[]", r#"{"claude-": "gpt-4"}"#);
        assert!(provider_supports_model(&p, "claude-3-opus"));
        assert!(!provider_supports_model(&p, "gpt-4"));
    }

    #[test]
    fn supports_model_empty_mapping_key_matches_nothing() {
        // An empty mapping key would prefix-match every model; it is skipped.
        let p = provider("[]", r#"{"": "gpt-4"}"#);
        assert!(!provider_supports_model(&p, "anything"));
    }

    #[test]
    fn supports_model_malformed_json() {
        let p = provider("not json", "also not json");
        assert!(!provider_supports_model(&p, "gpt-4"));
    }

    #[test]
    fn score_clamps_non_finite_and_extreme_values() {
        // inf/NaN 权重按 0.1 处理，评分必须保持有限
        let mut p = provider(r#"["gpt-4"]"#, "");
        p.weight = f64::INFINITY;
        let s = score_provider(&p, 100.0);
        assert!(s.is_finite());
        p.weight = f64::NAN;
        assert!(score_provider(&p, 100.0).is_finite());

        // NaN 错误率按最差处理（error_factor = 0）
        let mut p = provider(r#"["gpt-4"]"#, "");
        p.error_rate = f64::NAN;
        assert_eq!(score_provider(&p, 100.0), 0.0);

        // 极端延迟差时 latency_factor 比值 clamp 到 10
        let mut p = provider(r#"["gpt-4"]"#, "");
        p.latency_ms = 1.0;
        assert_eq!(score_provider(&p, 10_000.0), 10.0 * 10.0 * 1.0 * 1.0 * 1.0);
    }

    #[test]
    fn weighted_pick_survives_poisoned_fields() {
        // latency/weight 为 inf/NaN 时选路不得 panic
        let mut p = provider(r#"["gpt-4"]"#, "");
        p.latency_ms = f64::INFINITY;
        p.weight = f64::NAN;
        let picked = weighted_pick(&[&p]);
        assert!(picked.is_some());
    }

    // ---- record_failure 的健康口径 ----

    fn test_pool() -> crate::db::DbPool {
        let config = crate::config::AppConfig {
            database_url: ":memory:".into(),
            ..Default::default()
        };
        let pool = crate::db::DbPool::new(&config).unwrap();
        // schema 由 create_connection 建好,这里只造一个最小可用渠道行
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO providers (id, name, provider_type, openai_base_url, api_key, models)
                 VALUES ('p', 'p', 'openai', 'http://x', 'k', '[]')",
                [],
            )
            .unwrap();
        pool
    }

    fn provider_state(pool: &crate::db::DbPool) -> (f64, i32, bool, String) {
        pool.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT error_rate, consecutive_failures, is_active, disabled_reason
                 FROM providers WHERE id = 'p'",
                [],
                |r| {
                    Ok((
                        r.get::<_, f64>(0)?,
                        r.get::<_, i32>(1)?,
                        r.get::<_, i32>(2)? == 1,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .unwrap()
    }

    #[test]
    fn client_4xx_does_not_hurt_channel_health() {
        let pool = test_pool();
        // 先制造一次真实上游失败,让 error_rate 非零
        record_failure(&pool, "p", 100.0, 500, 5, true);
        let (er_before, cf_before, _, _) = provider_state(&pool);
        assert_eq!(cf_before, 1);
        // 客户端的错(400/404/422):不计 consecutive_failures,error_rate 只衰减
        for status in [400u16, 404, 422] {
            record_failure(&pool, "p", 100.0, status, 5, true);
        }
        let (er_after, cf_after, active, _) = provider_state(&pool);
        assert_eq!(cf_after, 1);
        assert!(er_after < er_before);
        assert!(active);
    }

    #[test]
    fn server_5xx_counts_and_threshold_disables() {
        let pool = test_pool();
        record_failure(&pool, "p", 100.0, 500, 2, true);
        record_failure(&pool, "p", 100.0, 502, 2, true);
        let (_, cf, active, reason) = provider_state(&pool);
        assert_eq!(cf, 2);
        assert!(!active);
        assert!(reason.contains("连续失败"));
    }

    #[test]
    fn repeated_attempt_in_same_request_not_double_counted() {
        let pool = test_pool();
        record_failure(&pool, "p", 100.0, 500, 5, true);
        // 同一请求内重试又打到同一渠道:指标照记,consecutive_failures 不加
        record_failure(&pool, "p", 100.0, 500, 5, false);
        let (er, cf, active, _) = provider_state(&pool);
        assert_eq!(cf, 1);
        assert!(er > 0.05); // error_rate 两次都推了
        assert!(active);
    }

    #[test]
    fn status_429_never_counts_nor_disables() {
        let pool = test_pool();
        for _ in 0..6 {
            record_failure(&pool, "p", 100.0, 429, 5, true);
        }
        let (er, cf, active, _) = provider_state(&pool);
        assert_eq!(cf, 0);
        assert!(er > 0.0); // 429 仍推 error_rate
        assert!(active);
    }

    #[test]
    fn status_401_disables_immediately() {
        let pool = test_pool();
        record_failure(&pool, "p", 100.0, 401, 5, true);
        let (_, _, active, reason) = provider_state(&pool);
        assert!(!active);
        assert!(reason.contains("认证失败"));
    }
}
