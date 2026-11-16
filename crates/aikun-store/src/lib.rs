//! # aikun-store:仓储层(能力 trait + SQLite 实现)
//!
//! plan 步骤 8 / roadmap 阶段 3 的落点:吸收二进制的 `src/db` 与
//! `src/models`(连接池、schema 迁移、行映射、静态加密、口令哈希),
//! 并按**能力**暴露存储 trait。
//!
//! ## 纪律:trait 签名零 SQL 方言
//!
//! 阶段 S 的 SurrealDB 第二实现只依赖这些签名,不依赖任何 rusqlite
//! 类型。因此 trait 里严禁出现:
//!
//! - `&rusqlite::Connection` / `Transaction` 等驱动类型;
//! - `?N` 占位或 SQL 片段参数;
//! - `ON CONFLICT` / `datetime('now')` / `substr(...)` 等 SQL 语义
//!   (日期一律 `chrono::NaiveDate`,幂等累加一律叫 `record_*`/`rebuild_*`);
//! - `PRAGMA`(版本闸门是 SQLite impl 的内部细节,不进签名)。
//!
//! 方言只允许出现在 `pool.rs` / `schema.rs` 等 SQLite impl 文件里。
//!
//! ## 当前吸收状态(逐步吸收,architecture §四)
//!
//! | 来源 | 去向 |
//! |---|---|
//! | `aikun/src/db/mod.rs`(池/迁移/种子) | `pool.rs` + `schema.rs` |
//! | `aikun/src/models/provider_row.rs` | `provider_row.rs` |
//! | `aikun/src/crypto.rs`(静态加密) | `crypto.rs` |
//! | `aikun/src/auth` 的口令/密钥哈希四函数 | `password.rs` |
//! | `aikun/src/billing` 的默认价格播种 | `pricing.rs`(db 层不再反向调用 billing) |
//!
//! 记账事务本体(`billing/ledger.rs`)仍留在二进制(红线:计费事务不改),
//! 后续批次再收编为 `UsageLedger` 能力。

pub mod crypto;
pub mod password;
pub mod pool;
pub mod pricing;
pub mod provider_row;
pub mod schema;

use aikun_channels::Provider;

// ---------- 错误 ----------

/// 存储错误。`Unavailable` = 存储不可用(如连接锁中毒);
/// `Failed` 携带实现侧细节,只进日志,不进对外错误体。
#[derive(Debug)]
pub enum StoreError {
    Unavailable,
    Failed(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Unavailable => write!(f, "storage unavailable"),
            StoreError::Failed(msg) => write!(f, "storage failure: {msg}"),
        }
    }
}

impl std::error::Error for StoreError {}

// ---------- 领域行模型 ----------

/// 价格条目(每 1M tokens 的价格,元;`cached_price` 为 None 表示缓存
/// token 按输入价计)。匹配规则(精确优先 → 最长 `*` 前缀 → 单独的 `*`
/// 兜底)由 [`pricing::find_in_rules`] 提供,口径与 SQLite 侧
/// `billing::find_price` 逐值一致(见二进制 billing 的对照测试)。
#[derive(Debug, Clone, PartialEq)]
pub struct PriceRule {
    pub model: String,
    pub prompt_price: f64,
    pub completion_price: f64,
    pub cached_price: Option<f64>,
}

/// API key 限额与当日已用 token(计数器表口径,替代 request_logs SUM)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyUsageSnapshot {
    pub rate_limit_rpm: i64,
    pub quota_daily_tokens: i64,
    pub max_concurrent: i64,
    /// 当日(`date` 参数)已消耗的 total_tokens 合计
    pub tokens_spent: i64,
}

// ---------- 能力 trait(无方言) ----------

/// 渠道拓扑读取:选路快照缓存(`channel/registry.rs`)的数据源。
/// 返回的 [`Provider::api_key`] 保持**库内形态**(密文),由调用方在
/// 选中后解密一次——快照与解密职责分离(roadmap 决策 4)。
pub trait ChannelTopology: Send + Sync {
    /// 全部启用渠道(is_active = 1),含健康度/EMA 字段的库内现值。
    fn active_channels(&self) -> Result<Vec<Provider>, StoreError>;
}

/// 价格表读取与首启播种。计费热路径经进程内缓存消费,CRUD 失效后
/// 重新拉全量。
pub trait PriceBook: Send + Sync {
    /// 全量价格条目(顺序不限,匹配在内存做)。
    fn price_rules(&self) -> Result<Vec<PriceRule>, StoreError>;

    /// 价格表为空时导入内置默认价格(LiteLLM 刊例价快照),返回导入
    /// 条数;非空返回 0(不覆盖手工维护的数据)。
    fn seed_default_prices_if_empty(&self) -> Result<usize, StoreError>;
}

/// API key 限额与日用量计数器。计数器与记账同事务累加(见二进制
/// `billing/ledger.rs`),并可在任意时刻从 request_logs 明细**重建**
/// (refactor-plan 9.3:明细是唯一真相,计数器是加速器)。
pub trait KeyUsage: Send + Sync {
    /// 单把 key 的三项限额 + `date` 当日已用 tokens。
    /// 行不存在时限额按 0/用量按 0 返回(与旧口径一致)。
    fn key_limits_and_usage(
        &self,
        api_key_id: &str,
        date: chrono::NaiveDate,
    ) -> Result<KeyUsageSnapshot, StoreError>;

    /// 从 request_logs 明细重建 `date` 当日全部计数器(幂等)。
    /// 返回重建的 key 数。
    fn rebuild_usage_counters(&self, date: chrono::NaiveDate) -> Result<usize, StoreError>;
}

/// 缓存一致性令牌:进程内缓存(渠道拓扑 / 价格表)在每次取用时比对,
/// 令牌变化才回源重载。契约是**尽力而为的单调性**:行内容变化 ⇒ 令牌
/// 大概率变化;允许同内容令牌变化(只是多一次重载)。SQLite 实现用
/// `(行数, 最大 rowid, 最大 updated_at)` 聚合;第二实现可用表版本、
/// 变更流或始终递增的序号。
pub trait CacheTokens: Send + Sync {
    /// providers 表令牌(渠道拓扑缓存)。
    fn channel_topology_token(&self) -> Result<CacheToken, StoreError>;
    /// model_prices 表令牌(价格表缓存)。
    fn price_table_token(&self) -> Result<CacheToken, StoreError>;
}

/// 不透明令牌:仅要求 `PartialEq`,调用方不做任何解释。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheToken(String);

impl CacheToken {
    /// 实现侧构造(调用方不构造令牌)。
    pub fn new(stamp: String) -> Self {
        CacheToken(stamp)
    }
}
