//! # aikun-admin:管理面 API 业务实现的家
//!
//! impl `aikun_openapi::openapi::server::api::{AuthApi, UserApi, AdminApi}`,
//! 用 `aikun_openapi::openapi::server::router::{auth_api_router, user_api_router,
//! admin_api_router}` 挂路由。装配(中间件分层、路由合并、删旧 handler)
//! 一律在二进制 `aikun/src/main.rs`(architecture-v0.5 §三.3)。
//!
//! ## 文件组织(并行批次轨道,见 docs/migration-playbook.md)
//!
//! | 文件 | tag / 方法组 | 指派 |
//! |---|---|---|
//! | `auth.rs` | Auth tag(login / serveFrontend / serveStaticAsset) | 试点工人·Auth(已交付) |
//! | `user.rs` | User tag 全部 12 op | 并行工人·User |
//! | `admin_providers.rs` | Admin tag 的 providers/model-health/channel-kinds 组 | 并行工人·Providers |
//! | `admin_users.rs` | Admin tag 的 users 组 | 并行工人·Users |
//! | `admin_billing.rs` | Admin tag 的 prices/balance/redemption 组 | 并行工人·Billing |
//! | `admin_stats.rs` | Admin tag 的 usage-stats/system-stats 组 | 并行工人·Stats |
//!
//! **并行纪律**:工人只写三样——自己名下的 impl 文件、自己的
//! `aikun/tests/e2e_<tag>.rs`、指派给自己的前端 js;不碰本 lib.rs、彼此的
//! impl 文件、`main.rs`、`tests/e2e.rs` 与存量旧 handler——装配与删旧由
//! 集成工人统一做。
//!
//! ## 共享助手
//!
//! `problem()` 与 `sqlite_utc()` 是试点(Auth)批引入的全 crate 助手,放在本
//! 文件;工人需要新助手时先在自己 impl 文件里 `pub fn`,集成阶段再收编,
//! 避免并行改本文件。

#![allow(clippy::too_many_arguments)]

pub mod admin_billing;
pub mod admin_providers;
pub mod admin_stats;
pub mod admin_users;
pub mod auth;
pub mod user;

use aikun_openapi::openapi::server::api::{AdminApi, AdminListRedemptionCodesStatus};
// 请求/域类型在 types;每 op 的响应枚举(*Response)在 server::errors。
use aikun_openapi::openapi::server::errors::*;
use aikun_openapi::openapi::types::*;

/// Admin tag 的单一 `AdminApi` 实现者(路由工厂只收一个 T)。
/// 四个方法组分别在四个 impl 文件里以固有方法实现,本 facade 一行委托;
/// 委托签名随 spec 重生成才会变,工人不要手改本文件。
#[derive(Clone)]
pub struct AdminService {
    /// providers / model-health / channel-kinds 组(`admin_providers.rs`)
    pub providers: admin_providers::AdminProvidersService,
    /// users 组(`admin_users.rs`)
    pub users: admin_users::AdminUsersService,
    /// prices / balance-transactions / redemption-codes 组(`admin_billing.rs`)
    pub billing: admin_billing::AdminBillingService,
    /// usage-stats / system-stats 组(`admin_stats.rs`)
    pub stats: admin_stats::AdminStatsService,
}

