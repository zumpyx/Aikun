//! WorkBuddy 渠道(占位实现)。
//!
//! TODO: WorkBuddy 的官方接口协议尚未公开 / 尚未拿到请求样例。当前按 OpenAI
//! 兼容协议转发(与 generic 相同),只是单独成为一个渠道类型,便于:
//! - 管理端把这类账号单独归类;
//! - 拿到真实协议后只改本文件:覆写 `chat_url` / `apply_auth` /
//!   `prepare_request` / `normalize_response` / `normalize_stream_event`,
//!   网关其余部分(选路、故障转移、计费、协议转换)无需改动。

use crate::ChannelAdapter;

pub struct WorkBuddyAdapter;

pub const KIND: &str = "workbuddy";

impl ChannelAdapter for WorkBuddyAdapter {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn label(&self) -> &'static str {
        "WorkBuddy(实验)"
    }

    fn description(&self) -> &'static str {
        "占位实现:暂按 OpenAI 兼容协议转发,待接入 WorkBuddy 真实协议"
    }

    fn experimental(&self) -> bool {
        true
    }
}
