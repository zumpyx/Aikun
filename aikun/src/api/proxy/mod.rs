//! /v1 代理面的手写残余:仅剩 OpenAPI 表达不了的通配兜底路由
//! (白名单外 404)。8 个显式端点已迁 openapi-to-rust 生成
//! (装配层 `crate::relay`,2026-11-18)。

pub mod passthrough;
