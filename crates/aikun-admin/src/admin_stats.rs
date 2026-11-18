//! Admin tag · stats 方法组(仪表盘用量聚合 / 全局计数快照)实现。
//!
//! **指派:集成工人填充**(plan 步骤 6 并行批次未单设 Stats 工人,由
//! 集成工人随装配统一完成;业务逻辑迁自 `aikun/src/api/logs.rs` 的
//! `usage_stats` / `admin_stats`,响应形状不变,见 openapi.yaml)。
//! 本文件只写**固有方法**(签名 = trait 方法去掉 self 的参数,返回同名
//! Response 枚举);`impl AdminApi for AdminService` 的委托在 lib.rs,
//! 由 `AdminService.stats` 字段路由到此处。装配与删旧由集成工人统一做;
//! 纪律与写法见 docs/migration-playbook.md。
//!
//! # 装配契约(main.rs 侧,参照 DbAuthStore 模式)
//!
//! * 过渡期 DB 经端口 trait [`StatsStore`] 注入:适配器(`DbStatsStore`)
//!   写在 main.rs,SQL 语义迁自旧 handler;aikun-store 就位后由 store
//!   层收编(plan 步骤 8)。
//! * 401/403 由装配层中间件(auth_middleware + require_admin)承担,
//!   impl 内不产。
//! * **响应枚举无 5xx 变体**(AdminGetUsageStatsResponse /
//!   AdminGetGlobalStatsResponse 只有 Ok/Unauthorized/Forbidden):旧实现
//!   "查询失败 → 500 query_failed" 无法表达。过渡折衷与 Users/Billing 批
//!   同族(admin_users.rs 文件头):fail-closed 403 + 真实 500 词表 code +
//!   `tracing::error!` 留痕(spec 缺口记 notes 待裁决)。
//!
//! # 语义口径(与旧实现逐条对齐,api/logs.rs::usage_stats / admin_stats)
//!
//! * 日期窗口按 `substr(created_at, 1, 10)`(库内 SQLite datetime 与
//!   RFC3339 两种格式通用)对齐 UTC 日期;today / week / month 三组计数
//!   由同一查询条件求和拆出;
//! * 30 天日序列缺日补零(图表完整序列);top_models 按请求数排序
//!   LIMIT 8;
//! * 全局计数是全系统总量(api_keys / users / providers)+ 今日请求数。

use std::sync::Arc;

use aikun_openapi::openapi::server::errors::*;
use aikun_openapi::openapi::types::*;

use crate::problem;

// ---------- 过渡期存储端口(装配层实现;SQL 语义写在方法文档里) ----------

/// 用量聚合原始行(近 30 天窗口)。daily 未补零——缺日补零是表现层
/// 语义,与查询无关。
pub struct UsageSnapshot {
    /// (requests, tokens) × (today / 近 7 天 / 近 30 天)
    pub today: (i64, i64),
    pub week: (i64, i64),
    pub month: (i64, i64),
    /// (date "YYYY-MM-DD", requests, tokens, avg_latency_ms, success_rate_pct)
    /// 按日期升序;success_rate_pct 为 0..=100 百分数(SQL 聚合原文)。
    pub daily: Vec<(String, i64, i64, f64, f64)>,
    /// (model, requests, tokens) 按请求数降序 LIMIT 8
    pub top_models: Vec<(String, i64, i64)>,
}

/// 全局计数快照(api_keys / users / providers 全系统总量 + 今日请求数)。
pub struct GlobalCounts {
    pub api_keys: i64,
    pub users: i64,
    pub providers: i64,
    pub requests_today: i64,
}

/// stats 方法组端口(过渡期由 main.rs 适配器实现,aikun-store 就位后收编)。
/// 方法是同步的:纯查询 + 进程内互斥锁,单条开销极小(与 AdminUsersStore
/// 同口径);`Err(())` = 存储不可用(连接锁中毒 / SQL 失败)。
/// 阶段 S async 化(docs/surreal-migration.md §六):端口随装配适配器
/// 翻转 `#[async_trait]`。
#[allow(clippy::result_unit_err)] // 与 crate 内其他端口 trait 同约定
#[async_trait::async_trait]
pub trait StatsStore: Send + Sync + 'static {
    /// 近 30 天用量聚合(单查询拆出 today/week/month 三组 + 日序列 +
    /// top 模型;日期口径 `substr(created_at, 1, 10)`,UTC)。
    /// `Err(())` 与 AuthStore/UserStore 同口径:存储不可用,细节只进日志。
    async fn usage_snapshot(&self) -> Result<UsageSnapshot, ()>;

    /// 全局计数快照:`(SELECT COUNT(*) FROM api_keys) / users / providers
    /// + request_logs 中今日行数(substr 前缀 = date('now'))`。
    async fn global_counts(&self) -> Result<GlobalCounts, ()>;
}

