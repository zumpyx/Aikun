//! Aikun 管理面 API 脚手架 —— 本文件与整个 crate 由 scripts/gen-api.sh
//! 生成,勿手改;调整一律走生成器配置(openapi-to-rust.toml)或 wrapper。
//!
//! `openapi/` 是 openapi-to-rust 按 openapi.yaml 生成的服务端代码:
//! 管理面(Auth / User / Admin tag)与代理面(Proxy tag,/v1,2026-11-18
//! 纳入生成)。实现侧在装配层实现 `openapi::server::{AuthApi, UserApi,
//! AdminApi, ProxyApi}` trait,用 `openapi::server::{auth_api_router, …}`
//! 挂路由(装配与中间件分层见 docs/architecture-v0.5.md §三.3);
//! Proxy 响应枚举的 `Raw` 变体由 gen-api.sh wrapper 注入(代理面响应由
//! 管线动态产出,类型化变体装不下,impl 只走 Raw)。

// 生成代码触发的已知 clippy 告警(v0.24.0:嵌套 if let 校验块、
// double #[must_use])。crate 级 allow 由 wrapper 模板携带,
// 不允许散进任何生成文件。
#![allow(clippy::collapsible_if, clippy::double_must_use)]

pub mod openapi;

pub use openapi::*;
