//! ZCode 应用业务:OAuth 在线登录 + 免费额度领取(签到)。
//!
//! 从 admin_providers 主模块拆出(2026-11-18 渠道业务模块化):端口
//! trait(HTTP 经装配层注入,crate 纯净度)与 [`AdminProvidersService`]
//! 的 zcode 专属方法都在这里;渠道 CRUD/导入导出等通用面见 `mod.rs`。

use aikun_openapi::openapi::server::errors::{
    AdminListZcodeTasksResponse, AdminZcodeAccountInfoCachedResponse,
    AdminZcodeAccountInfoResponse, AdminZcodeClaimQuotaResponse, AdminZcodeLoginInitResponse,
    AdminZcodeLoginPollResponse, AdminZcodeResetQuotaResponse,
};
use aikun_openapi::openapi::types::{
    AdminZcodeClaimResult, ProxyTaskList, ProxyTaskRecord, ProxyTaskRecordSource, ZcodeAccountInfo,
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
    /// token = zcode JWT(data.token);business_token = 家族业务 token
    /// (`data.bigmodel.access_token` / `data.zai.access_token`,monitor 域
    /// 的订阅与 5h/周额度查询要用;官方客户端无此字段即判响应无效)。
    Ready {
        token: String,
        email: String,
        business_token: Option<String>,
    },
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

    /// GET billing/balance 返回 data 原始对象(零消耗;一键获取账号信息
    /// 的数据源——套餐/额度/账号字段都在里面)。
    async fn fetch_balance(&self, base: &str, jwt: &str) -> Result<serde_json::Value, String>;

    /// GET coding-plan/reset/status 的 data(零消耗):可用 5h/周重置次数
    /// (含过期时间)与最近使用记录。双头认证(X-Bigmodel-Authorization +
    /// Bigmodel-Target-Type: PERSONAL)由实现侧负责。
    async fn reset_status(&self, base: &str, jwt: &str) -> Result<serde_json::Value, String>;

    /// POST coding-plan/reset/use(**消耗性**:扣一次重置机会,把 5h/周
    /// 额度窗清零重来)。reset_type = "FIVE_HOUR" | "WEEK";实现侧自生成
    /// idempotency_key。返回 data(used 布尔在里面)。
    async fn reset_use(
        &self,
        base: &str,
        jwt: &str,
        reset_type: &str,
    ) -> Result<serde_json::Value, String>;

    /// GET {monitor 域}/api/biz/subscription/list 的 data(订阅信息:
    /// productId / productName / billingCycle / expireTime)。鉴权用
    /// OAuth 业务 token;monitor 域按国内/国际家族选择(实现侧解析,
    /// env `AIKUN_ZCODE_BIGMODEL_HOST` / `AIKUN_ZCODE_ZAI_HOST` 可覆盖)。
    async fn fetch_subscription(
        &self,
        domestic: bool,
        token: &str,
    ) -> Result<serde_json::Value, String>;

    /// GET {monitor 域}/api/monitor/usage/quota/limit 的 data(5 小时窗
    /// 与每周窗的用量额度:`{level, limits:[...]}`)。
    async fn fetch_quota(&self, domestic: bool, token: &str) -> Result<serde_json::Value, String>;
}

// ---- 服务方法(挂在主模块的服务上;子模块可访问其私有字段) ----
impl AdminProvidersService {
    /// OAuth 业务 token 加密落库(monitor 域订阅/5h·周额度凭证)。
    /// 空值跳过;失败只留痕——订阅信息是增强能力,不该阻断建渠道。
    async fn persist_business_token(&self, provider_id: &str, token: Option<&str>) {
        let Some(token) = token.filter(|t| !t.trim().is_empty()) else {
            return;
        };
        let enc = self.store.encrypt_key(token);
        if let Err(()) = self.store.upsert_zcode_oauth(provider_id, &enc).await {
            tracing::warn!("zcode 业务 token 落库失败({provider_id}),订阅/额度查询将不可用");
        }
    }

