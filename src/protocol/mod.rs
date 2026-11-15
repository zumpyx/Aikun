//! 协议层:OpenAI Chat Completions / OpenAI Responses / Anthropic Messages
//! 三种线协议之间的请求、响应与 SSE 流式事件双向转换。
//!
//! 本层只做纯数据变换(serde_json::Value → Value),不碰网络、数据库与计费,
//! 便于单测。渠道插件(`crate::channels`)负责"怎么发",网关管线
//! (`crate::gateway`)负责"发给谁、失败了怎么办"。

pub mod convert;
