//! # SurrealDB 计费域实现桩(阶段 S)
//!
//! **指派:并行工人 · Billing**(分工边界与纪律见
//! `docs/surreal-migration.md` §七)。
//!
//! ## 你负责的域
//!
//! 计费与日志相关的全部表:`model_price`、`request_log`、
//! `api_key_usage_daily`、`usage_daily`、`billing_transaction`、
//! `redemption_code`、`channel_affinity`。
//!
//! ## 两条红线(任何时候不许变)
//!
//! 1. **计费扣费事务原子性**:[`BillingStore::record_request`] 的
//!    日志插入 + 日用量计数器累加 + 余额扣减必须**原子**。实现口径:
//!    单次 `db.query` 内 `BEGIN TRANSACTION; …; COMMIT TRANSACTION;`
//!    (多语句一次提交;语句失败服务器端整体回滚,不经任何中间 await)。
//!    与 SQLite 侧 `billing/ledger.rs` 的事务边界逐句对齐:价格折算在
//!    事务**外**(进程内价格缓存),`cost` 为 0 时不写余额,api_key 为
//!    None 不计计数器。
//! 2. **兑换码核销事务原子性**:[`BillingStore::redeem_code`] 的
//!    校验(unused/未过期)→ 标记 used → 用户余额入账 → 记
//!    billing_transaction 必须同一 `BEGIN TRANSACTION…COMMIT` 块;
//!    并发兑换同一码只有一个成功(事务内先 SELECT 后 UPDATE)。
//!
//! ## 其余实现约定(必读)
//!
//! * **金额一律 i64 微元**(balance/cost/amount/balance_after/
//!   redemption amount);价格表(prompt/completion/cached,元/1M tokens)
//!   保持 f64 ——与 SQLite 侧口径一致,红线只约束金额。
//! * 计数器表 `api_key_usage_daily` 数组 record id
//!   `api_key_usage_daily:[api_key:<id>, '<YYYY-MM-DD>']`,`UPSERT … SET
//!   tokens_spent += $n` 等价 SQLite 的 `ON CONFLICT DO UPDATE`。
//! * `record_request` 关联列(user/api_key/provider)直接写 record 链接,
//!   **不做 EXISTS 预检降级**(Surreal 无强制 FK,dangling 链接由
//!   `query_logs` 读侧降级为 None——语义与 SQLite 的 ON DELETE SET NULL
//!   对齐,见设计文档 §四.request_log)。
//! * `purge_expired_logs`:先按 (user, date) 聚合将被删除的明细到
//!   `usage_daily`(数组 id UPSERT),再删明细——对账依据永久保留
//!   (语义同 SQLite 侧 purge)。
//! * 价格缓存令牌 `price_table_token`:口径 = `count + max(updated_at)`
//!   聚合(同 SQLite);价格 CRUD 必须 bump updated_at。
//! * `seed_default_prices_if_empty`:复用 `crate::pricing` 的快照常量
//!   (default_prices.json 内嵌,汇率折算口径不变);价格表非空返回 0。
//! * 日志时间过滤:created_at 是原生 datetime,日界按 UTC——计数器
//!   date 字符串 `YYYY-MM-DD` 与旧 `substr(created_at,1,10)` 口径一致。
//! * 语义基准 = SQLite 实现(`aikun/src/main.rs` 的 `DbBillingStore`/
//!   `DbStatsStore` 适配器、`billing/ledger.rs`、`schema.rs` 的
//!   rebuild_usage_counters);对外契约不变。

use crate::surreal::SurrealHandle;
use crate::{CacheToken, KeyUsageSnapshot, PriceRule, StoreError};

/// 一笔请求的记账入参(字段与 SQLite `insert_request_log_ext` 对齐;
/// cost 由调用方按进程内价格缓存折算后传入,微元)。
#[derive(Debug, Clone)]
pub struct RequestLedgerEntry {
    pub user_id: String,
    pub api_key_id: Option<String>,
    pub provider_id: Option<String>,
    pub model: String,
    pub request_type: String,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
    pub cached_tokens: i32,
    pub latency_ms: i32,
    pub status_code: i32,
    pub success: bool,
    pub error_message: Option<String>,
    /// 单次请求费用(微元);0 = 不扣(失败路径/缓存 free)。
    pub cost_micro: i64,
}

