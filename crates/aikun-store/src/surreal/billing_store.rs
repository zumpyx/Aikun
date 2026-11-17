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

use surrealdb::IndexedResults;
use surrealdb::types::{Array, RecordId, RecordIdKey, SurrealValue, Value as Sval};

use crate::surreal::SurrealHandle;
use crate::{CacheToken, KeyUsageSnapshot, PriceRule, StoreError};

// ---------- 本文件内部的实现约定 ----------
//
// * **ID 口径**:trait 面的 id 一律是**裸 record key**(新建行 = ULID,迁移
//   旧行 = 旧 uuid,与 SQLite 时代的外部 id 完全一致——设计文档 §三
//   "管理面展示的旧 ID 不变")。库内 record id 是 `表:key`,行映射边界
//   剥一次表前缀(见 [`bare_id`]/[`rid`]);入参同时容忍 `表:key` 完整
//   形式(幂等归一)。
// * **读行方式**:行统一 take 成 `serde_json::Value`(SDK 的全量转换:
//   datetime → RFC3339 字符串、record 链接 → `表:key` 字符串,见
//   surrealdb-types value/into_json.rs),再用本文件的小工具取字段——
//   免为每张表 derive SurrealValue(设计文档 §一"优先用现成 impl")。
// * **UPSERT 语义**:`INSERT … ON DUPLICATE KEY UPDATE`,更新子句里
//   `$input.<字段>` 引用本行待插入值(surrealdb-core exec/ir 的
//   DuplicateUpdate 行名绑定)——SQLite `ON CONFLICT DO UPDATE … excluded`
//   的逐值对位;裸 `UPSERT … SET x = x + $n` 在新建行上会得 NONE,不用。
//   另:DELETE 语句默认不回行,计数一律 `RETURN BEFORE`;LET 语法是
//   单等号(`LET $x = …`)。
// * **事务**:红线操作(记账/核销/调账/批量建码/purge 聚合+删除)全部
//   是单次 `run()` 内 `BEGIN TRANSACTION; …; COMMIT TRANSACTION;`,
//   语句级错误经 `check()` 带出且服务器端整体回滚(设计文档 §一)。
//   结果行按**语句序号** take(BEGIN/LET/IF 也占一个序号,缺失序号 take
//   返回空,见 SDK opt/query.rs 的 usize 实现)。

/// 一笔请求的记账入参(字段与 SQLite `insert_request_log_ext` 对齐;
/// cost 由调用方按进程内价格缓存折算后传入,微元)。
#[derive(Debug, Clone)]
pub struct RequestLedgerEntry {
    /// 请求追踪号(网关入口生成的 UUID v4,9.6;一次多渠道重试的各次
    /// 落账共享同一 id)。迁移旧行等历史数据无此值(库内 option)。
    pub request_id: Option<String>,
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
    /// 请求追踪号(9.6;旧行为空)。
    pub request_id: Option<String>,
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

    // ---- 集成批补充(默认切换装配面;口径 = SQLite 侧对应 SQL) ----

    /// 日志统计聚合(user tag get-log-stats):过滤同 [`LogFilters`],
    /// 单查询出 (total, tokens, avg_latency, success_pct, cost 微元)。
    async fn log_stats(&self, filters: &LogFilters) -> Result<LogStatsRaw, StoreError>;

    /// 钱包序列(user tag get-wallet-stats):daily 是明细(request_log)
    /// 与聚合(usage_daily)按日**合并**的两表之和(purge 同事务聚合+删除,
    /// 相加不重复计数);top_models 只看明细,按费用降序 LIMIT 8。
    /// `since_date` 形如 `YYYY-MM-DD`。
    async fn wallet_series(
        &self,
        user_id: &str,
        since_date: &str,
    ) -> Result<WalletSeries, StoreError>;

    /// 仪表盘用量聚合(admin usage-stats):近 30 天窗口的
    /// today/week/month 三组计数 + 日序列 + top 模型(日期口径 =
    /// `time::format(created_at, "%Y-%m-%d")`,与旧 substr 一致)。
    async fn usage_snapshot(&self) -> Result<UsageSnapshotRaw, StoreError>;

    /// 今日请求数(admin global-stats 的第四项)。
    async fn requests_today(&self) -> Result<i64, StoreError>;

    /// 价格表全量记录(admin 计费页:含 id 与时间戳,ORDER BY model)。
    async fn list_price_records(&self) -> Result<Vec<PriceRecordRow>, StoreError>;

    /// 插入价格行(model 撞 unique 索引 → Err,消息含
    /// `unique_model_price_model`,调用方据此映射 Constraint)。
    async fn insert_price_record(
        &self,
        model: &str,
        prompt_price: f64,
        completion_price: f64,
        cached_price: Option<f64>,
    ) -> Result<PriceRecordRow, StoreError>;

    /// 按 id 部分更新价格(None = 不动;cached_price 双层三态);
    /// updated_at 必 bump(价格缓存令牌)。未命中 None。
    async fn update_price_record(
        &self,
        id: &str,
        model: Option<&str>,
        prompt_price: Option<f64>,
        completion_price: Option<f64>,
        cached_price: Option<Option<f64>>,
    ) -> Result<Option<PriceRecordRow>, StoreError>;

    /// 按 id 删除价格;返回是否删除。
    async fn delete_price_record(&self, id: &str) -> Result<bool, StoreError>;

    /// 调账流水分页(admin 视角:user_id None/空 = 全部;带兑换者用户名,
    /// dangling 降级 None)。ORDER BY created_at DESC, id DESC。
    async fn list_transactions_scoped(
        &self,
        user_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<TxRecordFull>), StoreError>;

    /// 按 id 读单个兑换码(disable 三态判定的存在性回查)。
    async fn get_redemption_code(&self, id: &str) -> Result<Option<RedemptionCodeRow>, StoreError>;
}

/// 日志统计聚合原文(修约与换算在调用方)。
#[derive(Debug, Clone, Copy, Default)]
pub struct LogStatsRaw {
    pub total_requests: i64,
    pub total_tokens: i64,
    pub avg_latency_ms: f64,
    /// 百分数(0..100)
    pub success_rate_pct: f64,
    pub total_cost_micro: i64,
}

/// 钱包序列(daily 已合并明细 + 聚合两表)。
#[derive(Debug, Clone, Default)]
pub struct WalletSeries {
    /// (date "YYYY-MM-DD", cost_micro, tokens, requests) 按日升序。
    pub daily: Vec<(String, i64, i64, i64)>,
    /// (model, cost_micro, requests) 按费用降序 LIMIT 8(明细口径)。
    pub top_models: Vec<(String, i64, i64)>,
}

/// 仪表盘用量聚合原文(近 30 天窗口)。
#[derive(Debug, Clone, Default)]
pub struct UsageSnapshotRaw {
    pub today: (i64, i64),
    pub week: (i64, i64),
    pub month: (i64, i64),
    /// (date, requests, tokens, avg_latency_ms, success_rate_pct) 升序。
    pub daily: Vec<(String, i64, i64, f64, f64)>,
    /// (model, requests, tokens) 按请求数降序 LIMIT 8。
    pub top_models: Vec<(String, i64, i64)>,
}

