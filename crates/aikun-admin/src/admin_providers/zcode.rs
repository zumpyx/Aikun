//! ZCode 应用业务:OAuth 在线登录 + 免费额度领取(签到)。
//!
//! 从 admin_providers 主模块拆出(2026-11-18 渠道业务模块化):端口
//! trait(HTTP 经装配层注入,crate 纯净度)与 [`AdminProvidersService`]
//! 的 zcode 专属方法都在这里;渠道 CRUD/导入导出等通用面见 `mod.rs`。

use aikun_openapi::openapi::server::errors::{
    AdminListZcodeTasksResponse, AdminZcodeClaimQuotaResponse, AdminZcodeLoginInitResponse,
    AdminZcodeLoginPollResponse,
};
use aikun_openapi::openapi::types::{
    AdminZcodeClaimResult, ProxyTaskList, ProxyTaskRecord, ProxyTaskRecordSource,
    ZcodeLoginInitRequestEnv, ZcodeLoginInitResult, ZcodeLoginPollRequest,
    ZcodeLoginPollRequestEnv, ZcodeLoginPollResult, ZcodeLoginPollResultStatus,
};
use async_trait::async_trait;

use aikun_store::surreal::channels_store::ProxyTaskRow;

use super::{
    AdminProvidersService, ProviderCandidate, insert_checked, to_api_provider, validate_candidate,
};
use crate::problem;

/// ZCode OAuth 在线登录端口(HTTP 经装配层注入;aikun-admin 无 reqwest
/// 的纯净度约束)。流程移植自 Zcode2Api internal/gateway/login.go:
/// init(随机 poll_token 作 Bearer)→ 浏览器 authorize → poll 轮询。
pub struct ZcodeOAuthInit {
    pub flow_id: String,
    pub authorize_url: String,
}

pub enum ZcodePollStatus {
    Pending,
    Ready { token: String, email: String },
    Expired,
    Failed(String),
}

#[async_trait]
pub trait ZcodeOAuth: Send + Sync + 'static {
    /// 发起授权流:生成 poll_token(随机)→ POST {base}/oauth/cli/init
    /// (Bearer poll_token,body {"provider":"zai"})。成功返回
    /// (init 结果, poll_token)——poll_token 原样返还前端供 poll 回传,
    /// 服务端无状态。Err 为网络/上游失败描述。
    /// env = "intl"(z.ai 账号,provider=zai)| "cn"(智谱 BigModel 账号,
    /// provider=bigmodel;2026-11-19 实测同一 OAuth 端点,仅 provider 值不同,
    /// 授权页分别为 chat.z.ai / bigmodel.cn)。
    async fn init(&self, env: &str) -> Result<(ZcodeOAuthInit, String), String>;
    /// 轮询:GET {base}/oauth/cli/poll/{flow_id}。网络错误按 Pending
    /// (调用方持续轮询;上游口径)。
    async fn poll(&self, flow_id: &str, poll_token: &str) -> ZcodePollStatus;
}

/// ZCode 免费额度领取的可领清单项(billing/preview ∪ marketing/touch)。
pub struct ZcodeOfferedPlan {
    pub plan_id: String,
    /// 展示名(无则用 plan_id)。
    pub name: String,
    /// marketing/touch 投放带来的 plan 才有;领取成功后需 confirm。
    pub campaign_id: String,
}

/// ZCode 免费额度领取端口(HTTP 经装配层注入,crate 纯净度)。序列炼化
/// 自 Zcode2Api warm.go:激活事件 → preview/touch 拉清单 → 逐项 claim →
/// campaign confirm。风控验证码(3007)不在网关侧破解,按 Err 报告。
#[async_trait]
pub trait ZcodeClaimPort: Send + Sync + 'static {
    /// 激活事件(app_launch / app_daily_active 各一条;上游按
    /// device_mid+日期去重)。领取前置,失败不阻断(记 problem 继续)。
    async fn report_activation(&self, base: &str, jwt: &str) -> Result<(), String>;
    /// 待领清单:billing/preview(priority 降序)+ marketing/touch 横幅
    /// 按钮(plan_id + campaign_id),已按领取优先级排好。
    async fn offered_plans(&self, base: &str, jwt: &str) -> Result<Vec<ZcodeOfferedPlan>, String>;
    /// billing/balance 已持有的 plan_id(小写;claim 前查重跳过)。
    async fn owned_plans(&self, base: &str, jwt: &str) -> Result<Vec<String>, String>;
    /// POST billing/claim:Ok(true)=领取成功;Ok(false)=已领取
    /// (上游 code 1003,视为成功);Err=问题(含 3007 风控)。
    async fn claim_plan(&self, base: &str, jwt: &str, plan_id: &str) -> Result<bool, String>;
    /// POST marketing/touch/action(confirm;campaigned plan 领取后补)。
    async fn confirm_campaign(
        &self,
        base: &str,
        jwt: &str,
        campaign_id: &str,
    ) -> Result<(), String>;
}

