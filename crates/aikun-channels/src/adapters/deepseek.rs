//! DeepSeek 官方渠道。三条地址分开，互不混用：
//!
//! * base：`https://api.deepseek.com`（不是 /v1）
//! * 对话：`POST https://api.deepseek.com/chat/completions`
//! * 模型列表 / 健康：`GET https://api.deepseek.com/models`
//!
//! 适配器写死，不看渠道上存的协议和 base。

use aikun_protocol::Protocol;
use serde_json::Value;

use crate::ChannelAdapter;
use crate::provider::Provider;

pub struct DeepSeekAdapter;

pub const KIND: &str = "deepseek";

pub const OPENAI_BASE: &str = "https://api.deepseek.com";
pub const CHAT_URL: &str = "https://api.deepseek.com/chat/completions";
pub const MODELS_URL: &str = "https://api.deepseek.com/models";
pub const BALANCE_URL: &str = "https://api.deepseek.com/user/balance";

pub const DEFAULT_MODELS: &[&str] = &["deepseek-flash", "deepseek-v4-pro"];

impl ChannelAdapter for DeepSeekAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn label(&self) -> &'static str {
        "DeepSeek"
    }

    fn description(&self) -> &'static str {
        "DeepSeek 官方。对话 POST /chat/completions，模型列表 GET /models，\
         都在 https://api.deepseek.com，不带 /v1。"
    }

    fn chat_url(&self, _provider: &Provider, _protocol: Protocol) -> String {
        CHAT_URL.to_string()
    }

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
