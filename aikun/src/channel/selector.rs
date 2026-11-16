use std::collections::HashSet;
use std::sync::Arc;

use rand::Rng;
use rusqlite::{Connection, params};

use crate::channel::registry::{self, ChannelEntry};
use crate::crypto::KeyCipher;
use crate::db::DbPool;
use crate::models::Provider;

// 模型支持判定(models 精确 + model_mapping 非空 key 前缀)已随快照
// 预解析移入 registry::ChannelEntry::supports_model(建快照时一次,
// 选路零重复 JSON parse);口径对拍见 registry.rs 测试。

/// 非有限值防御：持久化数据或计算结果中的 inf/NaN 一律回退安全值，
/// 避免 random_range(0.0..inf) panic。
fn finite_or(v: f64, fallback: f64) -> f64 {
    if v.is_finite() { v } else { fallback }
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

/// 钉住的渠道还能不能接着用:仍在候选里(快照只含启用渠道)、密钥解得开、
/// 没有被本请求排除、也不是 unhealthy。degraded 仍留下——提示缓存比略差
/// 的健康分更值钱。密钥可解性只对钉住渠道做一次解密判定(旧实现每请求
/// 逐行解密全部渠道;快照持有密文,选中后解一次)。
fn pinned_usable(p: &Provider, exclude: &HashSet<String>, cipher: &KeyCipher) -> bool {
    p.health_status != "unhealthy"
        && !exclude.contains(&p.id)
        && !(p.api_key.starts_with(crate::crypto::ENC_PREFIX)
            && cipher.decrypt(&p.api_key).is_none())
}

fn pinned_provider_id(conn: &Connection, api_key_id: &str, model: &str) -> Option<String> {
    conn.query_row(
        "SELECT provider_id FROM channel_affinity WHERE api_key_id = ?1 AND model = ?2",
        params![api_key_id, model],
        |row| row.get(0),
    )
    .ok()
}

/// 把 (API Key, 模型) 钉到这次成功的渠道。下一次同密钥同模型优先走它。
pub fn pin_channel(pool: &DbPool, api_key_id: &str, model: &str, provider_id: &str) {
    if api_key_id.is_empty() || model.is_empty() || provider_id.is_empty() {
        return;
    }
    let Ok(conn) = pool.conn.lock() else {
        return;
    };
    let _ = conn.execute(
        "INSERT INTO channel_affinity (api_key_id, model, provider_id, updated_at)
         VALUES (?1, ?2, ?3, datetime('now'))
         ON CONFLICT(api_key_id, model) DO UPDATE SET
            provider_id = excluded.provider_id,
            updated_at = datetime('now')",
        params![api_key_id, model, provider_id],
    );
}

/// Select the best provider for a given model, excluding already-tried ones.
///
/// 吃渠道拓扑快照(`channel/registry`),选路路径零全表扫描、零逐行解密、
/// 零重复 JSON 解析:快照持有启用渠道(密文密钥 + 预解析模型清单),
/// 命中渠道后解一次密。每请求仅剩两条点查 SQL:快照失效检测的表令牌
/// (注册表内部)与渠道亲和点查(事实走 DB,roadmap 决策 4)。
///
/// 有 API Key 时先看渠道亲和:同一把密钥、同一个模型尽量一直走上次成功
/// 的渠道(上游提示缓存按渠道分开,中途换渠道等于缓存作废)。钉住的渠道
/// 不健康、解不开密钥、不再提供该模型,或已经在本请求的排除集里,才退回
/// 加权随机。没有 API Key(管理端试请求)不钉。
pub fn select_provider(
    pool: &DbPool,
    model: &str,
    exclude: &HashSet<String>,
    api_key_id: Option<&str>,
) -> Option<Provider> {
    let snap = registry::shared().current(pool);
    let entries = snap.entries();
    let cipher = &pool.cipher;

    if let Some(key_id) = api_key_id.map(str::trim).filter(|s| !s.is_empty())
        && let Some(pinned_id) = pool
            .read()
            .lock()
            .ok()
            .and_then(|conn| pinned_provider_id(&conn, key_id, model))
        // 钉住渠道必须仍支持该模型(旧语义:亲和查找发生在模型过滤后的
        // 候选集内),且健康可钉、未被本请求排除、密钥解得开。
        && let Some(e) = entries.iter().find(|e| {
            e.provider.id == pinned_id
                && e.supports_model(model)
                && pinned_usable(&e.provider, exclude, cipher)
        })
    {
        return Some(decrypt_provider(&e.provider, cipher));
    }

    let supports: Vec<&ChannelEntry> = entries.iter().filter(|e| e.supports_model(model)).collect();
    let mut candidates: Vec<&ChannelEntry> = supports
        .iter()
        .copied()
        .filter(|e| !exclude.contains(&e.provider.id))
        .collect();
    if candidates.is_empty() {
        // All matching providers were tried — allow retrying any of them.
        candidates = supports;
    }
    if candidates.is_empty() {
        return None;
    }

    // Filter to only healthy providers(快照只含 is_active=1 的渠道)。
    let healthy: Vec<&Provider> = candidates
        .iter()
        .copied()
        .map(|e| &e.provider)
        .filter(|p| p.health_status != "unhealthy")
        .collect();

    if healthy.is_empty() {
        // Fall back to any active provider even if unhealthy — still using
        // weighted scoring instead of always picking the first one.
        let active: Vec<&Provider> = candidates.iter().copied().map(|e| &e.provider).collect();
        return weighted_pick(&active).map(|p| decrypt_provider(&p, cipher));
    }

    // 选中后解一次密;解不开打 key_decrypt_failed 标记,由 attempt_loop
    // 跳过该渠道——空凭证打到上游会吃 401,触发立即禁用并连锁打挂全部渠道。
    weighted_pick(&healthy).map(|p| decrypt_provider(&p, cipher))
}

/// 选中渠道后解密密钥:明文原样兼容;enc:v1: 密文解密失败置
/// `key_decrypt_failed`(与旧 find_providers_for_model 的逐行解密口径
/// 一致,只是从"每渠道每请求"收敛到"选中渠道一次")。
fn decrypt_provider(p: &Provider, cipher: &KeyCipher) -> Provider {
    let mut out = p.clone();
    if out.api_key.starts_with(crate::crypto::ENC_PREFIX) && cipher.decrypt(&out.api_key).is_none()
    {
        out.key_decrypt_failed = true;
    }
    out.api_key = crate::crypto::decrypt_or_plain(cipher, &out.api_key);
    out
}

/// List all unique models available across all active providers.
/// 除 models 列表外,model_mapping 的 key(请求侧模型名)也并入——仅靠
/// 映射路由的模型调得通却看不到,客户端无法发现。走拓扑快照,与
/// health 循环的健康态回写同源。
pub fn list_available_models(pool: &DbPool) -> Vec<String> {
    registry::shared().current(pool).available_models()
}

/// Record a successful request: update latency/error-rate EMAs and reset the
/// consecutive failure counter.
/// DB 是事实(roadmap 决策 4);拓扑快照用同一公式镜像 EMA,保持选路
/// 评分的实时性(镜像与 SQL 逐值对拍见测试)。
pub fn record_request_result(pool: &DbPool, provider_id: &str, latency_ms: f64, success: bool) {
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
    registry::shared()
        .cached()
        .mirror_request_result(provider_id, latency_ms, success);
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
    let counts_as_failure = (count_consecutive && is_upstream_failure && status != 429) as i32;
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
    // EMA 事实镜像到拓扑快照(同公式;见 record_request_result 注释)。
    registry::shared()
        .cached()
        .mirror_failure(provider_id, latency_ms, is_upstream_failure);

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
        // 快照同步移除(该 DB 写会更新 updated_at,下次令牌比对也会触发
        // 重建;镜像让"下一请求"立即看不到该渠道,不等点查)。
        registry::shared().cached().deactivate(provider_id);
    }
}
// ---- 异步包装:热路径(每请求的鉴权/选路/记账)不得在 async executor 上
// 同步持锁访问 SQLite,一律派发到 blocking 线程池(M35)。----