/// 日志行(管理面/用户自助日志列表出参;关联维度已读侧解析为字符串)。
#[derive(Debug, Clone)]
pub struct LogRow {
    pub id: String,
    pub user_id: Option<String>,
    pub username: Option<String>,
    pub api_key_id: Option<String>,
    pub provider_id: Option<String>,
    pub provider_name: Option<String>,
    pub model: String,
    pub request_type: String,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
    pub cached_tokens: i32,
    pub latency_ms: i32,
    pub status_code: i32,
    pub success: bool,
    pub error_message: Option<String>,
    pub cost_micro: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// 日志过滤(None = 不过滤;语义对齐 SQLite 侧 LogFilters)。
#[derive(Debug, Clone, Default)]
pub struct LogFilters {
    pub user_id: Option<String>,
    pub api_key_id: Option<String>,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub success: Option<bool>,
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    pub until: Option<chrono::DateTime<chrono::Utc>>,
}

/// 兑换结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedeemOutcome {
    /// 核销成功,入账面值(微元)与账后余额(微元)。
    Redeemed {
        amount_micro: i64,
        balance_after_micro: i64,
    },
    /// 码不存在 / 已用 / 已禁用 / 已过期(不区分,防枚举)。
    Invalid,
}

/// 计费域能力:async trait(async 化决策见设计文档 §六)。
#[async_trait::async_trait]
pub trait BillingStore: Send + Sync {
    // ---- 价格表 ----

    /// 全量价格条目(进程内缓存消费;顺序不限)。
    async fn price_rules(&self) -> Result<Vec<PriceRule>, StoreError>;

    /// 价格表缓存令牌(count + max updated_at)。
    async fn price_table_token(&self) -> Result<CacheToken, StoreError>;

    /// 价格表为空时播种内置默认价(复用 crate::pricing 快照),返回导入
    /// 条数;非空返回 0(不覆盖手工数据)。
    async fn seed_default_prices_if_empty(&self) -> Result<usize, StoreError>;

    /// 创建/按模型 upsert 价格;model 唯一。
    async fn upsert_price(&self, rule: PriceRule) -> Result<PriceRule, StoreError>;

    /// 删除价格;返回是否删除。
    async fn delete_price(&self, model: &str) -> Result<bool, StoreError>;

    /// 价格列表(ORDER BY model),返回 (total, 页)。
    async fn list_prices(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<PriceRule>), StoreError>;

    // ---- 记账热路径(红线 1) ----

    /// 原子记账:请求日志 + 日用量计数器 + 余额扣减(见文件头红线 1)。
    async fn record_request(&self, entry: RequestLedgerEntry) -> Result<(), StoreError>;

    /// 单 key 限额 + 当日已用(计数器点查;行不存在按 0 返回)。
    async fn key_limits_and_usage(
        &self,
        api_key_id: &str,
        date: chrono::NaiveDate,
    ) -> Result<KeyUsageSnapshot, StoreError>;

    /// 从 request_log 明细重建 date 当日全部计数器(幂等:先清当日再
    /// 聚合);返回重建的 key 数。
    async fn rebuild_usage_counters(&self, date: chrono::NaiveDate) -> Result<usize, StoreError>;

    // ---- 日志 ----

    /// 日志列表(过滤 + ORDER BY created_at DESC LIMIT/OFFSET),
    /// 返回 (total, 页);dangling 关联读侧降级 None(文件头)。
    async fn query_logs(
        &self,
        filters: &LogFilters,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<LogRow>), StoreError>;

    /// 清理保留期外的明细:先聚合到 usage_daily(对账依据)再删。
    /// 返回删除条数。
    async fn purge_expired_logs(&self, retain_days: u32) -> Result<u64, StoreError>;

    // ---- 调账(红线 1 同级:余额写 + 流水同事务) ----

    /// 管理员调账(recharge/adjust):余额变更 + billing_transaction
    /// 同一事务;amount 为微元(正/负)。返回账后余额(微元)。
    async fn adjust_balance(
        &self,
        user_id: &str,
        amount_micro: i64,
        kind: &str,
        note: &str,
    ) -> Result<i64, StoreError>;

    /// 用户调账流水(ORDER BY created_at DESC),返回 (total, 页)。
    async fn list_transactions(
        &self,
        user_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<TransactionRow>), StoreError>;

    // ---- 兑换码(红线 2) ----

    /// 批量生成兑换码(code_hash=sha256(code),明文由调用方生成并
    /// 一次性返回);amount 面值微元。返回生成的行。
    async fn create_redemption_codes(
        &self,
        codes: Vec<NewRedemptionCode>,
    ) -> Result<Vec<RedemptionCodeRow>, StoreError>;

    /// 兑换码列表(batch/status 过滤),返回 (total, 页)。
    async fn list_redemption_codes(
        &self,
        batch: &str,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<RedemptionCodeRow>), StoreError>;

