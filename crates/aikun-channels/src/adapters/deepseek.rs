//! DeepSeek 官方渠道。
//!
//! 对照官方文档：
//! * OpenAI：`https://api.deepseek.com`，`POST /chat/completions`
//! * Anthropic：`https://api.deepseek.com/anthropic`，`POST /messages`
//! * 模型列表：`GET https://api.deepseek.com/models`（不在 anthropic 前缀下，也不是 /v1/models）
//! * 模型：deepseek-flash、deepseek-v4-pro

use aikun_protocol::Protocol;
use serde_json::Value;

use crate::ChannelAdapter;
use crate::provider::Provider;

pub struct DeepSeekAdapter;

pub const KIND: &str = "deepseek";

pub const OPENAI_BASE: &str = "https://api.deepseek.com";
pub const ANTHROPIC_BASE: &str = "https://api.deepseek.com/anthropic";
pub const MODELS_URL: &str = "https://api.deepseek.com/models";

pub const DEFAULT_MODELS: &[&str] = &["deepseek-flash", "deepseek-v4-pro"];

impl ChannelAdapter for DeepSeekAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn label(&self) -> &'static str {
        "DeepSeek"
    }

    fn description(&self) -> &'static str {
        "DeepSeek 官方。默认走 OpenAI Responses（POST /responses）。\
         健康和模型列表固定 GET https://api.deepseek.com/models。"
    }

    fn chat_url(&self, provider: &Provider, protocol: Protocol) -> String {
        let configured = crate::provider::base_url_for(provider, protocol);
        let base = if configured.trim().is_empty() {
            match protocol {
                Protocol::Anthropic => ANTHROPIC_BASE,
                Protocol::OpenAi | Protocol::Responses => OPENAI_BASE,
            }
        } else {
            configured
        };
        let path = match protocol {
            Protocol::Anthropic => "/messages",
            Protocol::OpenAi => "/chat/completions",
            Protocol::Responses => "/responses",
        };
        // Anthropic base 已含 /anthropic，不能再被 build_api_url 补 /v1。
        if protocol == Protocol::Anthropic {
            format!("{}{}", base.trim_end_matches('/'), path)
        } else {
            crate::url::build_api_url(base, path)
        }
    }

    /// 健康和拉模型都打官方 /models，不跟协议 base 走。
    fn models_url(&self, _base_url: &str, _protocol: Protocol) -> String {
        MODELS_URL.to_string()
    }

    fn prepare_request(&self, _provider: &Provider, _protocol: Protocol, body: &mut Value) {
        if body
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .is_empty()
        {
            body["model"] = Value::String("deepseek-flash".into());
        }
    }
}