/// 价格表完整行(管理面出参口径)。
#[derive(Debug, Clone)]
pub struct PriceRecordRow {
    pub id: String,
    pub model: String,
    pub prompt_price: f64,
    pub completion_price: f64,
    pub cached_price: Option<f64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// 调账流水行(admin 视角,带兑换者用户名)。
#[derive(Debug, Clone)]
pub struct TxRecordFull {
    pub id: String,
    pub user_id: String,
    pub username: Option<String>,
    pub amount_micro: i64,
    pub balance_after_micro: i64,
    pub kind: String,
    pub note: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
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

/// 计费域的 SurrealDB 实现(实现约定见文件头;SQL 语义基准 = SQLite
/// 侧行为,红线两条见上)。
pub struct SurrealBilling {
    handle: SurrealHandle,
}

// ---------- 内部小工具(record id / JSON 行值 / 查询执行) ----------

/// 一条语句参数绑定:键为 `$名字`,值为 Surreal 值(String 键用于按行
/// 生成的动态参数名,如批量 INSERT 的 $i0/$m0…)。
type Bind = (String, Sval);

/// 便捷构造:静态键 → Bind 列表。
fn binds(items: Vec<(&str, Sval)>) -> Vec<Bind> {
    items.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// 错误统一包装为 `StoreError::Failed`(细节只进日志,不进对外错误体)。
fn failed(e: impl std::fmt::Display) -> StoreError {
    StoreError::Failed(e.to_string())
}

/// 外部 id → 裸 record key:容忍 `表:key` 完整形式与裸 key 两种输入。
fn record_key(table: &str, id: &str) -> String {
    let id = id.trim();
    match id.strip_prefix(&format!("{table}:")) {
        Some(k) => k.to_string(),
        None => id.to_string(),
    }
}

/// 构造单键 record id。
fn rid(table: &str, id: &str) -> RecordId {
    RecordId::new(table, record_key(table, id))
}

/// 构造复合主键行的数组 record id:`表:[<record 链接>, '<子键>']`
/// (api_key_usage_daily / usage_daily / channel_affinity)。
fn array_rid(table: &str, key: RecordId, sub: &str) -> RecordId {
    RecordId::new(
        table,
        RecordIdKey::Array(Array::from(vec![
            Sval::RecordId(key),
            Sval::String(sub.to_string()),
        ])),
    )
}

/// `表:key` 字符串 → 裸 key(record 链接字段读出的字符串形式剥前缀)。
/// 含连字符的字符串 key(迁移进来的旧 uuid)被反引号包裹序列化,剥掉
/// 还原(identity.rs 的 unquote_key 同款)。
fn bare_id(s: &str) -> String {
    let key = s.split_once(':').map_or(s, |(_, k)| k);
    key.strip_prefix('`')
        .and_then(|x| x.strip_suffix('`'))
        .unwrap_or(key)
        .to_string()
}

/// NaiveDate → `YYYY-MM-DD`(计数器/汇总表的 date 字符串口径,与旧
/// `substr(created_at,1,10)` 一致)。
fn date_str(d: chrono::NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

// ---- JSON 行值取字段(datetime 读出为 RFC3339 字符串,带时区) ----

fn j_str(row: &serde_json::Value, k: &str) -> String {
    row.get(k)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn j_opt_str(row: &serde_json::Value, k: &str) -> Option<String> {
    row.get(k)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn j_i64(row: &serde_json::Value, k: &str) -> i64 {
    row.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0)
}

fn j_f64(row: &serde_json::Value, k: &str) -> f64 {
    row.get(k)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0)
}

fn j_bool(row: &serde_json::Value, k: &str) -> bool {
    row.get(k)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn j_dt(row: &serde_json::Value, k: &str) -> Result<chrono::DateTime<chrono::Utc>, StoreError> {
    j_opt_dt(row, k)?.ok_or_else(|| StoreError::Failed(format!("行缺少 datetime 字段 {k:?}")))
}

fn j_opt_dt(
    row: &serde_json::Value,
    k: &str,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, StoreError> {
    let Some(s) = j_opt_str(row, k) else {
        return Ok(None);
    };
    chrono::DateTime::parse_from_rfc3339(&s)
        .map(|d| d.with_timezone(&chrono::Utc))
        .map(Some)
        .map_err(|e| StoreError::Failed(format!("datetime 字段 {k:?} 解析失败({s}):{e}")))
}

/// `SELECT count() AS c … GROUP ALL` 的首行计数(空表/缺行按 0)。
fn count_of(rows: &[serde_json::Value]) -> i64 {
    rows.first()
        .and_then(|r| r.get("c"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0)
}

/// model_price 行 → PriceRule(cached null = 按输入价计,保持 None)。
fn price_row(row: &serde_json::Value) -> PriceRule {
    PriceRule {
        model: j_str(row, "model"),
        prompt_price: j_f64(row, "prompt_price"),
        completion_price: j_f64(row, "completion_price"),
        cached_price: row.get("cached_price").and_then(serde_json::Value::as_f64),
    }
}

/// LogFilters →(WHERE 子句(含尾随空格,无过滤时为空), 参数绑定)。
/// since/until 为闭区间(>= / <=)。两个语句共用同名参数。
fn log_where(f: &LogFilters) -> (String, Vec<Bind>) {
    let mut cond: Vec<&str> = Vec::new();
    let mut bs: Vec<Bind> = Vec::new();
    if let Some(u) = &f.user_id {
        cond.push("user = $f_user");
        bs.extend(binds(vec![("f_user", rid("user", u).into_value())]));
    }
    if let Some(k) = &f.api_key_id {
        cond.push("api_key = $f_key");
        bs.extend(binds(vec![("f_key", rid("api_key", k).into_value())]));
    }
    if let Some(p) = &f.provider_id {
        cond.push("provider = $f_prov");
        bs.extend(binds(vec![("f_prov", rid("provider", p).into_value())]));
    }
    if let Some(m) = &f.model {
        cond.push("model = $f_model");
        bs.extend(binds(vec![("f_model", m.clone().into_value())]));
    }
    if let Some(s) = f.success {
        cond.push("success = $f_ok");
        bs.extend(binds(vec![("f_ok", s.into_value())]));
    }
    if let Some(s) = f.since {
        cond.push("created_at >= $f_since");
        bs.extend(binds(vec![("f_since", s.into_value())]));
    }
    if let Some(u) = f.until {
        cond.push("created_at <= $f_until");
        bs.extend(binds(vec![("f_until", u.into_value())]));
    }
    let r#where = if cond.is_empty() {
        String::new()
    } else {
        format!("WHERE {} ", cond.join(" AND "))
    };
    (r#where, bs)
}

/// request_log 查询行 → LogRow。dangling 关联读侧降级:目标记录已删时
/// 关联字段(`user.username` 等)读取为 null,id 一并置 None——等价旧
/// `ON DELETE SET NULL` 的可观测语义(schema.rs request_logs 三列同款);
/// 探测列都是目标行的必填列,活行非 null、死行(null)可区分。
fn log_row(row: &serde_json::Value) -> Result<LogRow, StoreError> {
    let username = j_opt_str(row, "username");
    let user_id = username.as_ref().map(|_| bare_id(&j_str(row, "user_id")));
    let ak_alive = j_opt_str(row, "ak_suffix").is_some();
    let api_key_id = ak_alive.then(|| bare_id(&j_str(row, "api_key_id")));
    let provider_name = j_opt_str(row, "provider_name");
    let provider_id = provider_name
        .as_ref()
        .map(|_| bare_id(&j_str(row, "provider_id")));
    Ok(LogRow {
        id: bare_id(&j_str(row, "id")),
        request_id: j_opt_str(row, "request_id"),
        user_id,
        username,
        api_key_id,
        provider_id,
        provider_name,
        model: j_str(row, "model"),
        request_type: j_str(row, "request_type"),
        prompt_tokens: j_i64(row, "prompt_tokens") as i32,
        completion_tokens: j_i64(row, "completion_tokens") as i32,
        total_tokens: j_i64(row, "total_tokens") as i32,
        cached_tokens: j_i64(row, "cached_tokens") as i32,
        latency_ms: j_i64(row, "latency_ms") as i32,
        status_code: j_i64(row, "status_code") as i32,
        success: j_bool(row, "success"),
        error_message: j_opt_str(row, "error_message"),
        cost_micro: j_i64(row, "cost"),
        created_at: j_dt(row, "created_at")?,
    })
}

impl SurrealBilling {
    pub fn new(handle: SurrealHandle) -> Self {
        Self { handle }
    }

    /// 执行一段(可多语句的)SurrealQL:绑定参数 → await → `check()`
    /// 带出首个语句级错误(事务内语句失败即整体回滚,无中间 await)。
    async fn run(&self, sql: &str, bs: &[Bind]) -> Result<IndexedResults, StoreError> {
        let mut q = self.handle.db().query(sql);
        for (k, v) in bs {
            q = q.bind((k.as_str(), v.clone()));
        }
        q.await.map_err(failed)?.check().map_err(failed)
    }
}

#[async_trait::async_trait]
impl BillingStore for SurrealBilling {
    // ---- 价格表 ----

    async fn price_rules(&self) -> Result<Vec<PriceRule>, StoreError> {
        // 全量拉回,匹配(精确 > 最长 `*` 前缀 > `*` 兜底)在进程内经
        // `crate::pricing::find_in_rules` 做——与 SQLite 侧热路径同构,
        // 通配语义零分叉(见本文件测试 price_rules_match_sql_wildcard_semantics)。
        let mut res = self
            .run(
                "SELECT model, prompt_price, completion_price, cached_price FROM model_price",
                &[],
            )
            .await?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        Ok(rows.iter().map(price_row).collect())
    }

    async fn price_table_token(&self) -> Result<CacheToken, StoreError> {
        // 口径 = count + max(updated_at)(文件头约定;SQLite 令牌多一列
        // rowid,Surreal 无 rowid,行数变化仍必被捕获)。
        let mut res = self
            .run(
                "SELECT count() AS c FROM model_price GROUP ALL;\
                 \nSELECT updated_at FROM model_price ORDER BY updated_at DESC LIMIT 1;",
                &[],
            )
            .await?;
        let counts: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        let max_row: Option<serde_json::Value> = res.take(1).map_err(failed)?;
        let max_updated = max_row
            .as_ref()
            .and_then(|r| j_opt_str(r, "updated_at"))
            .unwrap_or_default();
        Ok(CacheToken::new(format!(
            "{}|{}",
            count_of(&counts),
            max_updated
        )))
    }

    async fn seed_default_prices_if_empty(&self) -> Result<usize, StoreError> {
        // 空表才播种;快照/汇率/兜底价统一收口在 crate::pricing(集成批
        // 消除了原桩的汇率常量双处定义,数据本体仍是同一份 JSON)。
        let mut res = self
            .run("SELECT count() AS c FROM model_price GROUP ALL", &[])
            .await?;
        let counts: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        if count_of(&counts) > 0 {
            return Ok(0);
        }
        let rows = crate::pricing::default_price_rules();
        if rows.is_empty() {
            // 快照随二进制内嵌,解析失败是构建期问题(pricing.rs 已留痕)
            return Ok(0);
        }
        let n = rows.len();
        // 分块单事务写入:语句数有界(嵌入式引擎查询在调用任务栈上递归,
        // 设计文档 §一栈配置),空表→非空之间的中断留给下次启动跳过
        // (与 SQLite 侧逐句 autocommit 的部分播种风险同口径)。
        for chunk in rows.chunks(50) {
            let mut sql = String::from(
                "BEGIN TRANSACTION;\nINSERT INTO model_price \
                 (id, model, prompt_price, completion_price, cached_price) VALUES ",
            );
            let mut bs: Vec<Bind> = Vec::with_capacity(chunk.len() * 5);
            for (i, rule) in chunk.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&format!("($i{i}, $m{i}, $p{i}, $c{i}, $k{i})"));
                bs.push((
                    format!("i{i}"),
                    RecordId::new("model_price", ulid::Ulid::new().to_string()).into_value(),
                ));
                bs.push((format!("m{i}"), rule.model.clone().into_value()));
                bs.push((format!("p{i}"), rule.prompt_price.into_value()));
                bs.push((format!("c{i}"), rule.completion_price.into_value()));
                bs.push((format!("k{i}"), rule.cached_price.into_value()));
            }
            sql.push_str(";\nCOMMIT TRANSACTION;");
            self.run(&sql, &bs).await?;
        }
        Ok(n)
    }

    async fn upsert_price(&self, rule: PriceRule) -> Result<PriceRule, StoreError> {
        // 按 model 定位(unique 索引):命中 UPDATE / 未命中 INSERT,单事务;
        // updated_at 必 bump(价格缓存令牌的输入)。
        // 语句序:0 BEGIN / 1 LET / 2 IF / 3 COMMIT / 4 SELECT 回读。
        let mut res = self
            .run(
                "BEGIN TRANSACTION;\
                 \nLET $e = (SELECT VALUE id FROM model_price WHERE model = $m LIMIT 1);\
                 \nIF array::len($e) > 0 {\
                    \nUPDATE model_price SET prompt_price = $p, completion_price = $c, \
                     cached_price = $k, updated_at = time::now() WHERE id = $e[0];\
                 \n} ELSE {\
                    \nINSERT INTO model_price (id, model, prompt_price, completion_price, \
                     cached_price, updated_at) \
                     VALUES ($nid, $m, $p, $c, $k, time::now());\
                 \n};\
                 \nCOMMIT TRANSACTION;\
                 \nSELECT model, prompt_price, completion_price, cached_price \
                  FROM model_price WHERE model = $m;",
                &binds(vec![
                    ("m", rule.model.clone().into_value()),
                    ("p", rule.prompt_price.into_value()),
                    ("c", rule.completion_price.into_value()),
                    ("k", rule.cached_price.into_value()),
                    (
                        "nid",
                        RecordId::new("model_price", ulid::Ulid::new().to_string()).into_value(),
                    ),
                ]),
            )
            .await?;
        let row: Option<serde_json::Value> = res.take(4).map_err(failed)?;
        row.as_ref().map(price_row).ok_or_else(|| {
            StoreError::Failed(format!("upsert_price 回读失败:model={}", rule.model))
        })
    }

    async fn delete_price(&self, model: &str) -> Result<bool, StoreError> {
        // DELETE 默认返回被删行(BEFORE 镜像);数组非空即删除成功。
        let mut res = self
            .run(
                "DELETE model_price WHERE model = $m RETURN BEFORE",
                &binds(vec![("m", model.to_string().into_value())]),
            )
            .await?;
        let deleted: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        Ok(!deleted.is_empty())
    }

    async fn list_prices(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<PriceRule>), StoreError> {
        let mut res = self
            .run(
                &format!(
                    "SELECT count() AS c FROM model_price GROUP ALL;\
                     \nSELECT model, prompt_price, completion_price, cached_price \
                      FROM model_price ORDER BY model LIMIT {limit} START {offset};"
                ),
                &[],
            )
            .await?;
        let counts: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        let rows: Vec<serde_json::Value> = res.take(1).map_err(failed)?;
        Ok((count_of(&counts), rows.iter().map(price_row).collect()))
    }

    // ---- 记账热路径 ----

    async fn record_request(&self, entry: RequestLedgerEntry) -> Result<(), StoreError> {
        // 红线 1:日志插入 + 日用量计数器累加 + 余额扣减在同一个
        // BEGIN…COMMIT 块内,单次 run() 无中间 await;语句失败服务器端
        // 整体回滚。与 ledger.rs 的语句边界逐句对齐:
        // * 价格折算在事务外(调用方折算好 cost_micro 传入);
        // * cost = 0(失败路径/缓存 free/无价格配置)不写余额;
        // * api_key 为 None(JWT/被降级)不计计数器;
        // * 关联列直接写 record 链接,不做 EXISTS 预检(无强制 FK,
        //   dangling 由 query_logs 读侧降级,见文件头)。
        let key_rid = entry.api_key_id.as_deref().map(|k| rid("api_key", k));
        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mut sql = String::from(
            "BEGIN TRANSACTION;\
             \nINSERT INTO request_log (id, request_id, user, api_key, provider, model, \
              request_type, prompt_tokens, completion_tokens, total_tokens, cached_tokens, \
              latency_ms, status_code, success, error_message, cost, created_at) \
              VALUES ($rid, $reqid, $user, $key, $prov, $model, $rt, $pt, $ct, $tt, $kt, \
              $lat, $sc, $ok, $err, $cost, time::now());",
        );
        if key_rid.is_some() {
            // 与记账同事务的日额度计数器累加(数组 id UPSERT 直位)。
            sql.push_str(
                "\nINSERT INTO api_key_usage_daily (id, api_key, date, tokens_spent) \
                 VALUES ($cid, $key, $day, $tt) \
                 ON DUPLICATE KEY UPDATE tokens_spent = tokens_spent + $input.tokens_spent;",
            );
        }
        if entry.cost_micro > 0 {
            // 允许扣成负数(余额预检在入口,用量先发生费用后结算);
            // 整数微元,无浮点漂移。
            sql.push_str("\nUPDATE user SET balance = balance - $cost WHERE id = $user;");
        }
        sql.push_str("\nCOMMIT TRANSACTION;");

        let mut bs = binds(vec![
            (
                "rid",
                RecordId::new("request_log", ulid::Ulid::new().to_string()).into_value(),
            ),
            ("reqid", entry.request_id.clone().into_value()),
            ("user", rid("user", &entry.user_id).into_value()),
            ("key", key_rid.clone().into_value()),
            (
                "prov",
                entry
                    .provider_id
                    .as_deref()
                    .map(|p| rid("provider", p))
                    .into_value(),
            ),
            ("model", entry.model.clone().into_value()),
            ("rt", entry.request_type.clone().into_value()),
            ("pt", i64::from(entry.prompt_tokens).into_value()),
            ("ct", i64::from(entry.completion_tokens).into_value()),
            ("tt", i64::from(entry.total_tokens).into_value()),
            ("kt", i64::from(entry.cached_tokens).into_value()),
            ("lat", i64::from(entry.latency_ms).into_value()),
            ("sc", i64::from(entry.status_code).into_value()),
            ("ok", entry.success.into_value()),
            ("err", entry.error_message.clone().into_value()),
            ("cost", entry.cost_micro.into_value()),
        ]);
        if let Some(k) = &key_rid {
            bs.push((
                "cid".to_string(),
                array_rid("api_key_usage_daily", k.clone(), &day).into_value(),
            ));
            bs.push(("day".to_string(), day.into_value()));
        }
        self.run(&sql, &bs).await?;
        Ok(())
    }

    async fn key_limits_and_usage(
        &self,
        api_key_id: &str,
        date: chrono::NaiveDate,
    ) -> Result<KeyUsageSnapshot, StoreError> {
        // 限额来自 api_key 行;日用量来自计数器点查。行不存在时限额按
        // 0/用量按 0 返回(能力 trait 的文档口径,lib.rs KeyUsage)。
        let mut res = self
            .run(
                "SELECT rate_limit_rpm, quota_daily_tokens, max_concurrent \
                  FROM api_key WHERE id = $k LIMIT 1;\
                 \nSELECT tokens_spent FROM api_key_usage_daily \
                  WHERE api_key = $k AND date = $d LIMIT 1;",
                &binds(vec![
                    ("k", rid("api_key", api_key_id).into_value()),
                    ("d", date_str(date).into_value()),
                ]),
            )
            .await?;
        let limits: Option<serde_json::Value> = res.take(0).map_err(failed)?;
        let usage: Option<serde_json::Value> = res.take(1).map_err(failed)?;
        let (l, u) = (limits.unwrap_or_default(), usage.unwrap_or_default());
        Ok(KeyUsageSnapshot {
            rate_limit_rpm: j_i64(&l, "rate_limit_rpm"),
            quota_daily_tokens: j_i64(&l, "quota_daily_tokens"),
            max_concurrent: j_i64(&l, "max_concurrent"),
            tokens_spent: j_i64(&u, "tokens_spent"),
        })
    }

    async fn rebuild_usage_counters(&self, date: chrono::NaiveDate) -> Result<usize, StoreError> {
        // 幂等:先清当日全部计数器,再按明细 GROUP BY api_key 回填。
        // 日期口径:time::format(created_at) 与旧 substr(created_at,1,10)
        // 一致(UTC 日界)。与 SQLite 侧同构的两步(那边也是 autocommit 的
        // DELETE + INSERT…SELECT 两条,无跨语句事务),重建为维护路径,
        // 竞态窗口与旧实现相同。
        let d = date_str(date);
        let mut res = self
            .run(
                "DELETE api_key_usage_daily WHERE date = $d;\
                 \nSELECT api_key, math::sum(total_tokens) AS tokens FROM request_log \
                  WHERE api_key IS NOT NONE AND time::format(created_at, \"%Y-%m-%d\") = $d \
                  GROUP BY api_key;",
                &binds(vec![("d", d.clone().into_value())]),
            )
            .await?;
        let groups: Vec<serde_json::Value> = res.take(1).map_err(failed)?;
        if groups.is_empty() {
            return Ok(0);
        }
        let mut sql = String::from(
            "BEGIN TRANSACTION;\nINSERT INTO api_key_usage_daily \
             (id, api_key, date, tokens_spent) VALUES ",
        );
        let mut bs: Vec<Bind> = Vec::with_capacity(groups.len() * 2);
        for (i, g) in groups.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&format!("($c{i}, $k{i}, $d, $t{i})"));
            let key = j_str(g, "api_key");
            bs.push((
                format!("c{i}"),
                array_rid("api_key_usage_daily", rid("api_key", &key), &d).into_value(),
            ));
            bs.push((format!("k{i}"), rid("api_key", &key).into_value()));
            bs.push((format!("t{i}"), j_i64(g, "tokens").into_value()));
        }
        // 冲突不可能发生(刚清当日),ON DUPLICATE 仅作竞态兜底(绝对值覆盖)。
        sql.push_str(" ON DUPLICATE KEY UPDATE tokens_spent = $input.tokens_spent;");
        sql.push_str("\nCOMMIT TRANSACTION;");
        bs.push(("d".to_string(), d.into_value()));
        self.run(&sql, &bs).await?;
        Ok(groups.len())
    }

    // ---- 日志 ----

    async fn query_logs(
        &self,
        filters: &LogFilters,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<LogRow>), StoreError> {
        let (r#where, bs) = log_where(filters);
        let mut res = self
            .run(
                &format!(
                    "SELECT count() AS c FROM request_log {W} GROUP ALL;\
                     \nSELECT id, request_id, user.id AS user_id, user.username AS username, \
                      api_key.id AS api_key_id, api_key.key_suffix AS ak_suffix, \
                      provider.id AS provider_id, provider.name AS provider_name, \
                      model, request_type, prompt_tokens, completion_tokens, total_tokens, \
                      cached_tokens, latency_ms, status_code, success, error_message, \
                      cost, created_at \
                      FROM request_log {W} ORDER BY created_at DESC, id DESC \
                      LIMIT {limit} START {offset};",
                    W = r#where
                ),
                &bs,
            )
            .await?;
        let counts: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        let rows: Vec<serde_json::Value> = res.take(1).map_err(failed)?;
        let out = rows.iter().map(log_row).collect::<Result<Vec<_>, _>>()?;
        Ok((count_of(&counts), out))
    }

    async fn purge_expired_logs(&self, retain_days: u32) -> Result<u64, StoreError> {
        // 语义同 health.rs::purge_old_request_logs_batched:
        // * 截止时间只算一次(分批期间 now 前移不扩大清理范围);
        // * 每批一个事务:先将被删明细按 (user, 日) 聚合 UPSERT 进
        //   usage_daily(对账依据永久保留),再删明细;
        // * 批间让出引擎(Surreal 无全局写锁,此处保留分批以限制单事务
        //   的语句量与栈深——嵌入式查询在调用任务栈上递归,设计文档 §一);
        // * 幂等:明细删掉后不再进聚合,重跑不会重复累加。
        const PURGE_BATCH: i64 = 5000;
        let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(retain_days));
        let mut total: u64 = 0;
        loop {
            let mut res = self
                .run(
                    &format!(
                        "SELECT id, user, time::format(created_at, \"%Y-%m-%d\") AS d, \
                          total_tokens, cost FROM request_log \
                          WHERE created_at < $cut ORDER BY created_at, id \
                          LIMIT {PURGE_BATCH};"
                    ),
                    &binds(vec![("cut", cutoff.into_value())]),
                )
                .await?;
            let rows: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
            if rows.is_empty() {
                break;
            }
            // Rust 侧聚合:(user 裸 key, 日期) → [requests, tokens, cost]
            let mut groups: std::collections::BTreeMap<(String, String), [i64; 3]> =
                std::collections::BTreeMap::new();
            let mut ids: Vec<RecordId> = Vec::with_capacity(rows.len());
            for row in &rows {
                // 裸 key 归一(含连字符的字符串 key——迁移进来的旧 uuid——
                // 序列化带反引号,直接 parse 会失败或错位)。
                let id = bare_id(&j_str(row, "id"));
                ids.push(rid("request_log", &id));
                // user 缺失(NONE)不进聚合(旧表 user_id NOT NULL,防御性跳过)
                if let Some(user_full) = j_opt_str(row, "user") {
                    let g = groups
                        .entry((bare_id(&user_full), j_str(row, "d")))
                        .or_insert([0, 0, 0]);
                    g[0] += 1;
                    g[1] += j_i64(row, "total_tokens");
                    g[2] += j_i64(row, "cost");
                }
            }
            // 聚合 + 删除同一事务(任一失败整体回滚,不产生"聚合了没删/删了
            // 没聚合"的双计或丢账中间态)。
            let mut sql = String::from(
                "BEGIN TRANSACTION;\nINSERT INTO usage_daily \
                 (id, user, date, requests, tokens, cost) VALUES ",
            );
            let mut bs: Vec<Bind> = Vec::with_capacity(groups.len() * 6 + 1);
            for (i, ((user, day), [r, t, c])) in groups.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&format!("($g{i}, $u{i}, $d{i}, $r{i}, $t{i}, $c{i})"));
                let user_rid = rid("user", user);
                bs.push((
                    format!("g{i}"),
                    array_rid("usage_daily", user_rid.clone(), day).into_value(),
                ));
                bs.push((format!("u{i}"), user_rid.into_value()));
                bs.push((format!("d{i}"), day.clone().into_value()));
                bs.push((format!("r{i}"), (*r).into_value()));
                bs.push((format!("t{i}"), (*t).into_value()));
                bs.push((format!("c{i}"), (*c).into_value()));
            }
            sql.push_str(
                " ON DUPLICATE KEY UPDATE requests = requests + $input.requests, \
                 tokens = tokens + $input.tokens, cost = cost + $input.cost;",
            );
            sql.push_str("\nDELETE request_log WHERE id IN $ids RETURN BEFORE;");
            sql.push_str("\nCOMMIT TRANSACTION;");
            bs.push(("ids".to_string(), ids.into_value()));
            // 语句序:0 BEGIN / 1 INSERT / 2 DELETE / 3 COMMIT
            let mut res = self.run(&sql, &bs).await?;
            let deleted: Vec<serde_json::Value> = res.take(2).map_err(failed)?;
            let n = deleted.len() as u64;
            total += n;
            if n < PURGE_BATCH as u64 {
                break;
            }
        }
        Ok(total)
    }