    /// 禁用兑换码(DELETE 语义:unused → disabled;已用不可禁)。
    /// 返回是否生效。
    async fn disable_redemption_code(&self, id: &str) -> Result<bool, StoreError>;

    /// 原子核销(红线 2)。
    async fn redeem_code(
        &self,
        user_id: &str,
        code_hash: &str,
    ) -> Result<RedeemOutcome, StoreError>;

    // ---- 渠道亲和(请求路径状态;归本域因其为 /v1 热路径写) ----

    /// 读 (api_key, model) 的亲和渠道;无/悬空返回 None。
    async fn get_affinity(
        &self,
        api_key_id: &str,
        model: &str,
    ) -> Result<Option<String>, StoreError>;

    /// 写亲和(UPSERT 数组 id `channel_affinity:[api_key, model]`)。
    async fn set_affinity(
        &self,
        api_key_id: &str,
        model: &str,
        provider_id: &str,
    ) -> Result<(), StoreError>;

    // ---- 统计 ----

    /// 用户/全站用量汇总(usage_daily + 计数器聚合;口径对齐
    /// SQLite 侧 stats 适配器)。返回 (total_requests, total_tokens,
    /// total_cost_micro)。
    async fn usage_summary(
        &self,
        user_id: Option<&str>,
        since: Option<chrono::NaiveDate>,
        until: Option<chrono::NaiveDate>,
    ) -> Result<(i64, i64, i64), StoreError>;

    /// 系统概况(users/keys/providers/活跃渠道计数等)。
    async fn system_stats(&self) -> Result<SystemStats, StoreError>;
}

/// 新兑换码(明文 code 已由调用方销毁,只带哈希与掩码)。
#[derive(Debug, Clone)]
pub struct NewRedemptionCode {
    pub code_hash: String,
    pub code_suffix: String,
    pub amount_micro: i64,
    pub batch: String,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub note: String,
}

/// 兑换码行。
#[derive(Debug, Clone)]
pub struct RedemptionCodeRow {
    pub id: String,
    pub code_suffix: String,
    pub amount_micro: i64,
    pub batch: String,
    pub status: String,
    pub used_by: Option<String>,
    pub used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub note: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// 调账流水行。
#[derive(Debug, Clone)]
pub struct TransactionRow {
    pub id: String,
    pub user_id: String,
    pub amount_micro: i64,
    pub balance_after_micro: i64,
    pub kind: String,
    pub note: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// 系统概况(字段对齐 SQLite 侧 system-stats 出参)。
#[derive(Debug, Clone, Default)]
pub struct SystemStats {
    pub users: i64,
    pub active_users: i64,
    pub api_keys: i64,
    pub active_api_keys: i64,
    pub providers: i64,
    pub active_providers: i64,
    pub unused_redemption_codes: i64,
}

/// 计费域的 SurrealDB 实现(桩:工人按上述约定填 todo!() 体)。
pub struct SurrealBilling {
    handle: SurrealHandle,
}

impl SurrealBilling {
    pub fn new(handle: SurrealHandle) -> Self {
        Self { handle }
    }
}

#[async_trait::async_trait]
impl BillingStore for SurrealBilling {
    // ---- 价格表 ----

    async fn price_rules(&self) -> Result<Vec<PriceRule>, StoreError> {
        let _ = &self.handle;
        todo!(
            "工人·Billing:SELECT model, prompt_price, completion_price, cached_price FROM model_price"
        )
    }

    async fn price_table_token(&self) -> Result<CacheToken, StoreError> {
        let _ = &self.handle;
        todo!("工人·Billing:count + max(updated_at) 聚合 → CacheToken::new")
    }

    async fn seed_default_prices_if_empty(&self) -> Result<usize, StoreError> {
        let _ = &self.handle;
        todo!("工人·Billing:空表时导入 crate::pricing 快照(ULID key)")
    }

    async fn upsert_price(&self, rule: PriceRule) -> Result<PriceRule, StoreError> {
        let _ = (&self.handle, rule);
        todo!("工人·Billing:UPSERT by model(unique),updated_at = time::now()")
    }

    async fn delete_price(&self, model: &str) -> Result<bool, StoreError> {
        let _ = (&self.handle, model);
        todo!("工人·Billing:DELETE WHERE model = $m")
    }

    async fn list_prices(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<PriceRule>), StoreError> {
        let _ = (&self.handle, limit, offset);
        todo!("工人·Billing:SELECT ... ORDER BY model LIMIT/START")
    }

    // ---- 记账热路径 ----