#[async_trait::async_trait]
impl AdminApi for AdminService {
    async fn admin_adjust_user_balance(
        &self,
        id: String,
        body: AdjustBalanceRequest,
    ) -> AdminAdjustUserBalanceResponse {
        self.billing.admin_adjust_user_balance(id, body).await
    }
    async fn admin_create_price(&self, body: UpsertPriceRequest) -> AdminCreatePriceResponse {
        self.billing.admin_create_price(body).await
    }
    async fn admin_create_provider(
        &self,
        body: CreateProviderRequest,
    ) -> AdminCreateProviderResponse {
        self.providers.admin_create_provider(body).await
    }
    async fn admin_create_providers_batch(
        &self,
        body: CreateProvidersBatchRequest,
    ) -> AdminCreateProvidersBatchResponse {
        self.providers.admin_create_providers_batch(body).await
    }
    async fn admin_create_redemption_codes(
        &self,
        body: CreateRedemptionCodesRequest,
    ) -> AdminCreateRedemptionCodesResponse {
        self.billing.admin_create_redemption_codes(body).await
    }
    async fn admin_create_user(&self, body: CreateUserRequest) -> AdminCreateUserResponse {
        self.users.admin_create_user(body).await
    }
    async fn admin_create_users_batch(
        &self,
        body: BatchCreateUsersRequest,
    ) -> AdminCreateUsersBatchResponse {
        self.users.admin_create_users_batch(body).await
    }
    async fn admin_delete_price(&self, id: String) -> AdminDeletePriceResponse {
        self.billing.admin_delete_price(id).await
    }
    async fn admin_delete_provider(&self, id: String) -> AdminDeleteProviderResponse {
        self.providers.admin_delete_provider(id).await
    }
    async fn admin_delete_user(&self, id: String) -> AdminDeleteUserResponse {
        self.users.admin_delete_user(id).await
    }
    async fn admin_disable_redemption_code(
        &self,
        id: String,
    ) -> AdminDisableRedemptionCodeResponse {
        self.billing.admin_disable_redemption_code(id).await
    }
    async fn admin_duplicate_provider(&self, id: String) -> AdminDuplicateProviderResponse {
        self.providers.admin_duplicate_provider(id).await
    }
    async fn admin_fetch_upstream_models(
        &self,
        body: FetchModelsRequest,
    ) -> AdminFetchUpstreamModelsResponse {
        self.providers.admin_fetch_upstream_models(body).await
    }
    async fn admin_get_global_stats(&self) -> AdminGetGlobalStatsResponse {
        self.stats.admin_get_global_stats().await
    }
    async fn admin_get_provider(&self, id: String) -> AdminGetProviderResponse {
        self.providers.admin_get_provider(id).await
    }
    async fn admin_get_usage_stats(&self) -> AdminGetUsageStatsResponse {
        self.stats.admin_get_usage_stats().await
    }
    async fn admin_get_user(&self, id: String) -> AdminGetUserResponse {
        self.users.admin_get_user(id).await
    }
    async fn admin_list_billing_transactions(
        &self,
        limit: ::std::option::Option<i64>,
        offset: ::std::option::Option<i64>,
        user_id: ::std::option::Option<String>,
    ) -> AdminListBillingTransactionsResponse {
        self.billing
            .admin_list_billing_transactions(limit, offset, user_id)
            .await
    }
    async fn admin_list_channel_kinds(&self) -> AdminListChannelKindsResponse {
        self.providers.admin_list_channel_kinds().await
    }
    async fn admin_list_model_health(
        &self,
        limit: ::std::option::Option<i64>,
        offset: ::std::option::Option<i64>,
        provider_id: ::std::option::Option<String>,
    ) -> AdminListModelHealthResponse {
        self.providers
            .admin_list_model_health(limit, offset, provider_id)
            .await
    }
    async fn admin_list_prices(
        &self,
        limit: ::std::option::Option<i64>,
        offset: ::std::option::Option<i64>,
        in_use: ::std::option::Option<String>,
    ) -> AdminListPricesResponse {
        self.billing.admin_list_prices(limit, offset, in_use).await
    }
    async fn admin_list_providers(
        &self,
        limit: ::std::option::Option<i64>,
        offset: ::std::option::Option<i64>,
        category: ::std::option::Option<
            aikun_openapi::openapi::server::api::AdminListProvidersCategory,
        >,
        kind: ::std::option::Option<String>,
    ) -> AdminListProvidersResponse {
        let cat = category.as_ref().map(|c| match c {
            aikun_openapi::openapi::server::api::AdminListProvidersCategory::Standard => "standard",
            aikun_openapi::openapi::server::api::AdminListProvidersCategory::Proxy => "proxy",
        });
        self.providers
            .admin_list_providers(cat, kind.as_deref(), limit, offset)
            .await
    }
    async fn admin_list_zcode_tasks(
        &self,
    ) -> aikun_openapi::openapi::server::errors::AdminListZcodeTasksResponse {
        self.providers.admin_list_zcode_tasks().await
    }
    async fn admin_bulk_providers(
        &self,
        body: aikun_openapi::openapi::types::ProvidersBulkRequest,
    ) -> aikun_openapi::openapi::server::errors::AdminBulkProvidersResponse {
        self.providers.admin_bulk_providers(body).await
    }
    async fn admin_export_providers(
        &self,
        category: ::std::option::Option<
            aikun_openapi::openapi::server::api::AdminExportProvidersCategory,
        >,
        kind: ::std::option::Option<String>,
    ) -> aikun_openapi::openapi::server::errors::AdminExportProvidersServerResponse {
        let cat = category.as_ref().map(|c| match c {
            aikun_openapi::openapi::server::api::AdminExportProvidersCategory::Standard => {
                "standard"
            }
            aikun_openapi::openapi::server::api::AdminExportProvidersCategory::Proxy => "proxy",
        });
        self.providers
            .admin_export_providers(cat, kind.as_deref())
            .await
    }
    async fn admin_import_providers(
        &self,
        body: aikun_openapi::openapi::types::AdminImportProvidersRequest,
    ) -> aikun_openapi::openapi::server::errors::AdminImportProvidersResponse {
        self.providers.admin_import_providers(body).await
    }
    async fn admin_zcode_login_init(
        &self,
        body: ::std::option::Option<aikun_openapi::openapi::types::ZcodeLoginInitRequest>,
    ) -> aikun_openapi::openapi::server::errors::AdminZcodeLoginInitResponse {
        self.providers
            .admin_zcode_login_init(body.and_then(|b| b.env))
            .await
    }
    async fn admin_zcode_login_poll(
        &self,
        body: aikun_openapi::openapi::types::ZcodeLoginPollRequest,
    ) -> aikun_openapi::openapi::server::errors::AdminZcodeLoginPollResponse {
        self.providers.admin_zcode_login_poll(body).await
    }
    async fn admin_list_proxies(
        &self,
    ) -> aikun_openapi::openapi::server::errors::AdminListProxiesResponse {
        self.providers.admin_list_proxies().await
    }
    async fn admin_create_proxy(
        &self,
        body: aikun_openapi::openapi::types::CreateProxyRequest,
    ) -> aikun_openapi::openapi::server::errors::AdminCreateProxyResponse {
        self.providers.admin_create_proxy(body).await
    }
    async fn admin_delete_proxy(
        &self,
        id: String,
    ) -> aikun_openapi::openapi::server::errors::AdminDeleteProxyResponse {
        self.providers.admin_delete_proxy(id).await
    }
    async fn admin_zcode_account_info(
        &self,
        id: String,
    ) -> aikun_openapi::openapi::server::errors::AdminZcodeAccountInfoResponse {
        self.providers.admin_zcode_account_info(id).await
    }
    async fn admin_zcode_account_info_cached(
        &self,
        id: String,
    ) -> aikun_openapi::openapi::server::errors::AdminZcodeAccountInfoCachedResponse {
        self.providers.admin_zcode_account_info_cached(id).await
    }
    async fn admin_zcode_reset_quota(
        &self,
        id: String,
        body: aikun_openapi::openapi::types::ZcodeResetRequest,
    ) -> aikun_openapi::openapi::server::errors::AdminZcodeResetQuotaResponse {
        self.providers.admin_zcode_reset_quota(id, body).await
    }
    async fn admin_zcode_claim_quota(
        &self,
        id: String,
    ) -> aikun_openapi::openapi::server::errors::AdminZcodeClaimQuotaResponse {
        self.providers.admin_zcode_claim_quota(id).await
    }
    async fn admin_list_redemption_codes(
        &self,
        limit: ::std::option::Option<i64>,
        offset: ::std::option::Option<i64>,
        batch: ::std::option::Option<String>,
        status: ::std::option::Option<AdminListRedemptionCodesStatus>,
    ) -> AdminListRedemptionCodesResponse {
        self.billing
            .admin_list_redemption_codes(limit, offset, batch, status)
            .await
    }
    async fn admin_list_users(
        &self,
        limit: ::std::option::Option<i64>,
        offset: ::std::option::Option<i64>,
    ) -> AdminListUsersResponse {
        self.users.admin_list_users(limit, offset).await
    }
    async fn admin_test_provider(&self, id: String) -> AdminTestProviderResponse {
        self.providers.admin_test_provider(id).await
    }
    async fn admin_test_provider_model(
        &self,
        id: String,
        body: ModelTestRequest,
    ) -> AdminTestProviderModelResponse {
        self.providers.admin_test_provider_model(id, body).await
    }
    async fn admin_update_price(
        &self,
        id: String,
        body: UpsertPriceRequest,
    ) -> AdminUpdatePriceResponse {
        self.billing.admin_update_price(id, body).await
    }
    async fn admin_update_provider(
        &self,
        id: String,
        body: UpdateProviderRequest,
    ) -> AdminUpdateProviderResponse {
        self.providers.admin_update_provider(id, body).await
    }
    async fn admin_update_user(
        &self,
        id: String,
        body: UpdateUserRequest,
    ) -> AdminUpdateUserResponse {
        self.users.admin_update_user(id, body).await
    }
}

