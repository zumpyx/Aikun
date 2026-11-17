//! # aikun-store:仓储层(能力 trait + SurrealDB 实现)
//!
//! 阶段 S 默认切换(docs/surreal-migration.md):主二进制只装配
//! SurrealDB 后端(`surreal/`);SQLite 读能力收口进迁移工具 crate
//! `aikun-migrate`(读旧库搬运),本 crate 不再含 SQLite 直连依赖。
//!
//! ## 纪律:能力 trait 签名零 SQL 方言
//!
//! trait 里严禁出现驱动类型(`&Connection` / `Surreal<Any>`)、`?N`
//! 占位、SQL 片段参数与方言函数(`datetime('now')` / `substr` 等);
//! 日期一律 `chrono` 类型,幂等累加一律叫 `record_*`/`rebuild_*`。
//! SurrealQL 只允许出现在 `surreal/` 各 impl 文件内部;SDK 类型
//! (`Surreal<Any>` / `RecordId`)不出模块公共能力面。
//!
//! ## 域类型与实现分册
//!
//! * 共享域类型(错误 / 价格条目 / 限额快照 / 缓存令牌):本文件;
//! * 连接管理 + schemafull schema:`surreal/mod.rs`(SCHEMA_SURQL);
//! * 身份域(user / api_key):`surreal/identity.rs`;
//! * 渠道域(provider / model_health):`surreal/channels_store.rs`;
//! * 计费域(价格 / 日志 / 计数器 / 调账 / 兑换 / 亲和 / 统计):
//!   `surreal/billing_store.rs`。
//!
//! 历史:SQLite 连接池/迁移/行映射(pool.rs / schema.rs /
//! provider_row.rs)随默认切换移出本 crate,交由 `aikun-migrate`
//! 承担"读旧库"的职责(设计文档 §五的迁移工具口径)。

pub mod crypto;
pub mod limiter;
pub mod password;
pub mod pricing;
pub mod surreal;

// ---------- 错误 ----------

/// 存储错误。`Unavailable` = 存储不可用;`Failed` 携带实现侧细节,
/// 只进日志,不进对外错误体。
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
/// `billing::find_price` 逐值一致。
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

/// 不透明令牌:仅要求 `PartialEq`,调用方不做任何解释。
/// 进程内缓存(渠道拓扑 / 价格表)在每次取用时比对,令牌变化才回源
/// 重载;契约是**尽力而为的单调性**(行内容变化 ⇒ 令牌大概率变化,
/// 允许同内容变化——只是多一次重载)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheToken(String);

impl CacheToken {
    /// 实现侧构造(调用方不构造令牌)。
    pub fn new(stamp: String) -> Self {
        CacheToken(stamp)
    }
}