// ---- 服务方法(挂在主模块的服务上;子模块可访问其私有字段) ----
impl AdminProvidersService {
    pub async fn admin_zcode_login_init(
        &self,
        env: Option<ZcodeLoginInitRequestEnv>,
    ) -> AdminZcodeLoginInitResponse {
        let env = if matches!(env, Some(ZcodeLoginInitRequestEnv::Cn)) {
            "cn"
        } else {
            "intl"
        };
        match self.zcode_oauth.init(env).await {
            Ok((init, poll_token)) => AdminZcodeLoginInitResponse::Ok(ZcodeLoginInitResult {
                flow_id: init.flow_id,
                authorize_url: init.authorize_url,
                poll_token,
            }),
            Err(e) => {
                AdminZcodeLoginInitResponse::BadGateway(problem(502, "oauth_init_failed", &e))
            }
        }
    }

    /// ZCode 在线登录:轮询;ready → 用 Coding Plan JWT 自动建渠道
    /// (kind=zcode / anthropic / 官方 base / 模型 GLM-5.3 系列 /
    /// name 默认授权邮箱)。
    ///
    /// `POST /api/providers/zcode/login-poll`
    pub async fn admin_zcode_login_poll(
        &self,
        body: ZcodeLoginPollRequest,
    ) -> AdminZcodeLoginPollResponse {
        if body.flow_id.trim().is_empty() || body.poll_token.trim().is_empty() {
            return AdminZcodeLoginPollResponse::BadRequest(problem(
                400,
                "invalid_request",
                "flow_id 与 poll_token 必填",
            ));
        }
        let env = if matches!(body.env, Some(ZcodeLoginPollRequestEnv::Cn)) {
            "cn"
        } else {
            "intl"
        };
        let status = self
            .zcode_oauth
            .poll(body.flow_id.trim(), body.poll_token.trim())
            .await;
        match status {
            ZcodePollStatus::Pending => AdminZcodeLoginPollResponse::Ok(ZcodeLoginPollResult {
                status: ZcodeLoginPollResultStatus::Pending,
                email: None,
                provider: None,
            }),
            ZcodePollStatus::Expired => AdminZcodeLoginPollResponse::Ok(ZcodeLoginPollResult {
                status: ZcodeLoginPollResultStatus::Expired,
                email: None,
                provider: None,
            }),
            ZcodePollStatus::Failed(msg) => {
                tracing::warn!("zcode oauth poll failed: {msg}");
                AdminZcodeLoginPollResponse::Ok(ZcodeLoginPollResult {
                    status: ZcodeLoginPollResultStatus::Failed,
                    email: None,
                    provider: None,
                })
            }
            ZcodePollStatus::Ready { token, email } => {
                // name 三级兜底:显式 name → 授权账号标识(email/phone,
                // 国内手机号登录时 email 常为空串)→ zcode-user。
                let name = body
                    .name
                    .clone()
                    .map(|n| n.trim().to_string())
                    .filter(|n| !n.is_empty())
                    .or_else(|| Some(email.trim().to_string()).filter(|e| !e.is_empty()))
                    .unwrap_or_else(|| "zcode-user".to_string());
                // 建渠道 base 与登录账号体系同环境(凭证互不通用)
                let (base, origin_note) = if env == "cn" {
                    ("https://zcode.bigmodel.cn", "国内 BigModel 账号")
                } else {
                    ("https://zcode.z.ai", "国际 z.ai 账号")
                };
                let cand = ProviderCandidate {
                    name,
                    provider_type: Some("anthropic".into()),
                    openai_base_url: Some(base.into()),
                    anthropic_base_url: Some(base.into()),
                    api_key: token,
                    models: vec!["GLM-5.3".into(), "GLM-5.3-Flash".into()],
                    priority: body.priority,
                    weight: body.weight,
                    max_retries: None,
                    timeout_secs: None,
                    proxy_url: None,
                    model_mapping: Default::default(),
                    protocols: Some(vec!["anthropic".into()]),
                    default_protocol: Some("anthropic".into()),
                    note: Some(format!("在线登录自动创建({origin_note},{email})")),
                    website_url: Some(
                        if env == "cn" {
                            "https://bigmodel.cn"
                        } else {
                            "https://zcode.z.ai"
                        }
                        .into(),
                    ),
                    channel_kind: Some("zcode".into()),
                };
                match validate_candidate(&cand) {
                    Ok(config) => match insert_checked(self.store.as_ref(), &cand, config).await {
                        Ok(row) => {
                            // 免费额度不会自动到账:建好渠道后台自动领取一遍
                            // (fire-and-forget,结果入 proxy_task 快照;失败可随时手动再领)。
                            let svc = self.clone();
                            let row_id = row.id.clone();
                            tokio::spawn(async move {
                                let r = match svc.admin_zcode_claim_quota_sourced(row_id.clone(), "oauth").await {
                                    aikun_openapi::openapi::server::errors::AdminZcodeClaimQuotaResponse::Ok(r) => r,
                                    _ => {
                                        tracing::warn!("zcode oauth 自动领取:渠道已不存在({row_id})");
                                        return;
                                    }
                                };
                                if r.problems.is_empty() {
                                    tracing::info!(
                                        "zcode oauth 自动领取完成: claimed={:?} skipped={:?}",
                                        r.claimed,
                                        r.skipped
                                    );
                                } else {
                                    tracing::warn!(
                                        "zcode oauth 自动领取有问题: claimed={:?} skipped={:?} problems={:?}",
                                        r.claimed,
                                        r.skipped,
                                        r.problems
                                    );
                                }
                            });
                            AdminZcodeLoginPollResponse::Ok(ZcodeLoginPollResult {
                                status: ZcodeLoginPollResultStatus::Ready,
                                email: Some(email),
                                provider: Some(to_api_provider(&row)),
                            })
                        }
                        Err(p) => AdminZcodeLoginPollResponse::BadRequest(problem(
                            p.status as u16,
                            &p.code,
                            &p.detail.unwrap_or_default(),
                        )),
                    },
                    Err(p) => AdminZcodeLoginPollResponse::BadRequest(problem(
                        p.status as u16,
                        &p.code,
                        &p.detail.unwrap_or_default(),
                    )),
                }
            }
        }
    }