// ---------- 共享助手(Auth 试点批引入) ----------

/// 构造 RFC 9457 problem+json 错误体(`Problem` 是所有响应枚举错误变体的
/// 载荷)。`code` 是机器可读短码(词表见 `types.rs` Problem 文档);
/// `type` 恒 `about:blank`(spec 预留位);`title` 用 HTTP 标准短语。
pub fn problem(status: u16, code: &str, detail: &str) -> Problem {
    Problem {
        code: code.to_string(),
        detail: Some(detail.to_string()),
        instance: None,
        status: i64::from(status),
        title: axum::http::StatusCode::from_u16(status)
            .ok()
            .and_then(|s| s.canonical_reason().map(str::to_string)),
        r#type: "about:blank".parse().ok(),
    }
}

/// SQLite `datetime('now')` 字符串(`YYYY-MM-DD HH:MM:SS`,UTC)→
/// `DateTime<Utc>`;RFC3339 兜底(测试夹具可能直插两种格式);
/// 均失败回 epoch(出参类型非可选,DB 约束保证不会走到)。
pub fn sqlite_utc(s: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .map(|n| n.and_utc())
        .ok()
        .or_else(|| {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|d| d.with_timezone(&chrono::Utc))
                .ok()
        })
        .unwrap_or_default()
}