    /// 读回业务 token 明文(zcode_oauth 行解密;无则 None)。
    async fn business_token_of(&self, provider_id: &str) -> Option<String> {
        let enc = self.store.find_zcode_oauth(provider_id).await.ok()??;
        let plain = self.store.decrypt_key(&enc);
        (!plain.trim().is_empty()).then_some(plain)
    }

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
            ZcodePollStatus::Ready {
                token,
                email,
                business_token,
            } => {
                // name 不依赖授权响应里的 email/phone(国内手机号账号常为
                // 空串,踩过 400 坑;用户裁决 2026-11-19):显式 name →
                // 自动生成 zcode-{6 hex},后续管理员自行改名标记;账号
                // 标识可随时用 token 调上游查询,不阻塞建渠道。
                let name = body
                    .name
                    .clone()
                    .map(|n| n.trim().to_string())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| format!("zcode-{}", random_hex6()));
                // API 端点统一走 zcode.z.ai(2026-11-19 实测:zcode.bigmodel.cn
                // 的 API 路径全部 301 到 bigmodel.cn 营销页,不可用;OAuth 与
                // API 同源,国内 BigModel 账号的 JWT 同样打 zcode.z.ai)。
                // 国内标记进 note 前缀([国内],分页归组用;账号信息回填保留)。
                let (base, origin_note) = if env == "cn" {
                    ("https://zcode.z.ai", "[国内] BigModel 账号")
                } else {
                    ("https://zcode.z.ai", "国际 z.ai 账号")
                };
                let cand = ProviderCandidate {
                    name,
                    provider_type: Some("anthropic".into()),
                    openai_base_url: Some(base.into()),
                    anthropic_base_url: Some(base.into()),
                    api_key: token.clone(),
                    models: Vec::new(),
                    priority: body.priority,
                    weight: body.weight,
                    max_retries: None,
                    timeout_secs: None,
                    proxy_url: None,
                    model_mapping: Default::default(),
                    protocols: Some(vec!["anthropic".into()]),
                    default_protocol: Some("anthropic".into()),
                    note: Some(format!("{origin_note}|在线登录自动创建({email})")),
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
                            // 业务 token(monitor 域订阅/5h·周额度凭证)加密落库
                            self.persist_business_token(&row.id, business_token.as_deref())
                                .await;
                            // 免费额度不会自动到账:建好渠道后台自动领取一遍
                            // (fire-and-forget,结果入 proxy_task 快照;失败可随时手动再领)。
                            // 同时拉一次账号信息，用 balance 回写实际可用模型。
                            let svc = self.clone();
                            let row_id = row.id.clone();
                            tokio::spawn(async move {
                                let _ = svc.admin_zcode_account_info(row_id.clone()).await;
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
                        // 同一账号重复在线登录:定位既有渠道回填业务 token
                        //(订阅/额度信息凭证),不报 409——重复登录是用户
                        // 补全凭证的正常路径(2026-11-20)。
                        Err(p) if p.code == "duplicate_key" => {
                            let hash = self.store.hash_key(&token);
                            let existing = self
                                .store
                                .find_provider_id_by_key_hash(&hash)
                                .await
                                .ok()
                                .flatten();
                            match existing {
                                Some(pid) => {
                                    self.persist_business_token(&pid, business_token.as_deref())
                                        .await;
                                    let svc = self.clone();
                                    let pid2 = pid.clone();
                                    tokio::spawn(async move {
                                        let _ = svc.admin_zcode_account_info(pid2).await;
                                    });
                                    let row = self.store.find_provider(&pid).await.ok().flatten();
                                    AdminZcodeLoginPollResponse::Ok(ZcodeLoginPollResult {
                                        status: ZcodeLoginPollResultStatus::Ready,
                                        email: Some(email),
                                        provider: row.as_ref().map(to_api_provider),
                                    })
                                }
                                None => AdminZcodeLoginPollResponse::BadRequest(problem(
                                    p.status as u16,
                                    &p.code,
                                    &p.detail.unwrap_or_default(),
                                )),
                            }
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

    /// `POST /api/providers/{id}/zcode/account-info`——token 查上游
    /// (全部零消耗)聚合:balance(主数据源,失败即整体 502)+ 重置
    /// 次数 + 订阅/5h·周额度(需业务 token;子项失败降级 null,不阻断)。
    /// 回填名称(仅自动生成名)与 note 摘要。
    pub async fn admin_zcode_account_info(&self, id: String) -> AdminZcodeAccountInfoResponse {
        let row = match self.store.find_provider(&id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return AdminZcodeAccountInfoResponse::NotFound(problem(
                    404,
                    "not_found",
                    "渠道不存在",
                ));
            }
            Err(()) => {
                return AdminZcodeAccountInfoResponse::NotFound(problem(
                    502,
                    "store_error",
                    "渠道读取失败",
                ));
            }
        };
        if row.channel_kind.trim() != aikun_channels::adapters::zcode::KIND {
            return AdminZcodeAccountInfoResponse::BadRequest(problem(
                400,
                "invalid_channel_kind",
                "仅 zcode 渠道支持获取账号信息",
            ));
        }
        let base = if row.anthropic_base_url.trim().is_empty() {
            row.openai_base_url.trim().to_string()
        } else {
            row.anthropic_base_url.trim().to_string()
        };
        let (jwt, _) = match self.store.find_provider_credentials(&id).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                return AdminZcodeAccountInfoResponse::NotFound(problem(
                    404,
                    "not_found",
                    "渠道不存在",
                ));
            }
            Err(()) => {
                return AdminZcodeAccountInfoResponse::BadRequest(problem(
                    502,
                    "store_error",
                    "渠道凭证读取失败",
                ));
            }
        };

        let balance = match self.zcode_claim.fetch_balance(&base, &jwt).await {
            Ok(v) => v,
            Err(e) => {
                return AdminZcodeAccountInfoResponse::BadGateway(problem(
                    502,
                    "upstream_error",
                    &e,
                ));
            }
        };

        // 子查询:重置次数(JWT 双头即可)+ 订阅/额度(需业务 token,
        // monitor 域按渠道归组选家族:[国内] → bigmodel,否则 zai)。
        // 失败降级 null + warnings 说明,不阻断主响应。
        let mut warnings: Vec<String> = Vec::new();
        let domestic = row.note.trim().starts_with("[国内]");
        let token = self.business_token_of(&id).await;
        let reset_res = self.zcode_claim.reset_status(&base, &jwt).await;
        // 无业务 token:订阅/额度一并跳过,单条说明(不报两条空错误)。
        let (sub_res, quota_res) = match &token {
            Some(t) => (
                self.zcode_claim.fetch_subscription(domestic, t).await,
                self.zcode_claim.fetch_quota(domestic, t).await,
            ),
            None => {
                warnings.push(
                    "订阅与 5h/周额度未查询:该渠道无 monitor 业务 token(重新在线登录即可补全)"
                        .to_string(),
                );
                (Ok(serde_json::Value::Null), Ok(serde_json::Value::Null))
            }
        };
        let reset_status = match reset_res {
            Ok(v) => Some(Some(v)),
            Err(e) => {
                warnings.push(format!("重置次数查询失败: {e}"));
                Some(None)
            }
        };
        let subscription = match sub_res {
            Ok(v) if v.is_null() => None,
            Ok(v) => Some(Some(v)),
            Err(e) => {
                warnings.push(format!("订阅信息查询失败: {e}"));
                Some(None)
            }
        };
        let quota = match quota_res {
            Ok(v) if v.is_null() => None,
            Ok(v) => Some(Some(v)),
            Err(e) => {
                warnings.push(format!("额度查询失败: {e}"));
                Some(None)
            }
        };

        // 账号标识:常见字段里取第一个非空字符串
        let pick = |keys: &[&str]| -> Option<String> {
            keys.iter().find_map(|k| {
                balance[k]
                    .as_str()
                    .filter(|v| !v.trim().is_empty())
                    .map(str::to_string)
            })
        };
        let account_id = pick(&["account_id", "accountId", "user_id", "id"]);
        let display = pick(&["email", "phone", "username", "name"]);
        let plan_name = balance["plans"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|p| p["name"].as_str())
            .map(str::to_string);
        // 模型不在 balances[].show_name（那是额度桶名，如 CREDIT_LIMIT）。
        // 从 capabilities / entitlements / 像模型名的 show_name 收集。
        let models = extract_zcode_models(&balance);
        let models_json = serde_json::to_string(&models).unwrap_or_else(|_| "[]".into());
        // 回填(2026-11-20 改版):**只回填名称**——列表名称即备注
        // (自由修改),自动生成名 zcode- 前缀才替换为账号标识;note 不再
        // 写摘要(仅保留 [国内] 归组标记,账号信息的持久化改走
        // zcode_account 缓存侧表)。
        let mut name_updated = false;
        let patch = super::ProviderPatch {
            name: if row.name.trim().starts_with("zcode-") {
                let picked = display.clone().or_else(|| account_id.clone());
                if picked.is_some() {
                    name_updated = true;
                }
                picked
            } else {
                None
            },
            models_json: Some(models_json),
            ..Default::default()
        };
        let _ = self.store.update_provider(&id, &patch).await;
        let info = ZcodeAccountInfo {
            account_id,
            display,
            plan_name,
            balance,
            name_updated,
            reset_status,
            subscription,
            quota,
            plan_credentials: token.is_some(),
            warnings,
        };
        // 持久化缓存(详情窗口 GET 读;失败只留痕不影响响应)
        if let Ok(json) = serde_json::to_string(&info)
            && let Err(()) = self.store.upsert_zcode_account(&id, &json).await
        {
            tracing::warn!("zcode 账号信息缓存写入失败({id})");
        }
        AdminZcodeAccountInfoResponse::Ok(info)
    }

    /// `GET /api/providers/{id}/zcode/account-info`——读缓存的账号信息
    /// (零上游调用;POST 获取成功即刷新缓存。详情窗口打开先读缓存,
    /// 需要最新再 POST 拉新)。
    pub async fn admin_zcode_account_info_cached(
        &self,
        id: String,
    ) -> AdminZcodeAccountInfoCachedResponse {
        use aikun_openapi::openapi::types::ZcodeAccountInfoCached;
        let row = match self.store.find_provider(&id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return AdminZcodeAccountInfoCachedResponse::NotFound(problem(
                    404,
                    "not_found",
                    "渠道不存在",
                ));
            }
            Err(()) => {
                return AdminZcodeAccountInfoCachedResponse::NotFound(problem(
                    502,
                    "store_error",
                    "渠道读取失败",
                ));
            }
        };
        if row.channel_kind.trim() != aikun_channels::adapters::zcode::KIND {
            return AdminZcodeAccountInfoCachedResponse::BadRequest(problem(
                400,
                "invalid_channel_kind",
                "仅 zcode 渠道支持账号信息",
            ));
        }
        match self.store.find_zcode_account(&id).await {
            Ok(Some((json, fetched_at))) => match serde_json::from_str::<ZcodeAccountInfo>(&json) {
                Ok(info) => AdminZcodeAccountInfoCachedResponse::Ok(ZcodeAccountInfoCached {
                    fetched_at: Some(fetched_at),
                    info: Some(Some(info)),
                }),
                // 旧版本/损坏的缓存:按未获取处理(下次 POST 重建)
                Err(e) => {
                    tracing::warn!("zcode 账号信息缓存解析失败({id}): {e}");
                    AdminZcodeAccountInfoCachedResponse::Ok(ZcodeAccountInfoCached {
                        fetched_at: None,
                        info: None,
                    })
                }
            },
            Ok(None) => AdminZcodeAccountInfoCachedResponse::Ok(ZcodeAccountInfoCached {
                fetched_at: None,
                info: None,
            }),
            Err(()) => AdminZcodeAccountInfoCachedResponse::BadRequest(problem(
                502,
                "store_error",
                "账号信息缓存读取失败",
            )),
        }
    }