    /// `POST /api/providers/{id}/zcode/claim`——领取 ZCode 渠道的免费/
    /// 营销额度(官方免费/体验额度不会自动到账,需登录态 JWT 走领取
    /// 序列,见 [`ZcodeClaimPort`])。
    pub async fn admin_zcode_claim_quota(&self, id: String) -> AdminZcodeClaimQuotaResponse {
        self.admin_zcode_claim_quota_sourced(id, "manual").await
    }

    /// 领取核心(manual/oauth/auto 三来源共用):领取后把结果写进
    /// proxy_task 快照(签到面板数据源;写失败只留痕不影响领取响应)。
    pub async fn admin_zcode_claim_quota_sourced(
        &self,
        id: String,
        source: &str,
    ) -> AdminZcodeClaimQuotaResponse {
        let row = match self.store.find_provider(&id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return AdminZcodeClaimQuotaResponse::NotFound(problem(
                    404,
                    "not_found",
                    "渠道不存在",
                ));
            }
            Err(()) => {
                return AdminZcodeClaimQuotaResponse::NotFound(problem(
                    502,
                    "store_error",
                    "渠道读取失败",
                ));
            }
        };
        if row.channel_kind.trim() != aikun_channels::adapters::zcode::KIND {
            return AdminZcodeClaimQuotaResponse::BadRequest(problem(
                400,
                "invalid_channel_kind",
                "仅 zcode 渠道支持领取额度",
            ));
        }
        // 领取端点跟渠道自身的 base 走(国内渠道打国内域,与凭证环境一致)
        let base = if row.anthropic_base_url.trim().is_empty() {
            row.openai_base_url.trim().to_string()
        } else {
            row.anthropic_base_url.trim().to_string()
        };
        let (jwt, _) = match self.store.find_provider_credentials(&id).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                return AdminZcodeClaimQuotaResponse::NotFound(problem(
                    404,
                    "not_found",
                    "渠道不存在",
                ));
            }
            Err(()) => {
                return AdminZcodeClaimQuotaResponse::BadRequest(problem(
                    502,
                    "store_error",
                    "渠道凭证读取失败",
                ));
            }
        };
        let result = self.claim_quota_sequence(&base, &jwt).await;
        let ok = result.problems.is_empty();
        if let Err(()) = self
            .store
            .upsert_proxy_task(ProxyTaskRow {
                provider_id: id.clone(),
                task_type: "zcode_claim".to_string(),
                last_run_at: chrono::Utc::now().to_rfc3339(),
                source: source.to_string(),
                claimed: result.claimed.clone(),
                skipped: result.skipped.clone(),
                problems: result.problems.clone(),
            })
            .await
        {
            tracing::warn!("proxy_task 快照写入失败(provider={id})");
        }
        let _ = ok;
        AdminZcodeClaimQuotaResponse::Ok(result)
    }

    /// `GET /api/providers/zcode/tasks`——签到面板(zcode_claim 快照全量)。
    pub async fn admin_list_zcode_tasks(&self) -> AdminListZcodeTasksResponse {
        match self.store.list_proxy_tasks(Some("zcode_claim")).await {
            Ok(rows) => AdminListZcodeTasksResponse::Ok(ProxyTaskList {
                tasks: rows
                    .into_iter()
                    .map(|r| ProxyTaskRecord {
                        provider_id: r.provider_id,
                        task_type: r.task_type,
                        last_run_at: r.last_run_at,
                        source: match r.source.as_str() {
                            "oauth" => ProxyTaskRecordSource::Oauth,
                            "auto" => ProxyTaskRecordSource::Auto,
                            _ => ProxyTaskRecordSource::Manual,
                        },
                        claimed: r.claimed,
                        skipped: r.skipped,
                        problems: r.problems.clone(),
                        ok: r.problems.is_empty(),
                    })
                    .collect(),
            }),
            Err(()) => {
                tracing::error!("list proxy tasks: query failed");
                AdminListZcodeTasksResponse::InternalServerError(crate::problem(
                    500,
                    "query_failed",
                    "查询签到任务失败",
                ))
            }
        }
    }

    /// 领取序列(管理端按钮与 OAuth ready 自动领取共用):激活事件 →
    /// 已持有清单 → 待领清单 → 逐项 claim → campaign confirm。
    /// 事件/余额失败不阻断(记 problem 继续)——领取本体才是目的;
    /// preview 拉不到则无事可做,直接带问题返回。
    async fn claim_quota_sequence(&self, base: &str, jwt: &str) -> AdminZcodeClaimResult {
        let mut claimed: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        let mut problems: Vec<String> = Vec::new();

        if let Err(p) = self.zcode_claim.report_activation(base, jwt).await {
            problems.push(format!("activation: {p}"));
        }
        // 已持有清单拉不到按空集处理:claim 回 1003(已领取)同样视为成功。
        let owned = self
            .zcode_claim
            .owned_plans(base, jwt)
            .await
            .unwrap_or_else(|p| {
                problems.push(format!("balance: {p}"));
                Vec::new()
            });
        let offers = match self.zcode_claim.offered_plans(base, jwt).await {
            Ok(o) => o,
            Err(p) => {
                problems.push(format!("preview: {p}"));
                return AdminZcodeClaimResult {
                    claimed,
                    skipped,
                    problems,
                };
            }
        };
        for plan in offers {
            let display = if plan.name.trim().is_empty() {
                plan.plan_id.clone()
            } else {
                plan.name.clone()
            };
            if owned.iter().any(|o| o.eq_ignore_ascii_case(&plan.plan_id)) {
                skipped.push(display);
                continue;
            }
            match self.zcode_claim.claim_plan(base, jwt, &plan.plan_id).await {
                Ok(true) => {
                    claimed.push(display);
                    if !plan.campaign_id.is_empty()
                        && let Err(p) = self
                            .zcode_claim
                            .confirm_campaign(base, jwt, &plan.campaign_id)
                            .await
                    {
                        problems.push(format!("confirm {}: {p}", plan.campaign_id));
                    }
                }
                // 上游 1003:已领取(比如 balance 拉取失败走了空集)。
                Ok(false) => skipped.push(display),
                Err(p) => problems.push(format!("claim {display}: {p}")),
            }
        }
        AdminZcodeClaimResult {
            claimed,
            skipped,
            problems,
        }
    }
}