/// select_provider 的异步包装。`api_key_id` 为空时不做渠道亲和。
pub async fn select_provider_async(
    pool: Arc<DbPool>,
    model: String,
    exclude: HashSet<String>,
    api_key_id: Option<String>,
) -> Option<Provider> {
    tokio::task::spawn_blocking(move || {
        select_provider(&pool, &model, &exclude, api_key_id.as_deref())
    })
    .await
    .ok()
    .flatten()
}

/// 成功后改钉渠道。没有 API Key 时什么都不做。
pub fn spawn_pin_channel(
    pool: Arc<DbPool>,
    api_key_id: Option<String>,
    model: String,
    provider_id: String,
) {
    let Some(key_id) = api_key_id.filter(|s| !s.is_empty()) else {
        return;
    };
    tokio::task::spawn_blocking(move || {
        pin_channel(&pool, &key_id, &model, &provider_id);
    });
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
        record_failure(
            &pool,
            &provider_id,
            latency_ms,
            status,
            threshold,
            count_consecutive,
        );
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
            key_decrypt_failed: false,
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
            channel_kind: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
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

    #[test]
    fn available_models_include_mapping_keys() {
        let pool = test_pool();
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE providers SET models = '[\"gpt-4\"]', model_mapping = '{\"claude-\":\"gpt-4\",\"\":\"x\"}' WHERE id = 'p'",
                [],
            )
            .unwrap();
        let models = list_available_models(&pool);
        assert!(models.contains(&"gpt-4".to_string()));
        // mapping 的 key(请求侧模型名)也要出现在列表里;空 key 除外
        assert!(models.contains(&"claude-".to_string()));
        assert!(!models.contains(&"".to_string()));
    }

    fn insert_provider(pool: &crate::db::DbPool, id: &str, models: &str, weight: f64) {
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO providers (id, name, provider_type, openai_base_url, api_key, models, weight)
                 VALUES (?1, ?1, 'openai', 'http://x', 'k', ?2, ?3)",
                params![id, models, weight],
            )
            .unwrap();
    }

    #[test]
    fn affinity_prefers_pinned_channel_over_higher_weight() {
        let pool = test_pool();
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE providers SET models = '[\"gpt-4\"]', weight = 1 WHERE id = 'p'",
                [],
            )
            .unwrap();
        insert_provider(&pool, "heavy", "[\"gpt-4\"]", 100.0);
        pin_channel(&pool, "key-1", "gpt-4", "p");

        let picked = select_provider(&pool, "gpt-4", &HashSet::new(), Some("key-1")).unwrap();
        assert_eq!(picked.id, "p");

        // 本请求已经失败过的渠道不再钉住,改走别的候选。
        let mut excluded = HashSet::new();
        excluded.insert("p".to_string());
        let picked = select_provider(&pool, "gpt-4", &excluded, Some("key-1")).unwrap();
        assert_eq!(picked.id, "heavy");

        // 略差(degraded)仍留下:缓存比健康分重要。
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE providers SET health_status = 'degraded' WHERE id = 'p'",
                [],
            )
            .unwrap();
        let picked = select_provider(&pool, "gpt-4", &HashSet::new(), Some("key-1")).unwrap();
        assert_eq!(picked.id, "p");

        // unhealthy 则放弃这颗钉,改去健康渠道。
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE providers SET health_status = 'unhealthy' WHERE id = 'p'",
                [],
            )
            .unwrap();
        let picked = select_provider(&pool, "gpt-4", &HashSet::new(), Some("key-1")).unwrap();
        assert_eq!(picked.id, "heavy");
    }

    #[test]
    fn affinity_repin_and_ignores_missing_key() {
        let pool = test_pool();
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE providers SET models = '[\"gpt-4\"]' WHERE id = 'p'",
                [],
            )
            .unwrap();
        insert_provider(&pool, "heavy", "[\"gpt-4\"]", 1.0);
        pin_channel(&pool, "key-1", "gpt-4", "p");
        pin_channel(&pool, "key-1", "gpt-4", "heavy");
        let picked = select_provider(&pool, "gpt-4", &HashSet::new(), Some("key-1")).unwrap();
        assert_eq!(picked.id, "heavy");

        // 另一把密钥没有钉,不继承 key-1 的渠道。这里只断言函数不 panic,
        // 具体落到谁由加权随机决定。
        assert!(select_provider(&pool, "gpt-4", &HashSet::new(), None).is_some());
        assert!(select_provider(&pool, "gpt-4", &HashSet::new(), Some("")).is_some());
    }

    #[test]
    fn affinity_skips_pin_that_no_longer_serves_the_model() {
        let pool = test_pool();
        pool.conn
            .lock()
            .unwrap()
            .execute("UPDATE providers SET models = '[]' WHERE id = 'p'", [])
            .unwrap();
        insert_provider(&pool, "heavy", "[\"gpt-4\"]", 1.0);
        pin_channel(&pool, "key-1", "gpt-4", "p");
        let picked = select_provider(&pool, "gpt-4", &HashSet::new(), Some("key-1")).unwrap();
        assert_eq!(picked.id, "heavy");
    }

    #[test]
    fn pinned_usable_rejects_bad_pin() {
        let cipher = crate::crypto::KeyCipher::from_secret("test-cipher");
        let mut p = provider(r#"["gpt-4"]"#, "");
        let exclude = HashSet::new();
        // 明文密钥:可解,健康即可用
        assert!(pinned_usable(&p, &exclude, &cipher));
        p.health_status = "degraded".into();
        assert!(pinned_usable(&p, &exclude, &cipher));
        p.health_status = "unhealthy".into();
        assert!(!pinned_usable(&p, &exclude, &cipher));
        // 密文解不开(enc 前缀但非本密钥加密)⇒ 钉不可用
        p.health_status = "healthy".into();
        p.api_key = "enc:v1:bm90LXZhbGlk".into();
        assert!(!pinned_usable(&p, &exclude, &cipher));
        p.api_key = "k".into();
        let mut exclude = HashSet::new();
        exclude.insert("p".into());
        assert!(!pinned_usable(&p, &exclude, &cipher));
    }

    // ---- 快照缓存与 DB 一致(roadmap §四.5 防御测试) ----
    //
    // registry 是进程单测共享单例;以下用例用独立 pool + 独立渠道 id,
    // 交叉重建只会让数据更新鲜,不影响断言。

    fn snap_pool() -> crate::db::DbPool {
        crate::db::DbPool::new(&crate::config::AppConfig {
            database_url: ":memory:".into(),
            ..Default::default()
        })
        .unwrap()
    }

    /// 直写 DB(绕过管理端 API,如测试夹具/运维脚本):下一请求必须看到
    /// 新渠道(表令牌捕获行数/rowid 变化)。
    #[test]
    fn selection_sees_direct_db_insert_next_request() {
        let pool = snap_pool();
        // 先选一次:快照为空,选不出渠道(等价旧行为 DB 空表 → None)
        assert!(select_provider(&pool, "gpt-4", &HashSet::new(), None).is_none());

        pool.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO providers (id, name, provider_type, openai_base_url, api_key, models)
                 VALUES ('snap-direct', 'd', 'openai', 'http://x', 'k', '[\"gpt-4\"]')",
                [],
            )
            .unwrap();
        let picked = select_provider(&pool, "gpt-4", &HashSet::new(), None)
            .expect("next selection must see the direct insert");
        assert_eq!(picked.id, "snap-direct");
        // 密文保持库内形态直到选中;明文 key 原样可用
        assert_eq!(picked.api_key, "k");
        assert!(!picked.key_decrypt_failed);
    }

    /// 失效检测两级防线:表令牌捕获「不改 updated_at 的 unhealthy 跃迁」
    /// (直写夹具可见);世代计数器捕获令牌盲区(如 weight 原地改值)——
    /// 管理端写路径全部走世代(roadmap「缓存与 DB 一致」防御)。
    #[test]
    fn token_and_epoch_both_invalidate_snapshot_next_request() {
        let pool = snap_pool();
        {
            let conn = pool.conn.lock().unwrap();
            for id in ["snap-ep-a", "snap-ep-b"] {
                conn.execute(
                    "INSERT INTO providers (id, name, provider_type, openai_base_url, api_key, models, weight)
                     VALUES (?1, ?1, 'openai', 'http://x', 'k', '[\"gpt-4\"]', 1)",
                    params![id],
                )
                .unwrap();
            }
            // 钉住 a:选路必中
            conn.execute(
                "INSERT INTO channel_affinity (api_key_id, model, provider_id)
                 VALUES ('snap-key', 'gpt-4', 'snap-ep-a')",
                [],
            )
            .unwrap();
        }
        let picked = select_provider(&pool, "gpt-4", &HashSet::new(), Some("snap-key")).unwrap();
        assert_eq!(picked.id, "snap-ep-a");

        // 令牌盲区演示:weight 原地改值(不动 updated_at/行数/健康态)
        // 不触发重建——钉住关系不受影响,断言保持稳定
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE providers SET weight = 1000 WHERE id = 'snap-ep-b'",
                [],
            )
            .unwrap();
        let picked = select_provider(&pool, "gpt-4", &HashSet::new(), Some("snap-key")).unwrap();
        assert_eq!(picked.id, "snap-ep-a");

        // 管理端写路径成功 ⇒ 世代递增 ⇒ 令牌盲区也在下一请求可见
        aikun_admin::admin_providers::bump_topology_epoch();
        // (钉住优先于权重,故改看非钉路径:去掉 key,加权应选 b)
        let picked = select_provider(&pool, "gpt-4", &HashSet::new(), None).unwrap();
        assert_eq!(picked.id, "snap-ep-b", "epoch 失效后 weight=1000 必须可见");

        // 直写 unhealthy 跃迁(不动 updated_at)⇒ 表令牌捕获 ⇒ 立即可见
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE providers SET health_status = 'unhealthy' WHERE id = 'snap-ep-a'",
                [],
            )
            .unwrap();
        let picked = select_provider(&pool, "gpt-4", &HashSet::new(), Some("snap-key")).unwrap();
        assert_ne!(
            picked.id, "snap-ep-a",
            "unhealthy 跃迁经表令牌失效,必须放弃这颗钉"
        );
    }

    /// EMA 事实:DB 写(SQL 原文)与快照镜像逐值一致;自动禁用后下一
    /// 请求不再选中该渠道。
    #[test]
    fn record_result_mirrors_snapshot_and_matches_db() {
        let pool = snap_pool();
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO providers (id, name, provider_type, openai_base_url, api_key, models)
                 VALUES ('snap-ema', 'e', 'openai', 'http://x', 'k', '[\"gpt-4\"]')",
                [],
            )
            .unwrap();
        assert!(select_provider(&pool, "gpt-4", &HashSet::new(), None).is_some());

        record_request_result(&pool, "snap-ema", 100.0, true);
        record_request_result(&pool, "snap-ema", 200.0, false);
        record_failure(&pool, "snap-ema", 50.0, 502, 5, true);

        // 快照里的 EMA 与 DB 行一致(镜像路径或重建路径都必须等于 DB)
        let snap = registry::shared().current(&pool);
        let (snap_latency, snap_error) = {
            let entries = snap.entries();
            let entry = entries
                .iter()
                .find(|e| e.provider.id == "snap-ema")
                .expect("snapshot must contain the provider");
            (entry.provider.latency_ms, entry.provider.error_rate)
        };
        let conn = pool.read().lock().unwrap();
        let (db_latency, db_error): (f64, f64) = conn
            .query_row(
                "SELECT latency_ms, error_rate FROM providers WHERE id = 'snap-ema'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        drop(conn);
        assert_eq!(snap_latency, db_latency);
        assert_eq!(snap_error, db_error);

        // 连续失败到阈值 → 自动禁用(DB 写 + 快照移除)→ 下一请求换渠道
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO providers (id, name, provider_type, openai_base_url, api_key, models, weight)
                 VALUES ('snap-ema-alt', 'alt', 'openai', 'http://x', 'k', '[\"gpt-4\"]', 1)",
                [],
            )
            .unwrap();
        record_failure(&pool, "snap-ema", 50.0, 502, 2, true);
        let picked = select_provider(&pool, "gpt-4", &HashSet::new(), None).unwrap();
        assert_eq!(picked.id, "snap-ema-alt");
    }
}