    async fn record_request(&self, entry: RequestLedgerEntry) -> Result<(), StoreError> {
        let _ = (&self.handle, entry);
        todo!(
            "工人·Billing:BEGIN TRANSACTION; CREATE request_log; UPSERT 计数器; \
             (cost>0 时) UPDATE user SET balance = balance - $cost; COMMIT(红线 1)"
        )
    }

    async fn key_limits_and_usage(
        &self,
        api_key_id: &str,
        date: chrono::NaiveDate,
    ) -> Result<KeyUsageSnapshot, StoreError> {
        let _ = (&self.handle, api_key_id, date);
        todo!("工人·Billing:api_key 限额 + api_key_usage_daily:[key, date] 点查")
    }

    async fn rebuild_usage_counters(&self, date: chrono::NaiveDate) -> Result<usize, StoreError> {
        let _ = (&self.handle, date);
        todo!("工人·Billing:清当日计数器 → 按明细 GROUP BY api_key 回填(幂等)")
    }

    // ---- 日志 ----

    async fn query_logs(
        &self,
        filters: &LogFilters,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<LogRow>), StoreError> {
        let _ = (&self.handle, filters, limit, offset);
        todo!("工人·Billing:过滤 + LEFT JOIN 用户/渠道名 + dangling 降级 None")
    }

    async fn purge_expired_logs(&self, retain_days: u32) -> Result<u64, StoreError> {
        let _ = (&self.handle, retain_days);
        todo!("工人·Billing:先聚合 usage_daily 再删明细(对账依据)")
    }

    // ---- 调账 ----

    async fn adjust_balance(
        &self,
        user_id: &str,
        amount_micro: i64,
        kind: &str,
        note: &str,
    ) -> Result<i64, StoreError> {
        let _ = (&self.handle, user_id, amount_micro, kind, note);
        todo!("工人·Billing:事务内 UPDATE balance + CREATE billing_transaction")
    }

    async fn list_transactions(
        &self,
        user_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<TransactionRow>), StoreError> {
        let _ = (&self.handle, user_id, limit, offset);
        todo!("工人·Billing:SELECT WHERE user = ... ORDER created_at DESC")
    }

    // ---- 兑换码 ----

    async fn create_redemption_codes(
        &self,
        codes: Vec<NewRedemptionCode>,
    ) -> Result<Vec<RedemptionCodeRow>, StoreError> {
        let _ = (&self.handle, codes);
        todo!("工人·Billing:批量 CREATE(ULID key)")
    }

    async fn list_redemption_codes(
        &self,
        batch: &str,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<RedemptionCodeRow>), StoreError> {
        let _ = (&self.handle, batch, status, limit, offset);
        todo!("工人·Billing:过滤 + 分页")
    }

    async fn disable_redemption_code(&self, id: &str) -> Result<bool, StoreError> {
        let _ = (&self.handle, id);
        todo!("工人·Billing:unused → disabled(已用不可禁)")
    }

    async fn redeem_code(
        &self,
        user_id: &str,
        code_hash: &str,
    ) -> Result<RedeemOutcome, StoreError> {
        let _ = (&self.handle, user_id, code_hash);
        todo!(
            "工人·Billing:BEGIN; SELECT by code_hash; 校验 unused/未过期; \
             UPDATE status/used_by/used_at; UPDATE user balance; \
             CREATE billing_transaction; COMMIT(红线 2)"
        )
    }

    // ---- 亲和 ----

    async fn get_affinity(
        &self,
        api_key_id: &str,
        model: &str,
    ) -> Result<Option<String>, StoreError> {
        let _ = (&self.handle, api_key_id, model);
        todo!("工人·Billing:SELECT channel_affinity:[api_key:<id>, $model]")
    }

    async fn set_affinity(
        &self,
        api_key_id: &str,
        model: &str,
        provider_id: &str,
    ) -> Result<(), StoreError> {
        let _ = (&self.handle, api_key_id, model, provider_id);
        todo!("工人·Billing:UPSERT channel_affinity:[api_key:<id>, $model]")
    }

    // ---- 统计 ----

    async fn usage_summary(
        &self,
        user_id: Option<&str>,
        since: Option<chrono::NaiveDate>,
        until: Option<chrono::NaiveDate>,
    ) -> Result<(i64, i64, i64), StoreError> {
        let _ = (&self.handle, user_id, since, until);
        todo!("工人·Billing:usage_daily 聚合(requests/tokens/cost 微元)")
    }

    async fn system_stats(&self) -> Result<SystemStats, StoreError> {
        let _ = &self.handle;
        todo!("工人·Billing:各表 count 聚合")
    }
}