// ---------- 服务 ----------

/// stats 方法组服务。具体类型(lib.rs 的 `AdminService.stats` 字段约束,
/// 路由工厂只收一个 `AdminApi` 实现者),端口以 `Arc<dyn StatsStore>` 注入。
#[derive(Clone)]
pub struct AdminStatsService {
    store: Arc<dyn StatsStore>,
}

impl AdminStatsService {
    pub fn new(store: Arc<dyn StatsStore>) -> Self {
        Self { store }
    }

    /// 仪表盘用量聚合
    ///
    /// `GET /api/usage-stats`(旧 GET /api/admin/usage-stats;响应形状不变)
    pub async fn admin_get_usage_stats(&self) -> AdminGetUsageStatsResponse {
        let snap = match self.store.usage_snapshot().await {
            Ok(s) => s,
            Err(()) => {
                tracing::error!("admin_get_usage_stats: store unavailable");
                return AdminGetUsageStatsResponse::Forbidden(problem(
                    403,
                    "query_failed",
                    "查询用量统计失败",
                ));
            }
        };

        // 缺日补零:窗口固定近 30 天(UTC 日期),与旧实现一致。
        let today = chrono::Utc::now().date_naive();
        let fmt = |d: chrono::NaiveDate| d.format("%Y-%m-%d").to_string();
        let daily_map: std::collections::HashMap<String, (i64, i64, f64, f64)> = snap
            .daily
            .iter()
            .map(|(d, r, t, lat, sr)| (d.clone(), (*r, *t, *lat, *sr)))
            .collect();
        let daily = (0..30)
            .map(|i| {
                let d = fmt(today - chrono::Duration::days(29 - i));
                let (r, t, lat, sr) = daily_map.get(&d).copied().unwrap_or((0, 0, 0.0, 0.0));
                UsageStatsDailyItem {
                    avg_latency_ms: Some(lat.round() as i64),
                    date: chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok(),
                    requests: Some(r),
                    // 两位小数百分数(与 logs/stats 同口径)
                    success_rate: Some((sr * 100.0).round() / 100.0),
                    tokens: Some(t),
                }
            })
            .collect();

        AdminGetUsageStatsResponse::Ok(UsageStats {
            daily,
            month: UsageStatsMonth {
                requests: Some(snap.month.0),
                tokens: Some(snap.month.1),
            },
            today: UsageStatsToday {
                requests: Some(snap.today.0),
                tokens: Some(snap.today.1),
            },
            top_models: snap
                .top_models
                .into_iter()
                .map(|(model, requests, tokens)| UsageStatsTopModelsItem {
                    model: Some(model),
                    requests: Some(requests),
                    tokens: Some(tokens),
                })
                .collect(),
            week: UsageStatsWeek {
                requests: Some(snap.week.0),
                tokens: Some(snap.week.1),
            },
        })
    }

    /// 全局计数快照
    ///
    /// `GET /api/system/stats`(旧 GET /api/admin/stats;响应形状不变)
    pub async fn admin_get_global_stats(&self) -> AdminGetGlobalStatsResponse {
        let counts = match self.store.global_counts().await {
            Ok(c) => c,
            Err(()) => {
                tracing::error!("admin_get_global_stats: store unavailable");
                return AdminGetGlobalStatsResponse::Forbidden(problem(
                    403,
                    "query_failed",
                    "查询全局统计失败",
                ));
            }
        };
        AdminGetGlobalStatsResponse::Ok(GlobalStats {
            api_keys: counts.api_keys,
            providers: counts.providers,
            requests_today: counts.requests_today,
            users: counts.users,
        })
    }
}
