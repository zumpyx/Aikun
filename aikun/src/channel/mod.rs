//! 渠道层:渠道适配器(adapter)、公共 HTTP 传输(transport)、选路(selector)、
//! 渠道拓扑快照(registry,阶段 3)、渠道熔断(breaker,9.5)、健康检查
//! (health)与模型测试矩阵(model_test)。
//!
//! `registry` 模块是进程内渠道拓扑快照(roadmap 阶段 3);适配器的类型
//! 注册表函数在 `aikun_channels::registry`(直接引用,避免与本模块重名)。

pub mod breaker;
pub mod health;
pub mod minimax;
pub mod model_test;
pub mod registry;
pub mod selector;
pub mod transport;

pub use aikun_channels::ChannelAdapter;