    // ---- 调账 ----

    async fn adjust_balance(
        &self,
        user_id: &str,
        amount_micro: i64,
        kind: &str,
        note: &str,
    ) -> Result<i64, StoreError> {
        // 余额变更 + billing_transaction 同一事务(不留"余额变了没流水"
        // 的中间态)。用户不存在:UPDATE 命中 0 行 → 不写流水 → 回读为空
        // → 报错(端口契约里用户缺失是调用方的 404 分支;trait 出参只有
        // i64,以 Failed 表达,集成批装配时映射)。
        // 语句序:0 BEGIN / 1 LET / 2 IF / 3 COMMIT / 4 SELECT 回读。
        let txid = RecordId::new("billing_transaction", ulid::Ulid::new().to_string());
        let urid = rid("user", user_id);
        let mut res = self
            .run(
                "BEGIN TRANSACTION;\
                 \nLET $u = (UPDATE user SET balance = balance + $amt, \
                  updated_at = time::now() WHERE id = $urid RETURN AFTER);\
                 \nIF array::len($u) > 0 {\
                    \nINSERT INTO billing_transaction \
                     (id, user, amount, balance_after, kind, note, created_at) \
                     VALUES ($txid, $urid, $amt, $u[0].balance, $kind, $note, time::now());\
                 \n};\
                 \nCOMMIT TRANSACTION;\
                 \nSELECT balance_after FROM billing_transaction WHERE id = $txid;",
                &binds(vec![
                    ("urid", urid.into_value()),
                    ("amt", amount_micro.into_value()),
                    ("kind", kind.to_string().into_value()),
                    ("note", note.to_string().into_value()),
                    ("txid", txid.clone().into_value()),
                ]),
            )
            .await?;
        let row: Option<serde_json::Value> = res.take(4).map_err(failed)?;
        let row = row
            .ok_or_else(|| StoreError::Failed(format!("adjust_balance:用户 {user_id:?} 不存在")))?;
        Ok(j_i64(&row, "balance_after"))
    }

