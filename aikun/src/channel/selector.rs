use std::collections::HashSet;
use std::sync::Arc;

use rand::Rng;

use aikun_store::surreal::channels_store::{ChannelStore, ProviderMetrics};

use crate::channel::registry::{self, ChannelEntry};
use crate::crypto::KeyCipher;
use crate::models::Provider;
use crate::store::AppStore;

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

/// Select the best provider for a given model, excluding already-tried ones.
///
/// 吃渠道拓扑快照(`channel/registry`),选路路径零全表扫描、零逐行解密、
/// 零重复 JSON 解析:快照持有启用渠道(密文密钥 + 预解析模型清单),
/// 命中渠道后解一次密。每请求仅剩两次点查:快照失效检测的表令牌
/// (注册表内部)与渠道亲和点查(事实走 DB,roadmap 决策 4)——阶段 S
/// 起两者都是 async Surreal 查询(docs/surreal-migration.md §六)。
///
/// 有 API Key 时先看渠道亲和:同一把密钥、同一个模型尽量一直走上次成功
/// 的渠道(上游提示缓存按渠道分开,中途换渠道等于缓存作废)。钉住的渠道
/// 不健康、解不开密钥、不再提供该模型,或已经在本请求的排除集里,才退回
/// 加权随机。没有 API Key(管理端试请求)不钉。
pub async fn select_provider(
    store: &AppStore,
    model: &str,
    exclude: &HashSet<String>,
    api_key_id: Option<&str>,
) -> Option<Provider> {
    // 亲和点查先行(悬空亲和——渠道已删——读侧自然降级 None):快照的
    // 读锁守卫非 Send,不得跨 await 持有,故先取钉住 id 再进守卫。
    let pinned_id = match api_key_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(key_id) => store
            .billing
            .get_affinity(key_id, model)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let snap = registry::shared().current(store).await;
    let entries = snap.entries();
    let cipher = &store.cipher;

    // 钉住渠道必须仍支持该模型(旧语义:亲和查找发生在模型过滤后的
    // 候选集内),且健康可钉、未被本请求排除、密钥解得开。
    if let Some(pinned_id) = pinned_id
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
pub async fn list_available_models(store: &AppStore) -> Vec<String> {
    registry::shared().current(store).await.available_models()
}

// ---- EMA 事实写(读-算-写;公式与旧 SQL 逐句对齐) ----
//
// SurrealQL 无 CASE 表达式,EMA 的条件分支读侧取现值后在 Rust 计算
// (provider_metrics → record_channel_result 两往返)。并发丢一次更新
// 只影响平滑指标的瞬时精度;consecutive_failures 的计数与阈值判定
// 同读同写,自动禁用语义不变。快照镜像公式照旧(实时性)。

/// 成功请求的 EMA 公式(旧 `record_request_result` 的 UPDATE 同构):
/// `latency = latency==0 ? l : latency*0.9 + l*0.1`;
/// `error_rate = success ? *0.95 : *0.95+0.05`;成功清零连败计数。
async fn ema_step(
    channels: &dyn ChannelStore,
    provider_id: &str,
    latency_ms: f64,
    success: bool,
    counts_consecutive: bool,
) {
    let Some(m) = channels.provider_metrics(provider_id).await.ok().flatten() else {
        return;
    };
    let new_latency = if m.latency_ms == 0.0 {
        latency_ms
    } else {
        m.latency_ms * 0.9 + latency_ms * 0.1
    };
    // 成功方向衰减;上游失败推高(由调用方换算成 success=false 语义)。
    let new_error = if success {
        m.error_rate * 0.95
    } else {
        m.error_rate * 0.95 + 0.05
    };
    let new_failures = if success {
        0
    } else if counts_consecutive {
        m.consecutive_failures + 1
    } else {
        m.consecutive_failures
    };
    let _ = channels
        .record_channel_result(
            provider_id,
            new_latency,
            new_error,
            if success { "healthy" } else { "degraded" },
            new_failures,
            None,
        )
        .await;
}

/// Record a successful request: update latency/error-rate EMAs and reset the
/// consecutive failure counter. DB 是事实(roadmap 决策 4);拓扑快照用
/// 同一公式镜像 EMA,保持选路评分的实时性。
pub async fn record_request_result(
    store: &AppStore,
    provider_id: &str,
    latency_ms: f64,
    success: bool,
) {
    ema_step(
        store.channels.as_ref(),
        provider_id,
        latency_ms,
        success,
        true,
    )
    .await;
    registry::shared()
        .cached()
        .mirror_request_result(provider_id, latency_ms, success);
}

// ---- 失败分类守卫(单测锁定,语义自旧 SQLite selector 迁移)----
/// 0(传输)/408/429/5xx 属上游侧;其余 4xx 是客户端请求的错,不伤渠道健康。
pub(crate) fn is_upstream_failure(status: u16) -> bool {
    status == 0 || status == 408 || status == 429 || status >= 500
}

/// consecutive_failures 累加守卫:**429 永不计失败**(渠道本身可能健康);
/// 同请求重试的后续尝试(count_consecutive=false)不重复累加,防误禁用。
pub(crate) fn counts_toward_disable(status: u16, count_consecutive: bool) -> bool {
    count_consecutive && is_upstream_failure(status) && status != 429
}

/// Record a failed upstream attempt. Auto-disables the provider when:
/// - the upstream returned 401/403 (bad credentials — always dead), or
/// - consecutive failures reach `threshold`.
///
/// 健康口径(与旧 SQL 逐句对齐):
/// - 传输错误(0)、408、429、5xx 视为上游失败,推高 error_rate;
/// - 其中除 429(上游限流,渠道本身可能健康)外累计 consecutive_failures;
/// - 其余 4xx(400/404/422)是客户端请求的错,与渠道健康无关——只更新
///   延迟、按成功方向衰减 error_rate,不计 consecutive_failures,避免
///   恶意/乱配客户端的坏请求把健康渠道打到自动禁用。
///
/// `count_consecutive`:同一请求内因重试反复打到同一渠道时,后续尝试传
/// false——latency/error_rate 照记,consecutive_failures 不重复累加,
/// 防止失败计数被重试放大导致误禁用。
pub async fn record_failure(
    store: &AppStore,
    provider_id: &str,
    latency_ms: f64,
    status: u16,
    threshold: u32,
    count_consecutive: bool,
) {
    let upstream_failure = is_upstream_failure(status);
    let counts_as_failure = counts_toward_disable(status, count_consecutive);
    // 客户端 4xx 按成功方向衰减 error_rate(不推高)。
    ema_step(
        store.channels.as_ref(),
        provider_id,
        latency_ms,
        !upstream_failure,
        counts_as_failure,
    )
    .await;
    // EMA 事实镜像到拓扑快照(同公式)。
    registry::shared()
        .cached()
        .mirror_failure(provider_id, latency_ms, upstream_failure);

    let reason = if status == 401 || status == 403 {
        Some(format!("上游返回 {} 认证失败,自动禁用", status))
    } else if counts_as_failure {
        match store
            .channels
            .provider_metrics(provider_id)
            .await
            .ok()
            .flatten()
        {
            Some(ProviderMetrics {
                consecutive_failures,
                ..
            }) if consecutive_failures >= threshold as i32 => {
                Some(format!("连续失败 {} 次,自动禁用", consecutive_failures))
            }
            _ => None,
        }
    } else {
        None
    };

    if let Some(reason) = reason {
        tracing::warn!("Auto-disabling provider {provider_id}: {reason}");
        // 自动禁用:bump updated_at(令牌/选路面立即可见)。
        let _ = store
            .channels
            .record_channel_result(provider_id, 0.0, 0.0, "unhealthy", 0, Some(&reason))
            .await;
        // 快照同步移除(镜像让"下一请求"立即看不到该渠道,不等点查)。
        registry::shared().cached().deactivate(provider_id);
    }
}

/// 把 (API Key, 模型) 钉到这次成功的渠道。下一次同密钥同模型优先走它。
pub async fn pin_channel(store: &AppStore, api_key_id: &str, model: &str, provider_id: &str) {
    if api_key_id.is_empty() || model.is_empty() || provider_id.is_empty() {
        return;
    }
    if let Err(e) = store
        .billing
        .set_affinity(api_key_id, model, provider_id)
        .await
    {
        tracing::warn!("pin_channel failed: {e}");
    }
}

// ---- 异步包装:fire-and-forget 写投递为独立任务(不阻塞响应路径) ----

/// 成功后改钉渠道。没有 API Key 时什么都不做。
pub fn spawn_pin_channel(
    store: Arc<AppStore>,
    api_key_id: Option<String>,
    model: String,
    provider_id: String,
) {
    let Some(key_id) = api_key_id.filter(|s| !s.is_empty()) else {
        return;
    };
    tokio::spawn(async move {
        pin_channel(&store, &key_id, &model, &provider_id).await;
    });
}

/// record_request_result 的 fire-and-forget 包装。
pub fn spawn_record_result(
    store: Arc<AppStore>,
    provider_id: String,
    latency_ms: f64,
    success: bool,
) {
    tokio::spawn(async move {
        record_request_result(&store, &provider_id, latency_ms, success).await;
    });
}

/// record_failure 的 fire-and-forget 包装。
pub fn spawn_record_failure(
    store: Arc<AppStore>,
    provider_id: String,
    latency_ms: f64,
    status: u16,
    threshold: u32,
    count_consecutive: bool,
) {
    tokio::spawn(async move {
        record_failure(
            &store,
            &provider_id,
            latency_ms,
            status,
            threshold,
            count_consecutive,
        )
        .await;
    });
}

/// list_available_models 的等待型包装(/v1/models)。
pub async fn list_available_models_async(store: Arc<AppStore>) -> Vec<String> {
    list_available_models(&store).await
}

/// select_provider 的等待型包装(存储句柄克隆,热路径调用形态)。
pub async fn select_provider_async(
    store: Arc<AppStore>,
    model: String,
    exclude: HashSet<String>,
    api_key_id: Option<String>,
) -> Option<Provider> {
    select_provider(&store, &model, &exclude, api_key_id.as_deref()).await
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
    fn failure_classification_guards() {
        // 客户端 4xx 不伤渠道健康(不计失败,EMA 按成功方向衰减)
        for s in [400u16, 402, 403, 404, 422] {
            assert!(!is_upstream_failure(s), "{s} 应属客户端错误");
            assert!(
                !counts_toward_disable(s, true),
                "{s} 不得计 consecutive_failures"
            );
        }
        // 上游侧失败:0(传输)/408/5xx
        for s in [0u16, 408, 500, 502, 503] {
            assert!(is_upstream_failure(s), "{s} 应属上游失败");
            assert!(counts_toward_disable(s, true), "{s} 首次尝试应计失败");
        }
        // 429:渠道可能健康,永不计 consecutive_failures
        assert!(is_upstream_failure(429));
        assert!(!counts_toward_disable(429, true), "429 永不计失败");
        // 同请求重试的后续尝试:不重复累加(防重试放大导致误禁用)
        for s in [0u16, 408, 500] {
            assert!(!counts_toward_disable(s, false), "{s} 重试重复尝试不得累加");
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

    // 阶段 S 切换:DB 行为断言(EMA 写库、亲和钉住、快照失效)移至
    // aikun-store 的 surreal 单测(mem:// 进程内直连,docs/surreal-migration.md
    // §七第 2 层);本文件保留纯函数测试,端到端行为由下一批恢复 e2e。
}