    /// `POST /api/providers/{id}/zcode/reset-quota`——使用一次额度重置
    /// (**消耗性**;上游 coding-plan/reset/use,reset_type FIVE_HOUR|WEEK)。
    /// 成功后回读最新重置状态一并返回(前端直接刷新显示)。
    pub async fn admin_zcode_reset_quota(
        &self,
        id: String,
        body: aikun_openapi::openapi::types::ZcodeResetRequest,
    ) -> AdminZcodeResetQuotaResponse {
        use aikun_openapi::openapi::types::ZcodeResetResult;
        let row = match self.store.find_provider(&id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return AdminZcodeResetQuotaResponse::NotFound(problem(
                    404,
                    "not_found",
                    "渠道不存在",
                ));
            }
            Err(()) => {
                return AdminZcodeResetQuotaResponse::NotFound(problem(
                    502,
                    "store_error",
                    "渠道读取失败",
                ));
            }
        };
        if row.channel_kind.trim() != aikun_channels::adapters::zcode::KIND {
            return AdminZcodeResetQuotaResponse::BadRequest(problem(
                400,
                "invalid_channel_kind",
                "仅 zcode 渠道支持额度重置",
            ));
        }
        let reset_type = body.reset_type.as_str();
        let base = if row.anthropic_base_url.trim().is_empty() {
            row.openai_base_url.trim().to_string()
        } else {
            row.anthropic_base_url.trim().to_string()
        };
        let (jwt, _) = match self.store.find_provider_credentials(&id).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                return AdminZcodeResetQuotaResponse::NotFound(problem(
                    404,
                    "not_found",
                    "渠道不存在",
                ));
            }
            Err(()) => {
                return AdminZcodeResetQuotaResponse::BadRequest(problem(
                    502,
                    "store_error",
                    "渠道凭证读取失败",
                ));
            }
        };
        let data = match self.zcode_claim.reset_use(&base, &jwt, reset_type).await {
            Ok(v) => v,
            Err(e) => {
                return AdminZcodeResetQuotaResponse::BadGateway(problem(
                    502,
                    "upstream_error",
                    &e,
                ));
            }
        };
        let used = data["used"].as_bool().unwrap_or(false);
        // 回读最新状态(失败给空对象——重置本身已成功,不为展示失败报错)
        let reset_status = self
            .zcode_claim
            .reset_status(&base, &jwt)
            .await
            .unwrap_or(serde_json::Value::Null);
        AdminZcodeResetQuotaResponse::Ok(ZcodeResetResult { used, reset_status })
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

