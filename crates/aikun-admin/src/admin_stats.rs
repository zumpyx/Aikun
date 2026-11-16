//! Admin tag · stats 方法组(仪表盘用量聚合 / 全局计数快照)实现桩。
//!
//! **指派:并行工人·Stats**。当前是 todo!() 桩 —— Auth 试点批只预置
//! 轨道;Stats 批填充方法体。本文件只写**固有方法**(签名 = trait 方法
//! 去掉 self 的参数,返回同名 Response 枚举);`impl AdminApi for
//! AdminService` 的委托在 lib.rs,由 `AdminService.stats` 字段路由到
//! 这里,工人不要改 lib.rs。装配与删旧由集成工人统一做;纪律与写法见
//! docs/migration-playbook.md。**不要改本 crate 的 lib.rs 与其他 impl 文件。**
// 桩文件:todo!() 体不消费参数;工人填充时删除本 allow。
#![allow(unused_variables, unused_imports)]

use aikun_openapi::openapi::server::errors::*;
use aikun_openapi::openapi::types::*;
/// stats 方法组服务。字段与构造函数由 Stats 批工人按需定义(过渡期依赖
/// 经端口 trait 注入,参照 auth.rs 的 AuthStore 模式;适配器写在 main.rs,
/// 由集成工人装配)。
#[derive(Clone, Default)]
pub struct AdminStatsService;

impl AdminStatsService {
    /// 仪表盘用量聚合
    ///
    /// `GET /api/usage-stats`
    pub async fn admin_get_usage_stats(&self) -> AdminGetUsageStatsResponse {
        todo!("Stats 批:GET /api/usage-stats(旧 GET /api/admin/usage-stats;响应形状不变)")
    }

    /// 全局计数快照
    ///
    /// `GET /api/system/stats`
    pub async fn admin_get_global_stats(&self) -> AdminGetGlobalStatsResponse {
        todo!("Stats 批:GET /api/system/stats(旧 GET /api/admin/stats;响应形状不变)")
    }
}
