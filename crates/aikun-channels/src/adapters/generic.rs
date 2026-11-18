//! 通用渠道:OpenAI / Anthropic / Responses 协议兼容的上游(官方 API、
//! one-api / new-api 类中转站、DeepSeek、智谱、MiniMax、OrcaRouter 等)。
//! 即重构前的全部行为;channel_kind 为空的存量渠道也落到这里。

use crate::ChannelAdapter;

pub struct GenericAdapter;

pub const KIND: &str = "generic";

impl ChannelAdapter for GenericAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn label(&self) -> &'static str {
        "通用(OpenAI / Anthropic 兼容)"
    }

    fn description(&self) -> &'static str {
        "按渠道勾选的协议直连上游,适用于官方 API 与绝大多数兼容中转站"
    }
}