/// 渠道自动命名的短随机后缀(rand 已是 crate 依赖的等价物:这里手写
/// 无依赖 xorshift,避免仅为 6 个字符引运行时)。
/// 从 balance 响应收集可用模型。
/// - `capabilities` 里的 `model:glm-5.4` → `GLM-5.4`
/// - `entitlements[].show_name` / `balances[].show_name` 若像模型名（含 GLM/Flash 等）则收录
/// - 跳过额度桶名（CREDIT_LIMIT 等）
fn extract_zcode_models(balance: &serde_json::Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |out: &mut Vec<String>, raw: &str| {
        let name = normalize_model_name(raw);
        if !name.is_empty() && !out.iter().any(|m| m.eq_ignore_ascii_case(&name)) {
            out.push(name);
        }
    };
    fn walk(v: &serde_json::Value, out: &mut Vec<String>, push: &dyn Fn(&mut Vec<String>, &str)) {
        match v {
            serde_json::Value::Object(map) => {
                if let Some(caps) = map.get("capabilities").and_then(|c| c.as_array()) {
                    for c in caps {
                        if let Some(s) = c.as_str() {
                            if let Some(rest) = s.strip_prefix("model:") {
                                push(out, rest);
                            }
                        }
                    }
                }
                if let Some(name) = map.get("show_name").and_then(|s| s.as_str()) {
                    if looks_like_model(name) {
                        push(out, name);
                    }
                }
                for val in map.values() {
                    walk(val, out, push);
                }
            }
            serde_json::Value::Array(arr) => {
                for item in arr {
                    walk(item, out, push);
                }
            }
            _ => {}
        }
    }
    walk(balance, &mut out, &push);
    out
}

fn looks_like_model(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    if l.contains("credit") || l.contains("limit") || l.contains("quota") || l == "额度" {
        return false;
    }
    l.contains("glm")
        || l.contains("flash")
        || l.contains("minimax")
        || l.contains("deepseek")
        || l.contains("qwen")
        || l.contains("mimo")
}

fn normalize_model_name(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    // model:glm-5.4 / glm-5.3-flash → GLM-5.4 / GLM-5.3-Flash
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("glm") {
        let rest = &s[3.min(s.len())..];
        let mut out = String::from("GLM");
        let mut cap = true;
        for c in rest.chars() {
            if c == '-' || c == '.' {
                out.push(c);
                cap = true;
            } else if cap && c.is_ascii_alphabetic() {
                out.push(c.to_ascii_uppercase());
                cap = false;
            } else {
                out.push(c);
                if c.is_ascii_alphabetic() {
                    cap = false;
                }
            }
        }
        return out;
    }
    s.to_string()
}

fn random_hex6() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut x = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0x9e3779b9);
    let mut out = String::with_capacity(6);
    for _ in 0..6 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push(std::char::from_digit((x & 0xf) as u32, 16).unwrap_or('0'));
    }
    out
}
