//! 候选迭代(阶段 2 拆分出的第一个可单测单元):依次选渠道 → 交给
//! [`single_attempt`] 单次尝试 → 按 [`crate::pipeline::classify`] 的硬边界
//! 决定"换下一条渠道 / 报错收尾"。
//!
//! 9.4 的结构保证:本循环唯一的续体(`continue`)发生在
//! [`AttemptOutcome::Failed`] 且 [`failover_allowed`] 判定为
//! `NextChannel` 时——该判定以 `ClientOutput::NotStarted` 为前提(流式
//! 移交路径在 200+SSE 头时直接 `return`,ctx 已 move 进生成器,不存在
//! "已向客户端写出后再 attempt"的分支)。

use std::collections::{HashMap, HashSet};

use tracing::debug;

use crate::AppState;
use crate::channel::selector::select_provider_async;
use crate::pipeline::attempt::{AttemptOutcome, single_attempt};
use crate::pipeline::classify::{ClientOutput, FailoverDecision, failover_allowed};
use crate::pipeline::context::RequestCtx;
use crate::pipeline::error::GatewayError;

/// 候选迭代在渠道之间携带的循环状态。
pub(crate) struct AttemptState {
    /// 已选过的渠道(选择器排除集;耗尽后由选择器重置)。
    pub(crate) tried: HashSet<String>,
    /// 每个渠道在本请求内的尝试次数:受渠道级 max_retries(重试次数,
    /// 0 = 不重试)约束——同一渠道最多尝试 max_retries + 1 次。
    pub(crate) per_provider_attempts: HashMap<String, u32>,
    /// 本请求已对哪些渠道累计过 consecutive_failure:同一渠道被重选回时
    /// 不重复累加,防止失败计数被重试放大。
    pub(crate) failure_counted: HashSet<String>,
    /// 最近一次失败(候选耗尽时回给客户端)。
    pub(crate) last_error: Option<GatewayError>,
}

