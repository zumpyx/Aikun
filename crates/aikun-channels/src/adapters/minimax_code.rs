//! MiniMax Code 渠道适配器（官方 managed-login 反代）。
//!
//! 对照 MiniMax-AI/minimax-code：
//! * 国内：`https://agent.minimax.cn/mavis/api/v1/llm/v1`
//! * 国际：`https://agent.minimax.io/mavis/api/v1/llm/v1`
//! * 协议 Anthropic Messages，鉴权 `Authorization: Bearer`（登录后的 access token）
//! * 模型以官方 catalog 为准：MiniMax-M3 / M3.1-Flash / M2.7 / M2.7-highspeed

use aikun_protocol::Protocol;
use serde_json::Value;

use crate::ChannelAdapter;
use crate::provider::Provider;

pub struct MiniMaxCodeAdapter;

pub const KIND: &str = "minimax-code";

pub const CN_BASE: &str = "https://agent.minimax.cn/mavis/api/v1/llm/v1";
pub const INTL_BASE: &str = "https://agent.minimax.io/mavis/api/v1/llm/v1";

pub const DEFAULT_MODELS: &[&str] = &[
    "MiniMax-M3",
    "MiniMax-M3.1-Flash",
    "MiniMax-M2.7",
    "MiniMax-M2.7-highspeed",
];

fn is_cn(provider: &Provider) -> bool {
    let note = provider.note.to_lowercase();
    let website = provider.website_url.to_lowercase();
    let base = format!(
        "{} {}",
        provider.openai_base_url, provider.anthropic_base_url
    )
    .to_lowercase();
    note.contains("国内") || website.contains("minimax.cn") || base.contains("minimax.cn")
}

impl ChannelAdapter for MiniMaxCodeAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn label(&self) -> &'static str {
        "MiniMax Code"
    }

    fn description(&self) -> &'static str {
        "MiniMax Code 订阅反代。API Key 填 managed-login 的 access token，\
         只勾选 Anthropic。国内 Base 填 https://agent.minimax.cn/mavis/api/v1/llm/v1"
    }

    fn chat_url(&self, provider: &Provider, protocol: Protocol) -> String {
        let configured = crate::provider::base_url_for(provider, protocol);
        let base = if configured.trim().is_empty() {
            if is_cn(provider) { CN_BASE } else { INTL_BASE }
        } else {
            configured
        };
        crate::url::build_api_url(base, "/messages")
    }

    fn models_url(&self, base_url: &str, _protocol: Protocol) -> String {
        crate::url::build_api_url(base_url, "/models")
    }

    fn prepare_request(&self, _provider: &Provider, _protocol: Protocol, body: &mut Value) {
        // 官方默认模型。请求没带 model 时补上，避免上游 400。
        if body
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .is_empty()
        {
            body["model"] = Value::String("MiniMax-M3.1-Flash".into());
        }
    }
}