    async fn list_transactions(
        &self,
        user_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<TransactionRow>), StoreError> {
        let mut res = self
            .run(
                &format!(
                    "SELECT count() AS c FROM billing_transaction WHERE user = $u GROUP ALL;\
                     \nSELECT id, user.id AS user_id, amount, balance_after, kind, note, \
                      created_at FROM billing_transaction WHERE user = $u \
                      ORDER BY created_at DESC, id DESC LIMIT {limit} START {offset};"
                ),
                &binds(vec![("u", rid("user", user_id).into_value())]),
            )
            .await?;
        let counts: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        let rows: Vec<serde_json::Value> = res.take(1).map_err(failed)?;
        let out = rows
            .iter()
            .map(|r| {
                Ok(TransactionRow {
                    id: bare_id(&j_str(r, "id")),
                    user_id: bare_id(&j_str(r, "user_id")),
                    amount_micro: j_i64(r, "amount"),
                    balance_after_micro: j_i64(r, "balance_after"),
                    kind: j_str(r, "kind"),
                    note: j_str(r, "note"),
                    created_at: j_dt(r, "created_at")?,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok((count_of(&counts), out))
    }

    // ---- 兑换码 ----

    async fn create_redemption_codes(
        &self,
        codes: Vec<NewRedemptionCode>,
    ) -> Result<Vec<RedemptionCodeRow>, StoreError> {
        // 整批一个事务:中途失败整体回滚,避免哈希落库而明文从未送达
        // (SQLite 侧同语义)。created_at 由本侧写入(库内形态仍是原生
        // datetime)。unique code_hash 冲突 → 事务整体失败 → Failed
        // (端口层的 Constraint 区分留给集成批映射)。
        if codes.is_empty() {
            return Ok(Vec::new());
        }
        let now = chrono::Utc::now();
        let mut sql = String::from(
            "BEGIN TRANSACTION;\nINSERT INTO redemption_code \
             (id, code_hash, code_suffix, amount, batch, status, used_by, used_at, \
              expires_at, note, created_at) VALUES ",
        );
        let mut bs: Vec<Bind> = Vec::with_capacity(codes.len() * 6 + 1);
        let mut out = Vec::with_capacity(codes.len());
        for (i, c) in codes.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&format!(
                "($i{i}, $h{i}, $s{i}, $a{i}, $b{i}, 'unused', NONE, NONE, $e{i}, $n{i}, $now)"
            ));
            let id = ulid::Ulid::new().to_string();
            bs.push((
                "i".to_string() + &i.to_string(),
                RecordId::new("redemption_code", id.clone()).into_value(),
            ));
            bs.push((
                "h".to_string() + &i.to_string(),
                c.code_hash.clone().into_value(),
            ));
            bs.push((
                "s".to_string() + &i.to_string(),
                c.code_suffix.clone().into_value(),
            ));
            bs.push((
                "a".to_string() + &i.to_string(),
                c.amount_micro.into_value(),
            ));
            bs.push((
                "b".to_string() + &i.to_string(),
                c.batch.clone().into_value(),
            ));
            bs.push(("e".to_string() + &i.to_string(), c.expires_at.into_value()));
            bs.push((
                "n".to_string() + &i.to_string(),
                c.note.clone().into_value(),
            ));
            out.push(RedemptionCodeRow {
                id,
                code_suffix: c.code_suffix.clone(),
                amount_micro: c.amount_micro,
                batch: c.batch.clone(),
                status: "unused".to_string(),
                used_by: None,
                used_at: None,
                expires_at: c.expires_at,
                note: c.note.clone(),
                created_at: now,
            });
        }
        sql.push_str(";\nCOMMIT TRANSACTION;");
        bs.push(("now".to_string(), now.into_value()));
        self.run(&sql, &bs).await?;
        Ok(out)
    }

    async fn list_redemption_codes(
        &self,
        batch: &str,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<RedemptionCodeRow>), StoreError> {
        // expired 是显示态:库中 status 仍为 unused,按过期时刻判定
        // (与 SQLite 侧 list_redemption_codes 的 status_sql 同口径);
        // 生成层已枚举校验,未知 status 片段不追加条件(防御直调)。
        let mut cond = String::from("($batch = '' OR batch = $batch)");
        let mut bs = binds(vec![("batch", batch.to_string().into_value())]);
        match status.unwrap_or("") {
            "" => {}
            "expired" => cond.push_str(
                " AND status = 'unused' AND expires_at IS NOT NONE \
                 AND expires_at <= time::now()",
            ),
            st @ ("unused" | "used" | "disabled") => {
                cond.push_str(" AND status = $st");
                bs.push(("st".to_string(), st.to_string().into_value()));
            }
            _ => {}
        }
        let r#where = format!("WHERE {cond}");
        let mut res = self
            .run(
                &format!(
                    "SELECT count() AS c FROM redemption_code {W} GROUP ALL;\
                     \nSELECT id, code_suffix, amount, batch, status, \
                      used_by.username AS used_by, used_at, expires_at, note, created_at \
                      FROM redemption_code {W} \
                      ORDER BY created_at DESC, id DESC LIMIT {limit} START {offset};",
                    W = r#where
                ),
                &bs,
            )
            .await?;
        let counts: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        let rows: Vec<serde_json::Value> = res.take(1).map_err(failed)?;
        let out = rows
            .iter()
            .map(|r| {
                Ok(RedemptionCodeRow {
                    id: bare_id(&j_str(r, "id")),
                    code_suffix: j_str(r, "code_suffix"),
                    amount_micro: j_i64(r, "amount"),
                    batch: j_str(r, "batch"),
                    status: j_str(r, "status"),
                    used_by: j_opt_str(r, "used_by"),
                    used_at: j_opt_dt(r, "used_at")?,
                    expires_at: j_opt_dt(r, "expires_at")?,
                    note: j_str(r, "note"),
                    created_at: j_dt(r, "created_at")?,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok((count_of(&counts), out))
    }

    async fn disable_redemption_code(&self, id: &str) -> Result<bool, StoreError> {
        // DELETE 语义:unused → disabled;已用/不存在都不可禁(WHERE 覆盖,
        // UPDATE 返回 AFTER 镜像,命中与否以行数为准)。
        let mut res = self
            .run(
                "UPDATE redemption_code SET status = 'disabled' \
                 WHERE id = $rid AND status = 'unused'",
                &binds(vec![("rid", rid("redemption_code", id).into_value())]),
            )
            .await?;
        let updated: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        Ok(!updated.is_empty())
    }

    async fn redeem_code(
        &self,
        user_id: &str,
        code_hash: &str,
    ) -> Result<RedeemOutcome, StoreError> {
        // 红线 2:校验(unused/未过期)→ 标记 used → 余额入账 → 记
        // billing_transaction 全部在一个 BEGIN…COMMIT 块。并发兑换同一码:
        // UPDATE 的 WHERE 同时覆盖状态与过期校验,只有一个事务能把行
        // 置为 used;落败事务命中 0 行,IF 块整体跳过,不产生任何写入
        // (SQLite 侧"影响行数=0 即无效"的守卫同构)。
        // 语句序:0 BEGIN / 1 LET / 2 IF / 3 COMMIT / 4 SELECT 回读。
        let txid = RecordId::new("billing_transaction", ulid::Ulid::new().to_string());
        let urid = rid("user", user_id);
        let mut res = self
            .run(
                "BEGIN TRANSACTION;\
                 \nLET $hits = (UPDATE redemption_code SET status = 'used', \
                  used_by = $urid, used_at = time::now() \
                  WHERE code_hash = $h AND status = 'unused' \
                    AND (expires_at IS NONE OR expires_at > time::now()) RETURN AFTER);\
                 \nIF array::len($hits) > 0 {\
                    \nLET $amt = $hits[0].amount;\
                    \nLET $b = (UPDATE user SET balance = balance + $amt, \
                     updated_at = time::now() WHERE id = $urid RETURN AFTER);\
                    \nINSERT INTO billing_transaction \
                     (id, user, amount, balance_after, kind, note, created_at) \
                     VALUES ($txid, $urid, $amt, $b[0].balance, 'recharge', '兑换码充值', \
                     time::now());\
                 \n};\
                 \nCOMMIT TRANSACTION;\
                 \nSELECT amount, balance_after FROM billing_transaction WHERE id = $txid;",
                &binds(vec![
                    ("h", code_hash.to_string().into_value()),
                    ("urid", urid.into_value()),
                    ("txid", txid.into_value()),
                ]),
            )
            .await?;
        let row: Option<serde_json::Value> = res.take(4).map_err(failed)?;
        Ok(match row {
            // 无效/已用/已禁用/已过期不区分(防枚举),与 SQLite 侧同口径
            None => RedeemOutcome::Invalid,
            Some(r) => RedeemOutcome::Redeemed {
                amount_micro: j_i64(&r, "amount"),
                balance_after_micro: j_i64(&r, "balance_after"),
            },
        })
    }

    // ---- 亲和 ----

    async fn get_affinity(
        &self,
        api_key_id: &str,
        model: &str,
    ) -> Result<Option<String>, StoreError> {
        // 行不存在或渠道已删(悬空链接,join 探测列 name 为空)都返回
        // None——悬空亲和对选路自然失效,等价旧"残留行选路对不上候选"。
        let mut res = self
            .run(
                "SELECT provider.id AS pid, provider.name AS pname \
                  FROM channel_affinity WHERE id = $rid LIMIT 1",
                &binds(vec![(
                    "rid",
                    array_rid("channel_affinity", rid("api_key", api_key_id), model).into_value(),
                )]),
            )
            .await?;
        let row: Option<serde_json::Value> = res.take(0).map_err(failed)?;
        Ok(row
            .filter(|r| j_opt_str(r, "pname").is_some())
            .map(|r| bare_id(&j_str(&r, "pid"))))
    }

    async fn set_affinity(
        &self,
        api_key_id: &str,
        model: &str,
        provider_id: &str,
    ) -> Result<(), StoreError> {
        // 数组 id UPSERT 直位(等价旧 INSERT…ON CONFLICT(api_key_id, model))。
        self.run(
            "INSERT INTO channel_affinity (id, provider, updated_at) \
             VALUES ($rid, $p, time::now()) \
             ON DUPLICATE KEY UPDATE provider = $p, updated_at = time::now()",
            &binds(vec![
                (
                    "rid",
                    array_rid("channel_affinity", rid("api_key", api_key_id), model).into_value(),
                ),
                ("p", rid("provider", provider_id).into_value()),
            ]),
        )
        .await?;
        Ok(())
    }

    // ---- 统计 ----

    async fn usage_summary(
        &self,
        user_id: Option<&str>,
        since: Option<chrono::NaiveDate>,
        until: Option<chrono::NaiveDate>,
    ) -> Result<(i64, i64, i64), StoreError> {
        // usage_daily 聚合(requests/tokens/cost;cost 微元)。注:usage_daily
        // 只含已过保留期被清理的日期(明细仍在保留期内时以 request_log
        // 为准)——本口径按桩文件的 todo 定义,与 SQLite stats 适配器
        // (读明细)的差异记录在案,由集成批装配时裁决。
        let mut cond: Vec<&str> = Vec::new();
        let mut bs: Vec<Bind> = Vec::new();
        if let Some(u) = user_id {
            cond.push("user = $u");
            bs.extend(binds(vec![("u", rid("user", u).into_value())]));
        }
        if let Some(s) = since {
            cond.push("date >= $since");
            bs.extend(binds(vec![("since", date_str(s).into_value())]));
        }
        if let Some(t) = until {
            cond.push("date <= $until");
            bs.extend(binds(vec![("until", date_str(t).into_value())]));
        }
        let r#where = if cond.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", cond.join(" AND "))
        };
        let mut res = self
            .run(
                &format!(
                    "SELECT math::sum(requests) AS r, math::sum(tokens) AS t, \
                      math::sum(cost) AS c FROM usage_daily {W} GROUP ALL;",
                    W = r#where
                ),
                &bs,
            )
            .await?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        let row = rows.first().cloned().unwrap_or_default();
        Ok((j_i64(&row, "r"), j_i64(&row, "t"), j_i64(&row, "c")))
    }

    async fn system_stats(&self) -> Result<SystemStats, StoreError> {
        // 七个计数一次往返;语句序 0..=6 与字段一一对应。
        let mut res = self
            .run(
                "SELECT count() AS c FROM user GROUP ALL;\
                 \nSELECT count() AS c FROM user WHERE is_active = true GROUP ALL;\
                 \nSELECT count() AS c FROM api_key GROUP ALL;\
                 \nSELECT count() AS c FROM api_key WHERE is_active = true GROUP ALL;\
                 \nSELECT count() AS c FROM provider GROUP ALL;\
                 \nSELECT count() AS c FROM provider WHERE is_active = true GROUP ALL;\
                 \nSELECT count() AS c FROM redemption_code WHERE status = 'unused' GROUP ALL;",
                &[],
            )
            .await?;
        let mut counts = Vec::with_capacity(7);
        for i in 0..7 {
            let rows: Vec<serde_json::Value> = res.take(i).map_err(failed)?;
            counts.push(count_of(&rows));
        }
        Ok(SystemStats {
            users: counts[0],
            active_users: counts[1],
            api_keys: counts[2],
            active_api_keys: counts[3],
            providers: counts[4],
            active_providers: counts[5],
            unused_redemption_codes: counts[6],
        })
    }

    // ---- 集成批补充(默认切换装配面) ----

    async fn log_stats(&self, filters: &LogFilters) -> Result<LogStatsRaw, StoreError> {
        let (r#where, bs) = log_where(filters);
        let mut res = self
            .run(
                &format!(
                    "SELECT count() AS c, math::sum(total_tokens) AS t, \
                      math::mean(latency_ms) AS lat, count(IF success {{ 1 }} ELSE {{ 0 }}) AS s, \
                      math::sum(cost) AS cost \
                      FROM request_log {W} GROUP ALL;",
                    W = r#where
                ),
                &bs,
            )
            .await?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        let r = rows.first().cloned().unwrap_or_default();
        let total = j_i64(&r, "c");
        Ok(LogStatsRaw {
            total_requests: total,
            total_tokens: j_i64(&r, "t"),
            avg_latency_ms: j_f64(&r, "lat"),
            success_rate_pct: if total > 0 {
                j_i64(&r, "s") as f64 * 100.0 / total as f64
            } else {
                0.0
            },
            total_cost_micro: j_i64(&r, "cost"),
        })
    }

    async fn wallet_series(
        &self,
        user_id: &str,
        since_date: &str,
    ) -> Result<WalletSeries, StoreError> {
        // 两表按日合并(BTreeMap 累加;purge 同事务聚合+删除 ⇒ 相加不重复)。
        let mut res = self
            .run(
                "SELECT time::format(created_at, \"%Y-%m-%d\") AS d, \
                  math::sum(cost) AS c, math::sum(total_tokens) AS t, count() AS r \
                  FROM request_log WHERE user = $u \
                    AND time::format(created_at, \"%Y-%m-%d\") >= $since GROUP BY d;\
                 \nSELECT date AS d, math::sum(cost) AS c, math::sum(tokens) AS t, \
                  math::sum(requests) AS r FROM usage_daily WHERE user = $u AND date >= $since \
                  GROUP BY d;\
                 \nSELECT model, math::sum(cost) AS c, count() AS r FROM request_log \
                  WHERE user = $u AND time::format(created_at, \"%Y-%m-%d\") >= $since \
                  GROUP BY model ORDER BY c DESC LIMIT 8;",
                &binds(vec![
                    ("u", rid("user", user_id).into_value()),
                    ("since", since_date.to_string().into_value()),
                ]),
            )
            .await?;
        let detail: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        let agg: Vec<serde_json::Value> = res.take(1).map_err(failed)?;
        let models: Vec<serde_json::Value> = res.take(2).map_err(failed)?;
        let mut daily: std::collections::BTreeMap<String, (i64, i64, i64)> =
            std::collections::BTreeMap::new();
        for rows in [&detail, &agg] {
            for r in rows {
                let d = j_str(r, "d");
                if d.is_empty() {
                    continue;
                }
                let e = daily.entry(d).or_insert((0, 0, 0));
                e.0 += j_i64(r, "c");
                e.1 += j_i64(r, "t");
                e.2 += j_i64(r, "r");
            }
        }
        Ok(WalletSeries {
            daily: daily
                .into_iter()
                .map(|(d, (c, t, r))| (d, c, t, r))
                .collect(),
            top_models: models
                .iter()
                .map(|r| (j_str(r, "model"), j_i64(r, "c"), j_i64(r, "r")))
                .collect(),
        })
    }

    async fn usage_snapshot(&self) -> Result<UsageSnapshotRaw, StoreError> {
        // 近 30 天日序列一条查询;today/week/month 三组计数从日序列推导
        // (窗口分别是 d==today / d>=today-6 / 全序列,与旧 CASE WHEN 的
        // 分组口径逐值一致)。
        let today = chrono::Utc::now().date_naive();
        let fmt = |d: chrono::NaiveDate| d.format("%Y-%m-%d").to_string();
        let month_start = fmt(today - chrono::Duration::days(29));
        let week_start = fmt(today - chrono::Duration::days(6));
        let today_s = fmt(today);
        let mut res = self
            .run(
                "SELECT time::format(created_at, \"%Y-%m-%d\") AS d, count() AS r, \
                  math::sum(total_tokens) AS t, math::mean(latency_ms) AS lat, \
                  count(IF success { 1 } ELSE { 0 }) AS s \
                  FROM request_log \
                  WHERE time::format(created_at, \"%Y-%m-%d\") >= $since GROUP BY d \
                  ORDER BY d ASC;\
                 \nSELECT model, count() AS c, math::sum(total_tokens) AS t \
                  FROM request_log \
                  WHERE time::format(created_at, \"%Y-%m-%d\") >= $since \
                  GROUP BY model ORDER BY c DESC LIMIT 8;",
                &binds(vec![("since", month_start.into_value())]),
            )
            .await?;
        let daily_rows: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        let model_rows: Vec<serde_json::Value> = res.take(1).map_err(failed)?;

        let mut daily: Vec<(String, i64, i64, f64, f64)> = Vec::with_capacity(daily_rows.len());
        let (mut today_acc, mut week_acc, mut month_acc) =
            ((0i64, 0i64), (0i64, 0i64), (0i64, 0i64));
        for r in &daily_rows {
            let d = j_str(r, "d");
            let (req, tok) = (j_i64(r, "r"), j_i64(r, "t"));
            month_acc.0 += req;
            month_acc.1 += tok;
            if d >= week_start {
                week_acc.0 += req;
                week_acc.1 += tok;
            }
            if d == today_s {
                today_acc.0 += req;
                today_acc.1 += tok;
            }
            daily.push((
                d,
                req,
                tok,
                j_f64(r, "lat"),
                if req > 0 {
                    j_i64(r, "s") as f64 * 100.0 / req as f64
                } else {
                    0.0
                },
            ));
        }
        Ok(UsageSnapshotRaw {
            today: today_acc,
            week: week_acc,
            month: month_acc,
            daily,
            top_models: model_rows
                .iter()
                .map(|r| (j_str(r, "model"), j_i64(r, "c"), j_i64(r, "t")))
                .collect(),
        })
    }

    async fn requests_today(&self) -> Result<i64, StoreError> {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mut res = self
            .run(
                "SELECT count() AS c FROM request_log \
                  WHERE time::format(created_at, \"%Y-%m-%d\") = $d GROUP ALL;",
                &binds(vec![("d", today.into_value())]),
            )
            .await?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        Ok(count_of(&rows))
    }

    async fn list_price_records(&self) -> Result<Vec<PriceRecordRow>, StoreError> {
        let mut res = self
            .run(
                "SELECT id, model, prompt_price, completion_price, cached_price, \
                  created_at, updated_at FROM model_price ORDER BY model;",
                &[],
            )
            .await?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        rows.iter()
            .map(|r| {
                Ok(PriceRecordRow {
                    id: bare_id(&j_str(r, "id")),
                    model: j_str(r, "model"),
                    prompt_price: j_f64(r, "prompt_price"),
                    completion_price: j_f64(r, "completion_price"),
                    cached_price: r.get("cached_price").and_then(serde_json::Value::as_f64),
                    created_at: j_dt(r, "created_at")?,
                    updated_at: j_dt(r, "updated_at")?,
                })
            })
            .collect()
    }

    async fn insert_price_record(
        &self,
        model: &str,
        prompt_price: f64,
        completion_price: f64,
        cached_price: Option<f64>,
    ) -> Result<PriceRecordRow, StoreError> {
        let id = RecordId::new("model_price", ulid::Ulid::new().to_string());
        let mut res = self
            .run(
                "INSERT INTO model_price (id, model, prompt_price, completion_price, \
                  cached_price) VALUES ($rid, $m, $p, $c, $k) RETURN AFTER;",
                &binds(vec![
                    ("rid", id.into_value()),
                    ("m", model.to_string().into_value()),
                    ("p", prompt_price.into_value()),
                    ("c", completion_price.into_value()),
                    ("k", cached_price.into_value()),
                ]),
            )
            .await?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        rows.first()
            .map(|r| {
                Ok(PriceRecordRow {
                    id: bare_id(&j_str(r, "id")),
                    model: j_str(r, "model"),
                    prompt_price: j_f64(r, "prompt_price"),
                    completion_price: j_f64(r, "completion_price"),
                    cached_price: r.get("cached_price").and_then(serde_json::Value::as_f64),
                    created_at: j_dt(r, "created_at")?,
                    updated_at: j_dt(r, "updated_at")?,
                })
            })
            .transpose()?
            .ok_or_else(|| StoreError::Failed("insert_price_record 未回读到新行".to_string()))
    }

    async fn update_price_record(
        &self,
        id: &str,
        model: Option<&str>,
        prompt_price: Option<f64>,
        completion_price: Option<f64>,
        cached_price: Option<Option<f64>>,
    ) -> Result<Option<PriceRecordRow>, StoreError> {
        // 动态 SET(字段静态、值绑定);updated_at 必 bump(缓存令牌)。
        let mut sets: Vec<&str> = Vec::new();
        if model.is_some() {
            sets.push("model = $m");
        }
        if prompt_price.is_some() {
            sets.push("prompt_price = $p");
        }
        if completion_price.is_some() {
            sets.push("completion_price = $c");
        }
        if cached_price.is_some() {
            sets.push("cached_price = $k");
        }
        sets.push("updated_at = time::now()");
        let mut res = self
            .run(
                &format!(
                    "UPDATE model_price SET {} WHERE id = $rid RETURN AFTER;",
                    sets.join(", ")
                ),
                &binds(vec![
                    ("m", model.map(str::to_string).into_value()),
                    ("p", prompt_price.into_value()),
                    ("c", completion_price.into_value()),
                    ("k", cached_price.into_value()),
                    ("rid", rid("model_price", id).into_value()),
                ]),
            )
            .await?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        rows.first()
            .map(|r| {
                Ok(PriceRecordRow {
                    id: bare_id(&j_str(r, "id")),
                    model: j_str(r, "model"),
                    prompt_price: j_f64(r, "prompt_price"),
                    completion_price: j_f64(r, "completion_price"),
                    cached_price: r.get("cached_price").and_then(serde_json::Value::as_f64),
                    created_at: j_dt(r, "created_at")?,
                    updated_at: j_dt(r, "updated_at")?,
                })
            })
            .transpose()
    }

    async fn delete_price_record(&self, id: &str) -> Result<bool, StoreError> {
        let mut res = self
            .run(
                "DELETE model_price WHERE id = $rid RETURN BEFORE;",
                &binds(vec![("rid", rid("model_price", id).into_value())]),
            )
            .await?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        Ok(!rows.is_empty())
    }

    async fn list_transactions_scoped(
        &self,
        user_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<TxRecordFull>), StoreError> {
        let uid = user_id.unwrap_or("").trim();
        let (r#where, mut bs) = if uid.is_empty() {
            (String::new(), Vec::new())
        } else {
            (
                "WHERE user = $u ".to_string(),
                binds(vec![("u", rid("user", uid).into_value())]),
            )
        };
        let mut res = self
            .run(
                &format!(
                    "SELECT id, user.id AS user_id, user.username AS username, \
                      amount, balance_after, kind, note, created_at \
                      FROM billing_transaction {W}\
                      ORDER BY created_at DESC, id DESC LIMIT {limit} START {offset};\
                     \nSELECT count() AS c FROM billing_transaction {W}GROUP ALL;",
                    W = r#where
                ),
                &bs,
            )
            .await?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(failed)?;
        let counts: Vec<serde_json::Value> = res.take(1).map_err(failed)?;
        let out = rows
            .iter()
            .map(|r| {
                Ok(TxRecordFull {
                    id: bare_id(&j_str(r, "id")),
                    user_id: bare_id(&j_str(r, "user_id")),
                    username: j_opt_str(r, "username"),
                    amount_micro: j_i64(r, "amount"),
                    balance_after_micro: j_i64(r, "balance_after"),
                    kind: j_str(r, "kind"),
                    note: j_str(r, "note"),
                    created_at: j_dt(r, "created_at")?,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        bs.clear();
        Ok((count_of(&counts), out))
    }

    async fn get_redemption_code(&self, id: &str) -> Result<Option<RedemptionCodeRow>, StoreError> {
        let mut res = self
            .run(
                "SELECT id, code_suffix, amount, batch, status, \
                  used_by.username AS used_by, used_at, expires_at, note, created_at \
                  FROM redemption_code WHERE id = $rid LIMIT 1;",
                &binds(vec![("rid", rid("redemption_code", id).into_value())]),
            )
            .await?;
        let row: Option<serde_json::Value> = res.take(0).map_err(failed)?;
        row.map(|r| {
            Ok(RedemptionCodeRow {
                id: bare_id(&j_str(&r, "id")),
                code_suffix: j_str(&r, "code_suffix"),
                amount_micro: j_i64(&r, "amount"),
                batch: j_str(&r, "batch"),
                status: j_str(&r, "status"),
                used_by: j_opt_str(&r, "used_by"),
                used_at: j_opt_dt(&r, "used_at")?,
                expires_at: j_opt_dt(&r, "expires_at")?,
                note: j_str(&r, "note"),
                created_at: j_dt(&r, "created_at")?,
            })
        })
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 嵌入式引擎的查询执行在调用任务栈上内联递归,官方建议 ~10MiB 栈
    // (mod.rs 测试同款的大栈线程 + current-thread runtime)。
    fn on_big_stack<F>(name: &str, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .name(name.to_string())
            .spawn(f)
            .expect("spawn big-stack thread")
            .join()
            .expect("join test thread");
    }

    fn block_on<F>(name: &str, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        on_big_stack(name, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            rt.block_on(fut);
        });
    }

    /// 每个测试独享 ns/db,避免 mem:// 实例共享导致的数据串扰。
    async fn store(db: &str) -> SurrealBilling {
        let ep = crate::surreal::Endpoint::parse(&format!("mem://?ns=test&db={db}"))
            .expect("mem endpoint");
        let handle = crate::surreal::SurrealHandle::connect(&ep)
            .await
            .expect("connect mem://");
        handle.ensure_schema().await.expect("ensure schema");
        SurrealBilling::new(handle)
    }

    /// 直接写库的种子行(user/api_key/provider),供记账路径消费。
    async fn mk_user(s: &SurrealBilling, id: &str, balance_micro: i64) {
        s.run(
            "INSERT INTO user (id, username, password_hash, balance) \
             VALUES ($r, $n, 'x', $b)",
            &binds(vec![
                ("r", RecordId::new("user", id.to_string()).into_value()),
                ("n", format!("u-{id}").into_value()),
                ("b", balance_micro.into_value()),
            ]),
        )
        .await
        .expect("mk_user");
    }

    async fn mk_api_key(s: &SurrealBilling, id: &str, user: &str) {
        s.run(
            "INSERT INTO api_key (id, user, key, key_suffix) VALUES ($r, $u, $k, $s)",
            &binds(vec![
                ("r", RecordId::new("api_key", id.to_string()).into_value()),
                ("u", rid("user", user).into_value()),
                ("k", format!("hash-{id}").into_value()),
                ("s", format!("…{id}").into_value()),
            ]),
        )
        .await
        .expect("mk_api_key");
    }

    async fn mk_provider(s: &SurrealBilling, id: &str, name: &str) {
        s.run(
            "INSERT INTO provider (id, name, openai_base_url) VALUES ($r, $n, 'http://x')",
            &binds(vec![
                ("r", RecordId::new("provider", id.to_string()).into_value()),
                ("n", name.to_string().into_value()),
            ]),
        )
        .await
        .expect("mk_provider");
    }

    async fn user_balance(s: &SurrealBilling, id: &str) -> i64 {
        let mut res = s
            .run(
                "SELECT balance FROM user WHERE id = $r",
                &binds(vec![("r", rid("user", id).into_value())]),
            )
            .await
            .expect("read balance");
        let row: Option<serde_json::Value> = res.take(0).expect("take balance");
        row.map(|r| j_i64(&r, "balance")).unwrap_or(-1)
    }

    async fn count_rows(s: &SurrealBilling, table: &str) -> i64 {
        let mut res = s
            .run(&format!("SELECT count() AS c FROM {table} GROUP ALL"), &[])
            .await
            .expect("count");
        let rows: Vec<serde_json::Value> = res.take(0).expect("take count");
        count_of(&rows)
    }

    fn entry<'a>(
        user: &str,
        key: Option<&'a str>,
        prov: Option<&'a str>,
        cost: i64,
    ) -> RequestLedgerEntry {
        RequestLedgerEntry {
            request_id: None,
            user_id: user.to_string(),
            api_key_id: key.map(str::to_string),
            provider_id: prov.map(str::to_string),
            model: "gpt-test".to_string(),
            request_type: "chat".to_string(),
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_tokens: 10,
            latency_ms: 12,
            status_code: 200,
            success: true,
            error_message: None,
            cost_micro: cost,
        }
    }

    /// 9.6:request_id 落 request_log 并经 query_logs 读回(同一请求多渠道
    /// 重试链按它拼出);旧行/未带 id 的写入读回 None(option 语义)。
    #[test]
    fn surreal_request_id_round_trip() {
        block_on("reqid", async {
            let s = store("reqid").await;
            mk_user(&s, "u1", 1_000).await;
            mk_provider(&s, "p1", "prov-one").await;

            let mut e = entry("u1", None, Some("p1"), 0);
            e.request_id = Some("11111111-2222-3333-4444-555555555555".to_string());
            s.record_request(e).await.unwrap();
            let e2 = entry("u1", None, Some("p1"), 0);
            s.record_request(e2).await.unwrap(); // 无 request_id(历史形态)

            let (_, rows) = s.query_logs(&LogFilters::default(), 10, 0).await.unwrap();
            assert_eq!(rows.len(), 2);
            let with_id = rows
                .iter()
                .find(|r| r.request_id.as_deref() == Some("11111111-2222-3333-4444-555555555555"))
                .expect("row with request_id");
            assert_eq!(with_id.provider_name.as_deref(), Some("prov-one"));
            assert!(
                rows.iter().any(|r| r.request_id.is_none()),
                "无 id 的写入读回 None"
            );
        });
    }

    // ---- 价格表:CRUD + 令牌 + 播种 + 通配语义 ----

    #[test]
    fn surreal_price_seed_upsert_list_delete_token() {
        block_on("price", async {
            let s = store("price").await;
            // 空表播种:gpt-4o 刊例价 $2.5/$10 × 7.2 = 18/72,缓存 $1.25×7.2=9;
            // 兜底 * → 3/7,cached 为 NONE(与 pricing.rs 测试逐值同款)。
            let n = s.seed_default_prices_if_empty().await.unwrap();
            assert!(n > 100, "expected substantial seed, got {n}");
            let (_, rules) = s.list_prices(1000, 0).await.unwrap();
            let gpt4o = rules.iter().find(|r| r.model == "gpt-4o").unwrap();
            assert_eq!(
                (
                    gpt4o.prompt_price,
                    gpt4o.completion_price,
                    gpt4o.cached_price
                ),
                (18.0, 72.0, Some(9.0))
            );
            let fallback = rules.iter().find(|r| r.model == "*").unwrap();
            assert_eq!(fallback.cached_price, None);

            // 非空表:跳过,不覆盖手工数据
            s.upsert_price(PriceRule {
                model: "manual".into(),
                prompt_price: 1.5,
                completion_price: 2.5,
                cached_price: Some(0.5),
            })
            .await
            .unwrap();
            assert_eq!(s.seed_default_prices_if_empty().await.unwrap(), 0);

            // upsert:同 model 覆盖(不新增行),新 model 插入
            let total0 = s.list_prices(0, 0).await.unwrap().0;
            s.upsert_price(PriceRule {
                model: "manual".into(),
                prompt_price: 9.0,
                completion_price: 9.0,
                cached_price: None,
            })
            .await
            .unwrap();
            let (total1, page) = s.list_prices(1000, 0).await.unwrap();
            assert_eq!(total1, total0, "同 model upsert 不增行");
            let m = page.iter().find(|r| r.model == "manual").unwrap();
            assert_eq!((m.prompt_price, m.cached_price), (9.0, None));
            // 分页:ORDER BY model + LIMIT/START
            let (t, first) = s.list_prices(2, 0).await.unwrap();
            let (_, second) = s.list_prices(2, 2).await.unwrap();
            assert_eq!(t, total1);
            assert_ne!(first[0].model, second[0].model);
            assert!(first[0].model < second[0].model);

            // 令牌:行内容变化 ⇒ 变化;同内容稳定;删除也变化
            let t0 = s.price_table_token().await.unwrap();
            assert_eq!(t0, s.price_table_token().await.unwrap());
            s.upsert_price(PriceRule {
                model: "manual".into(),
                prompt_price: 9.5,
                completion_price: 9.0,
                cached_price: None,
            })
            .await
            .unwrap();
            assert_ne!(t0, s.price_table_token().await.unwrap());
            assert!(s.delete_price("manual").await.unwrap());
            assert!(!s.delete_price("manual").await.unwrap(), "重复删除");

            // 通配查询语义对齐现行为:price_rules() 输出喂 find_in_rules
            let rules = s.price_rules().await.unwrap();
            assert_eq!(
                crate::pricing::find_in_rules(&rules, "gpt-4o"),
                Some((18.0, 72.0, Some(9.0))),
                "精确命中优先"
            );
            assert_eq!(
                crate::pricing::find_in_rules(&rules, "totally-unknown-model")
                    .map(|(p, c, _)| (p, c)),
                Some((3.0, 7.0)),
                "* 兜底"
            );
        });
    }

    // ---- 记账热路径(红线 1) ----

    #[test]
    fn surreal_record_request_micro_and_counters() {
        block_on("record", async {
            let s = store("record").await;
            mk_user(&s, "u1", 10_000_000).await; // 10 元 = 1e7 微元
            mk_api_key(&s, "k1", "u1").await;
            mk_provider(&s, "p1", "prov-one").await;

            // cost>0 + 有 key:扣余额 + 计数器累加 + 日志落库,同一事务
            s.record_request(entry("u1", Some("k1"), Some("p1"), 123_456))
                .await
                .unwrap();
            assert_eq!(user_balance(&s, "u1").await, 10_000_000 - 123_456);
            let snap = s
                .key_limits_and_usage("k1", chrono::Utc::now().date_naive())
                .await
                .unwrap();
            assert_eq!(snap.tokens_spent, 150);
            // 计数器累加(不是覆盖)
            s.record_request(entry("u1", Some("k1"), Some("p1"), 1))
                .await
                .unwrap();
            assert_eq!(
                s.key_limits_and_usage("k1", chrono::Utc::now().date_naive())
                    .await
                    .unwrap()
                    .tokens_spent,
                300
            );
            assert_eq!(user_balance(&s, "u1").await, 10_000_000 - 123_457);

            // cost = 0:只记日志,不动余额
            s.record_request(entry("u1", Some("k1"), None, 0))
                .await
                .unwrap();
            assert_eq!(user_balance(&s, "u1").await, 10_000_000 - 123_457);

            // api_key = None:不计计数器
            let before = count_rows(&s, "api_key_usage_daily").await;
            s.record_request(entry("u1", None, None, 500))
                .await
                .unwrap();
            assert_eq!(count_rows(&s, "api_key_usage_daily").await, before);
            assert_eq!(user_balance(&s, "u1").await, 10_000_000 - 123_957);

            // 行不存在的 key:限额 0/用量 0(能力 trait 文档口径)
            let miss = s
                .key_limits_and_usage("nope", chrono::Utc::now().date_naive())
                .await
                .unwrap();
            assert_eq!(
                (
                    miss.rate_limit_rpm,
                    miss.quota_daily_tokens,
                    miss.max_concurrent,
                    miss.tokens_spent
                ),
                (0, 0, 0, 0)
            );

            // 重建:幂等,值 = 明细 SUM
            let n = s
                .rebuild_usage_counters(chrono::Utc::now().date_naive())
                .await
                .unwrap();
            assert_eq!(n, 1, "只重建有明细的 key(k1)");
            assert_eq!(
                s.key_limits_and_usage("k1", chrono::Utc::now().date_naive())
                    .await
                    .unwrap()
                    .tokens_spent,
                450 // 3 条带 key 的明细 × 150
            );
            s.rebuild_usage_counters(chrono::Utc::now().date_naive())
                .await
                .unwrap();
            assert_eq!(
                s.key_limits_and_usage("k1", chrono::Utc::now().date_naive())
                    .await
                    .unwrap()
                    .tokens_spent,
                450,
                "重建幂等"
            );

            assert_eq!(count_rows(&s, "request_log").await, 4);
        });
    }

    /// 单查询 BEGIN…COMMIT 的回滚语义(红线 1 的机制前提):事务中途
    /// 语句失败,先执行的语句不落库。
    #[test]
    fn surreal_single_query_transaction_rolls_back() {
        block_on("rollback", async {
            let s = store("rollback").await;
            mk_user(&s, "u1", 1_000).await;
            let err = s
                .run(
                    "BEGIN TRANSACTION;\
                     \nUPDATE user SET balance = 999 WHERE id = $r;\
                     \nINSERT INTO model_price (id, model) VALUES ($x, 'dup');\
                     \nINSERT INTO model_price (id, model) VALUES ($x, 'dup');\
                     \nCOMMIT TRANSACTION;",
                    &binds(vec![
                        ("r", rid("user", "u1").into_value()),
                        (
                            "x",
                            RecordId::new("model_price", RecordIdKey::from("same")).into_value(),
                        ),
                    ]),
                )
                .await;
            assert!(err.is_err(), "第二条违反 unique_model_price_model,必须报错");
            assert_eq!(
                user_balance(&s, "u1").await,
                1_000,
                "先执行的 UPDATE 已回滚"
            );
            assert_eq!(count_rows(&s, "model_price").await, 0);
        });
    }

    // ---- 日志查询 + dangling 降级 ----

    #[test]
    fn surreal_query_logs_filters_and_dangling() {
        block_on("logs", async {
            let s = store("logs").await;
            mk_user(&s, "u1", 0).await;
            mk_user(&s, "u2", 0).await;
            mk_api_key(&s, "k1", "u1").await;
            mk_provider(&s, "p1", "prov-one").await;
            s.record_request(entry("u1", Some("k1"), Some("p1"), 10))
                .await
                .unwrap();
            let mut fail = entry("u1", None, None, 0);
            fail.success = false;
            fail.status_code = 500;
            fail.error_message = Some("boom".into());
            fail.model = "other-model".into();
            s.record_request(fail).await.unwrap();
            s.record_request(entry("u2", None, None, 5)).await.unwrap();

            let (total, rows) = s.query_logs(&LogFilters::default(), 10, 0).await.unwrap();
            assert_eq!(total, 3);
            let r0 = &rows[0]; // ORDER BY created_at DESC:同秒内按 id DESC,断言集合而非顺序
            assert!(rows.iter().all(|r| r.id.len() > 4));
            assert!(r0.created_at <= chrono::Utc::now());

            // 过滤:user / model / success
            let f = LogFilters {
                user_id: Some("u1".into()),
                ..Default::default()
            };
            assert_eq!(s.query_logs(&f, 10, 0).await.unwrap().0, 2);
            let f = LogFilters {
                model: Some("other-model".into()),
                ..Default::default()
            };
            let (_, rows) = s.query_logs(&f, 10, 0).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert!(!rows[0].success);
            assert_eq!(rows[0].error_message.as_deref(), Some("boom"));
            assert_eq!(rows[0].status_code, 500);
            let f = LogFilters {
                success: Some(true),
                ..Default::default()
            };
            assert_eq!(s.query_logs(&f, 10, 0).await.unwrap().0, 2);
            // since/ununtil 闭区间:未来时刻 → 空
            let f = LogFilters {
                since: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                ..Default::default()
            };
            assert_eq!(s.query_logs(&f, 10, 0).await.unwrap().0, 0);
            let f = LogFilters {
                api_key_id: Some("k1".into()),
                ..Default::default()
            };
            assert_eq!(s.query_logs(&f, 10, 0).await.unwrap().0, 1);

            // 关联解析:u1+p1 的行带 username/provider_name;id 均为裸 key
            let f = LogFilters {
                provider_id: Some("p1".into()),
                ..Default::default()
            };
            let (_, rows) = s.query_logs(&f, 10, 0).await.unwrap();
            assert_eq!(rows.len(), 1);
            let r = &rows[0];
            assert_eq!(r.username.as_deref(), Some("u-u1"));
            assert_eq!(r.user_id.as_deref(), Some("u1"));
            assert_eq!(r.provider_name.as_deref(), Some("prov-one"));
            assert_eq!(r.provider_id.as_deref(), Some("p1"));
            assert_eq!(r.api_key_id.as_deref(), Some("k1"));
            assert_eq!(r.cost_micro, 10);

            // dangling:删 provider 后,provider_name 与 provider_id 都降级 None
            s.run("DELETE provider", &[]).await.unwrap();
            let (_, rows) = s.query_logs(&f, 10, 0).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert!(rows[0].provider_name.is_none());
            assert!(
                rows[0].provider_id.is_none(),
                "id 一并降级(等价 ON DELETE SET NULL)"
            );
            // 删 api_key 同理
            s.run("DELETE api_key", &[]).await.unwrap();
            let f = LogFilters {
                user_id: Some("u1".into()),
                success: Some(true),
                ..Default::default()
            };
            let (_, rows) = s.query_logs(&f, 10, 0).await.unwrap();
            assert!(rows[0].api_key_id.is_none());
        });
    }

    // ---- 清理:聚合进 usage_daily 再删(对账依据) ----

    #[test]
    fn surreal_purge_aggregates_then_deletes() {
        block_on("purge", async {
            let s = store("purge").await;
            mk_user(&s, "u1", 0).await;
            mk_api_key(&s, "k1", "u1").await;

            // 手写两条旧日志(跨两天)+ 一条保留期内的(记账路径总是 now)
            let old = |i: usize, days_ago: i64, cost: i64, tokens: i64| -> String {
                format!(
                    "(type::record('request_log', 'old-{days_ago}-{cost}'), $u, NONE, NONE, \
                     'm', 'chat', 0, 0, {tokens}, 0, 1, 200, true, NONE, {cost}, $t{i})"
                )
            };
            s.run(
                &format!(
                    "INSERT INTO request_log (id, user, api_key, provider, model, request_type, \
                      prompt_tokens, completion_tokens, total_tokens, cached_tokens, latency_ms, \
                      status_code, success, error_message, cost, created_at) VALUES \
                      {}, {}, (type::record('request_log', 'fresh'), $u, $k, NONE, \
                     'm', 'chat', 0, 0, 7, 0, 1, 200, true, NONE, 7, time::now());",
                    old(0, 40, 10_000, 100),
                    old(1, 39, 20_000, 200)
                ),
                &binds(vec![
                    ("u", rid("user", "u1").into_value()),
                    ("k", rid("api_key", "k1").into_value()),
                    (
                        "t0",
                        (chrono::Utc::now() - chrono::Duration::days(40)).into_value(),
                    ),
                    (
                        "t1",
                        (chrono::Utc::now() - chrono::Duration::days(39)).into_value(),
                    ),
                ]),
            )
            .await
            .unwrap();

            let deleted = s.purge_expired_logs(30).await.unwrap();
            assert_eq!(deleted, 2);
            assert_eq!(count_rows(&s, "request_log").await, 1, "保留期内明细保留");

            // 对账依据:(user, date) 聚合,微元直搬
            let mut res = s
                .run(
                    "SELECT requests, tokens, cost FROM usage_daily \
                     WHERE user = $u ORDER BY date",
                    &binds(vec![("u", rid("user", "u1").into_value())]),
                )
                .await
                .unwrap();
            let rows: Vec<serde_json::Value> = res.take(0).unwrap();
            assert_eq!(rows.len(), 2, "两天各一行");
            let (r0, r1) = (j_i64(&rows[0], "requests"), j_i64(&rows[1], "requests"));
            assert_eq!((r0, r1), (1, 1));
            let total_cost: i64 = rows.iter().map(|r| j_i64(r, "cost")).sum();
            assert_eq!(total_cost, 30_000, "微元直搬不乘不除");

            // 幂等:明细已删,重跑不重复累加
            assert_eq!(s.purge_expired_logs(30).await.unwrap(), 0);
            let mut res = s
                .run(
                    "SELECT math::sum(cost) AS c FROM usage_daily GROUP ALL",
                    &[],
                )
                .await
                .unwrap();
            let rows: Vec<serde_json::Value> = res.take(0).unwrap();
            assert_eq!(j_i64(&rows[0], "c"), 30_000);

            // usage_summary:聚合口径(含日期过滤)
            let (r, t, c) = s.usage_summary(None, None, None).await.unwrap();
            assert_eq!((r, t, c), (2, 300, 30_000));
            let (r, _, _) = s
                .usage_summary(Some("u1"), Some(chrono::Utc::now().date_naive()), None)
                .await
                .unwrap();
            assert_eq!(r, 0, "旧日期不在今日之后");
        });
    }

    // ---- 调账 + 流水 ----

    #[test]
    fn surreal_adjust_balance_and_transactions() {
        block_on("adjust", async {
            let s = store("adjust").await;
            mk_user(&s, "u1", 5_000).await;

            assert_eq!(
                s.adjust_balance("u1", 1_234_567, "recharge", "充值10元")
                    .await
                    .unwrap(),
                1_239_567
            );
            assert_eq!(
                s.adjust_balance("u1", -39_567, "adjust", "纠错")
                    .await
                    .unwrap(),
                1_200_000
            );
            assert_eq!(user_balance(&s, "u1").await, 1_200_000);

            // 不存在的用户:报错,且不产生流水
            assert!(s.adjust_balance("ghost", 1, "recharge", "").await.is_err());
            assert_eq!(count_rows(&s, "billing_transaction").await, 2);

            let (total, rows) = s.list_transactions("u1", 10, 0).await.unwrap();
            assert_eq!(total, 2);
            assert_eq!(
                rows[0].amount_micro, -39_567,
                "ORDER BY created_at DESC(最新在前)"
            );
            assert_eq!(rows[0].balance_after_micro, 1_200_000);
            assert_eq!(rows[0].user_id, "u1");
            assert_eq!(rows[0].kind, "adjust");
            assert_eq!(rows[1].note, "充值10元");
        });
    }

    // ---- 兑换码(红线 2) ----

    #[test]
    fn surreal_redemption_create_list_disable_redeem() {
        block_on("redeem", async {
            let s = store("redeem").await;
            mk_user(&s, "u1", 1_000).await;

            let codes = vec![
                NewRedemptionCode {
                    code_hash: "h-AAA".into(),
                    code_suffix: "…AAA".into(),
                    amount_micro: 5_000_000,
                    batch: "b1".into(),
                    expires_at: None,
                    note: String::new(),
                },
                NewRedemptionCode {
                    code_hash: "h-EXP".into(),
                    code_suffix: "…EXP".into(),
                    amount_micro: 7_000_000,
                    batch: "b1".into(),
                    expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
                    note: "已过期".into(),
                },
                NewRedemptionCode {
                    code_hash: "h-BBB".into(),
                    code_suffix: "…BBB".into(),
                    amount_micro: 9_000_000,
                    batch: "b2".into(),
                    expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                    note: String::new(),
                },
            ];
            let rows = s.create_redemption_codes(codes).await.unwrap();
            assert_eq!(rows.len(), 3);
            assert!(rows.iter().all(|r| r.status == "unused"));

            // 列表:batch 过滤 + status 过滤(expired 是显示态)
            let (t, _) = s.list_redemption_codes("", None, 10, 0).await.unwrap();
            assert_eq!(t, 3);
            let (t, _) = s.list_redemption_codes("b1", None, 10, 0).await.unwrap();
            assert_eq!(t, 2);
            let (t, expired) = s
                .list_redemption_codes("", Some("expired"), 10, 0)
                .await
                .unwrap();
            assert_eq!(t, 1);
            assert_eq!(expired[0].code_suffix, "…EXP");

            // 核销成功:余额入账 + 流水 + 状态 used,同一事务
            let out = s.redeem_code("u1", "h-AAA").await.unwrap();
            assert_eq!(
                out,
                RedeemOutcome::Redeemed {
                    amount_micro: 5_000_000,
                    balance_after_micro: 5_001_000
                }
            );
            assert_eq!(user_balance(&s, "u1").await, 5_001_000);
            let (t, rows) = s.list_transactions("u1", 10, 0).await.unwrap();
            assert_eq!(t, 1);
            assert_eq!(rows[0].kind, "recharge");
            let (_, rows) = s
                .list_redemption_codes("b1", Some("used"), 10, 0)
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].used_by.as_deref(),
                Some("u-u1"),
                "used_by 显示用户名"
            );

            // 同码二次兑换:Invalid,余额与流水都不动
            assert_eq!(
                s.redeem_code("u1", "h-AAA").await.unwrap(),
                RedeemOutcome::Invalid
            );
            assert_eq!(user_balance(&s, "u1").await, 5_001_000);
            assert_eq!(count_rows(&s, "billing_transaction").await, 1);

            // 过期 / 不存在 / 已禁用:同为 Invalid(防枚举)
            assert_eq!(
                s.redeem_code("u1", "h-EXP").await.unwrap(),
                RedeemOutcome::Invalid
            );
            assert_eq!(
                s.redeem_code("u1", "h-NOPE").await.unwrap(),
                RedeemOutcome::Invalid
            );
            let (_, b2) = s.list_redemption_codes("b2", None, 10, 0).await.unwrap();
            assert!(s.disable_redemption_code(&b2[0].id).await.unwrap());
            assert_eq!(
                s.redeem_code("u1", "h-BBB").await.unwrap(),
                RedeemOutcome::Invalid
            );
            assert_eq!(user_balance(&s, "u1").await, 5_001_000);
            assert_eq!(
                count_rows(&s, "billing_transaction").await,
                1,
                "失败核销不产生流水"
            );

            // 已用码不可禁
            let (_, used) = s
                .list_redemption_codes("b1", Some("used"), 10, 0)
                .await
                .unwrap();
            assert!(!s.disable_redemption_code(&used[0].id).await.unwrap());
        });
    }

    // ---- 渠道亲和 ----

    #[test]
    fn surreal_affinity_upsert_and_dangling() {
        block_on("affinity", async {
            let s = store("affinity").await;
            mk_api_key(&s, "k1", "ignored").await;
            mk_provider(&s, "p1", "prov-one").await;
            mk_provider(&s, "p2", "prov-two").await;

            assert_eq!(s.get_affinity("k1", "m1").await.unwrap(), None);
            s.set_affinity("k1", "m1", "p1").await.unwrap();
            assert_eq!(
                s.get_affinity("k1", "m1").await.unwrap().as_deref(),
                Some("p1")
            );
            // UPSERT:同 (key, model) 换渠道不增行
            s.set_affinity("k1", "m1", "p2").await.unwrap();
            assert_eq!(
                s.get_affinity("k1", "m1").await.unwrap().as_deref(),
                Some("p2")
            );
            assert_eq!(count_rows(&s, "channel_affinity").await, 1);
            // 不同 model 独立
            s.set_affinity("k1", "m2", "p1").await.unwrap();
            assert_eq!(
                s.get_affinity("k1", "m2").await.unwrap().as_deref(),
                Some("p1")
            );
            assert_eq!(count_rows(&s, "channel_affinity").await, 2);

            // 悬空:渠道删除后亲和失效(读侧 None)
            s.run("DELETE provider", &[]).await.unwrap();
            assert_eq!(s.get_affinity("k1", "m1").await.unwrap(), None);
        });
    }

    // ---- 系统概况 ----

    /// SDK 惯用法回归闸:本实现的全部关键 SurrealQL 形态(多行 VALUES /
    /// LET 单等号 / `ON DUPLICATE … $input.x` / `DELETE … IN … RETURN
    /// BEFORE` / IF 块)在 mem:// 上逐个钉死——换 SDK 版本时这里是第一道
    /// 破坏面检测。
    #[test]
    fn surrealql_idioms_regression() {
        block_on("probe", async {
            let s = store("probe").await;

            // 多行 VALUES(参数 + NONE 字面量)逐行落库
            s.run(
                "INSERT INTO model_price (id, model, cached_price) \
                 VALUES ($a, 'm1', 1.0), ($b, 'm2', NONE), ($c, 'm3', NONE)",
                &binds(vec![
                    ("a", rid("model_price", "pa").into_value()),
                    ("b", rid("model_price", "pb").into_value()),
                    ("c", rid("model_price", "pc").into_value()),
                ]),
            )
            .await
            .unwrap();
            assert_eq!(count_rows(&s, "model_price").await, 3);

            // LET = (单等号)可用;`:=` 不可(解析错误)
            s.run("LET $e = (SELECT VALUE id FROM model_price LIMIT 1)", &[])
                .await
                .unwrap();
            assert!(
                s.run("LET $e := (SELECT VALUE id FROM model_price LIMIT 1)", &[])
                    .await
                    .is_err()
            );

            // ON DUPLICATE:$input.<字段> 是本行待插入值(等价 excluded)
            mk_user(&s, "pu", 0).await;
            mk_api_key(&s, "pk", "pu").await;
            let cid = array_rid("api_key_usage_daily", rid("api_key", "pk"), "2020-01-01");
            for add in [5i64, 7] {
                s.run(
                    "INSERT INTO api_key_usage_daily (id, api_key, date, tokens_spent) \
                     VALUES ($c, $k, '2020-01-01', $n) \
                     ON DUPLICATE KEY UPDATE tokens_spent = tokens_spent + $input.tokens_spent",
                    &binds(vec![
                        ("c", cid.clone().into_value()),
                        ("k", rid("api_key", "pk").into_value()),
                        ("n", add.into_value()),
                    ]),
                )
                .await
                .unwrap();
            }
            let mut res = s
                .run("SELECT tokens_spent FROM api_key_usage_daily", &[])
                .await
                .unwrap();
            let rows: Vec<serde_json::Value> = res.take(0).unwrap();
            assert_eq!(j_i64(&rows[0], "tokens_spent"), 12, "5 + 7(累加非覆盖)");

            // DELETE:IN $ids 匹配 + RETURN BEFORE 回被删行(默认不回行)
            let ids = vec![rid("model_price", "pa"), rid("model_price", "pb")];
            let mut res = s
                .run(
                    "DELETE model_price WHERE id IN $ids RETURN BEFORE",
                    &binds(vec![("ids", ids.into_value())]),
                )
                .await
                .unwrap();
            let deleted: Vec<serde_json::Value> = res.take(0).unwrap();
            assert_eq!(deleted.len(), 2);
            assert_eq!(count_rows(&s, "model_price").await, 1);

            // IF/ELSE 块(嵌套 LET + 语句)可执行
            s.run("IF 1 > 0 { LET $x = 5; } ELSE { LET $x = 6; };", &[])
                .await
                .unwrap();
        });
    }

    #[test]
    fn surreal_system_stats_counts() {
        block_on("stats", async {
            let s = store("stats").await;
            mk_user(&s, "u1", 0).await;
            mk_user(&s, "u2", 0).await;
            s.run(
                "UPDATE user SET is_active = false WHERE id = $r",
                &binds(vec![("r", rid("user", "u2").into_value())]),
            )
            .await
            .unwrap();
            mk_api_key(&s, "k1", "u1").await;
            mk_provider(&s, "p1", "prov").await;
            s.create_redemption_codes(vec![NewRedemptionCode {
                code_hash: "h-X".into(),
                code_suffix: "…X".into(),
                amount_micro: 1,
                batch: String::new(),
                expires_at: None,
                note: String::new(),
            }])
            .await
            .unwrap();
            let st = s.system_stats().await.unwrap();
            assert_eq!(st.users, 2);
            assert_eq!(st.active_users, 1);
            assert_eq!(st.api_keys, 1);
            assert_eq!(st.active_api_keys, 1);
            assert_eq!(st.providers, 1);
            assert_eq!(st.active_providers, 1);
            assert_eq!(st.unused_redemption_codes, 1);
        });
    }

    // ---- 集成批补充方法的库内行为(口径 = SQLite 侧对应 SQL) ----

    /// 统计聚合 / 钱包序列 / 仪表盘快照 / 价格记录 CRUD / 流水分页 /
    /// 兑换码点查:钉死新引入的 SurrealQL 惯用法(`<int>` 布尔转整数求和、
    /// `time::format` 按日分组、链接投影 username)。
    #[test]
    fn surreal_integration_aggregates_and_price_records() {
        block_on("integ", async {
            let s = store("integ").await;
            mk_user(&s, "u1", 5_000).await;
            mk_api_key(&s, "k1", "u1").await;

            // 两笔成功 + 一笔失败(今日),不同模型
            s.record_request(entry("u1", Some("k1"), None, 10))
                .await
                .unwrap();
            let mut m2 = entry("u1", Some("k1"), None, 20);
            m2.model = "other".into();
            s.record_request(m2).await.unwrap();
            let mut fail = entry("u1", None, None, 0);
            fail.success = false;
            s.record_request(fail).await.unwrap();

            // log_stats:全量口径(布尔计数用 count(IF…) 惯用法,实测
            // math::sum(bool) 恒 0、<int> 转换报错)
            let st = s.log_stats(&LogFilters::default()).await.unwrap();
            assert_eq!(st.total_requests, 3);
            assert_eq!(st.total_tokens, 450);
            assert_eq!(st.total_cost_micro, 30);
            assert_eq!(st.success_rate_pct, 200.0 / 3.0);
            assert!(st.avg_latency_ms >= 0.0);

            // usage_snapshot:today/week/month 三组从日序列推导
            let snap = s.usage_snapshot().await.unwrap();
            assert_eq!(snap.today, (3, 450));
            assert_eq!(snap.week, (3, 450));
            assert_eq!(snap.month, (3, 450));
            assert_eq!(snap.daily.len(), 1);
            assert_eq!(snap.daily[0].1, 3);
            assert!((snap.daily[0].4 - 200.0 / 3.0).abs() < 1e-9);
            assert_eq!(snap.top_models.len(), 2);
            assert_eq!(BillingStore::requests_today(&s).await.unwrap(), 3);

            // wallet_series:今日窗口含明细;top_models 按费用降序
            let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
            let w = s.wallet_series("u1", &today).await.unwrap();
            assert_eq!(w.daily.len(), 1);
            assert_eq!(w.daily[0], (today.clone(), 30, 450, 3));
            assert_eq!(w.top_models.len(), 2);
            // 费用降序:other(20)> gpt-test(10)
            assert_eq!(w.top_models[0].0, "other");

            // 价格记录 CRUD:插入 → 列表 → 按 id 更新 → 删除
            let rec = s
                .insert_price_record("m1", 1.5, 2.5, Some(0.5))
                .await
                .unwrap();
            assert_eq!(rec.model, "m1");
            assert!(!rec.id.is_empty());
            let err = s
                .insert_price_record("m1", 1.0, 1.0, None)
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("unique_model_price_model"),
                "unique violation surfaced: {err}"
            );
            let updated = s
                .update_price_record(&rec.id, Some("m1x"), Some(9.0), None, Some(None))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(updated.model, "m1x");
            assert_eq!(updated.prompt_price, 9.0);
            assert_eq!(updated.cached_price, None, "Some(None) 清回 NONE");
            let list = s.list_price_records().await.unwrap();
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].model, "m1x");
            assert!(s.delete_price_record(&rec.id).await.unwrap());
            assert!(!s.delete_price_record(&rec.id).await.unwrap());
            assert!(
                s.update_price_record(&rec.id, Some("zz"), None, None, None)
                    .await
                    .unwrap()
                    .is_none(),
                "未命中 → None"
            );

            // 流水分页(带用户名)+ 兑换码点查
            s.adjust_balance("u1", 100, "recharge", "r").await.unwrap();
            let (t, txs) = s.list_transactions_scoped(None, 10, 0).await.unwrap();
            assert_eq!(t, 1);
            assert_eq!(txs[0].username.as_deref(), Some("u-u1"));
            assert_eq!(txs[0].amount_micro, 100);
            let (t, _) = s.list_transactions_scoped(Some("u1"), 10, 0).await.unwrap();
            assert_eq!(t, 1);
            let codes = s
                .create_redemption_codes(vec![NewRedemptionCode {
                    code_hash: "h-1".into(),
                    code_suffix: "…1".into(),
                    amount_micro: 7,
                    batch: "b".into(),
                    expires_at: None,
                    note: String::new(),
                }])
                .await
                .unwrap();
            let got = s.get_redemption_code(&codes[0].id).await.unwrap().unwrap();
            assert_eq!(got.amount_micro, 7);
            assert_eq!(got.status, "unused");
            assert!(s.get_redemption_code("nope").await.unwrap().is_none());
        });
    }
}