/// 渠道重试/故障转移循环:依次尝试可用渠道,全部失败时返回最后一次上游
/// 错误(或 503)。流式请求在拿到 200 + SSE 头后把整个 ctx move 进
/// stream_response(见 [`AttemptOutcome::Stream`])。
pub(crate) async fn attempt_loop(state: AppState, ctx: RequestCtx) -> axum::response::Response {
    let max_attempts = state.config.max_retries.max(1);
    let mut st = AttemptState {
        tried: HashSet::new(),
        per_provider_attempts: HashMap::new(),
        failure_counted: HashSet::new(),
        last_error: None,
    };

    for _ in 0..max_attempts {
        let provider = match select_provider_async(
            state.store.clone(),
            ctx.model.clone(),
            st.tried.clone(),
            ctx.api_key_id.clone(),
        )
        .await
        {
            Some(p) => p,
            None => break,
        };
        st.tried.insert(provider.id.clone());
        if let Ok(mut t) = ctx.tried_order.lock() {
            t.push(provider.id.clone());
        }

        // 渠道级重试上限:该渠道已尝试 max_retries + 1 次后跳过,换下一个。
        let attempts = st
            .per_provider_attempts
            .entry(provider.id.clone())
            .or_insert(0);
        *attempts += 1;
        if *attempts > provider.max_retries.max(0) as u32 + 1 {
            debug!(
                request_id = %ctx.request_id,
                "Provider {} reached its per-channel retry limit ({}) — skipping",
                provider.name, provider.max_retries
            );
            continue;
        }

        // 密钥解密失败(AIKUN_ENCRYPTION_KEY/AIKUN_JWT_SECRET 被换)的渠道
        // 直接跳过,记一次普通失败:绝不能把空凭证发给上游——上游 401 会
        // 触发渠道立即禁用,换掉 JWT secret 就会连锁打挂全部渠道。
        if provider.key_decrypt_failed {
            tracing::error!(
                request_id = %ctx.request_id,
                "Provider '{}' 密钥解密失败,跳过该渠道——检查 AIKUN_ENCRYPTION_KEY/AIKUN_JWT_SECRET 是否变更",
                provider.name
            );
            crate::channel::selector::spawn_record_failure(
                state.store.clone(),
                provider.id.clone(),
                0.0,
                0,
                state.config.auto_disable_threshold,
                st.failure_counted.insert(provider.id.clone()),
            );
            continue;
        }

        // 字节级透传只能走声明 openai 协议的渠道(protocols 为空的旧行按
        // provider_type 兜底)。
        if ctx.is_passthrough() && !speaks_openai(&provider) {
            continue;
        }

        // ---- 单次尝试 + 失败分类(硬边界判定唯一入口) ----
        match single_attempt(&state, &ctx, &provider, &mut st).await {
            AttemptOutcome::Done(resp) => return resp,
            AttemptOutcome::Stream {
                resp,
                adapter,
                provider_protocol,
                provider_id,
                idle_timeout_secs,
            } => {
                // 流式移交:ctx 被 move 进生成器,本循环随即返回——此后
                // 上游失败只能由流转发单元报错收尾,不再选路(9.4)。
                return super::stream::stream_response(
                    state,
                    ctx,
                    resp,
                    adapter,
                    provider_protocol,
                    provider_id,
                    idle_timeout_secs,
                );
            }
            AttemptOutcome::Failed(err) => {
                st.last_error = Some(err.clone());
                match failover_allowed(err.failure_kind(), ClientOutput::NotStarted) {
                    FailoverDecision::NextChannel => continue,
                    FailoverDecision::FinishWithError => {
                        return err.into_response(ctx.client_protocol);
                    }
                }
            }
        }
    }

    // 全部尝试耗尽(或无渠道支持该模型):回最后一次上游错误或 503。
    match st.last_error {
        Some(err) => err.into_response(ctx.client_protocol),
        None => {
            let err = GatewayError::NoProvider(ctx.model.clone());
            crate::billing::ledger::spawn_request_log(
                &state,
                crate::billing::ledger::RequestLogParams::failure(
                    &ctx.request_id,
                    ctx.user_id.clone(),
                    ctx.api_key_id.clone(),
                    None,
                    &ctx.model,
                    &ctx.request_type(),
                    0,
                    503,
                    err.log_message(),
                ),
            );
            err.into_response(ctx.client_protocol)
        }
    }
}

/// 渠道是否声明讲 openai 协议(protocols 为空的旧行按 provider_type 兜底)。
pub(crate) fn speaks_openai(p: &crate::models::Provider) -> bool {
    serde_json::from_str::<Vec<String>>(&p.protocols)
        .map(|ps| ps.iter().any(|x| x == "openai"))
        .unwrap_or(false)
        || (p.protocols.trim().is_empty() && p.provider_type == "openai")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_with_protocols(protocols: &str, provider_type: &str) -> crate::models::Provider {
        crate::models::Provider {
            id: "p".into(),
            name: "p".into(),
            provider_type: provider_type.into(),
            openai_base_url: "http://x".into(),
            anthropic_base_url: "http://x".into(),
            api_key: "k".into(),
            key_decrypt_failed: false,
            models: "[]".into(),
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
            model_mapping: String::new(),
            consecutive_failures: 0,
            disabled_reason: String::new(),
            protocols: protocols.into(),
            default_protocol: String::new(),
            note: String::new(),
            website_url: String::new(),
            channel_kind: String::new(),
            balance_url: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// 透传候选过滤:声明 openai 的渠道、空 protocols 的旧 openai 渠道
    /// 可选;仅 anthropic 的渠道跳过。
    #[test]
    fn passthrough_candidate_filter_openai_only() {
        assert!(speaks_openai(&provider_with_protocols(
            r#"["openai","anthropic"]"#,
            "anthropic"
        )));
        assert!(speaks_openai(&provider_with_protocols("", "openai")));
        assert!(!speaks_openai(&provider_with_protocols(
            r#"["anthropic"]"#,
            "anthropic"
        )));
        assert!(!speaks_openai(&provider_with_protocols("", "anthropic")));
        // 非法 JSON 的 protocols 视为未声明
        assert!(!speaks_openai(&provider_with_protocols(
            "oops",
            "anthropic"
        )));
    }
}
