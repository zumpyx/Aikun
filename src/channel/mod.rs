//! 渠道层:渠道适配器(adapter)、公共 HTTP 传输(transport)、选路(selector)、
//! 健康检查(health)与模型测试矩阵(model_test)。
//!
//! roadmap-v0.5 阶段 3 将在此新增 `registry.rs`(进程内渠道拓扑快照),
//! 并把 selector 改为吃快照的纯函数;适配器的类型注册表因此放在
//! `adapter/registry.rs`,避免与之重名。

pub mod adapter;
pub mod health;
pub mod model_test;
pub mod selector;
pub mod transport;

pub use adapter::{ChannelAdapter, adapter_for, registry};
