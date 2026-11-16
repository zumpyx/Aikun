//! 渠道适配层(Channel adapters)。
//!
//! 注意:这不是运行时插件系统(roadmap-v0.5「明确不做:插件系统」)——适配器
//! 在编译期静态注册,只是把"厂商与标准协议的差异"收敛到一个 trait 里。
//!
//! 每一种上游"渠道类型"(channel kind)实现一个 [`ChannelAdapter`]:它决定
//! 请求发到哪个 URL、带什么鉴权头、发送前后要不要对请求体 / 响应体 / SSE
//! 事件做厂商特有的修补。HTTP 发送、超时、代理、故障转移与协议转换仍由
//! 公共层负责,插件只描述"这个厂商和标准协议有什么不同"。
//!
//! ## 新增一个渠道类型(三步)
//!
//! 1. 在 `src/channel/adapter/` 下新建 `xxx.rs`,实现 [`ChannelAdapter`]
//!    (通常只需覆写 `kind` / `label` 和少数几个钩子,其余用默认实现);
//! 2. 在本文件加一行 `pub mod xxx;`,并在 [`registry::ChannelRegistry::builtin`]
//!    里加一行 `reg.register(Arc::new(xxx::XxxAdapter))`;
//! 3. 管理端添加渠道时在「渠道类型」下拉中选择它(值写入 providers.channel_kind)。
//!
//! 上游本身就兼容 OpenAI / Anthropic 协议时不需要写代码:用默认的
//! `generic` 类型、填好 Base URL 即可。

pub mod generic;
pub mod qoder;
pub mod registry;
pub mod workbuddy;

use reqwest::RequestBuilder;
use serde_json::Value;

use crate::models::Provider;
use crate::protocol::convert::{SseEvent, PROTOCOL_ANTHROPIC, PROTOCOL_RESPONSES};

pub use registry::{adapter_for, registry};

/// 渠道插件接口。所有方法都有与现网行为一致的默认实现,
/// 插件只覆写和标准协议不同的部分。
///
/// 方法均为同步:网络 I/O 统一在 [`transport::send_request`] 里完成,
/// 插件只做请求 / 响应的"形状修补",便于单测、也不引入 async-trait 依赖。
pub trait ChannelAdapter: Send + Sync + 'static {
    /// 渠道类型标识,写入 providers.channel_kind(小写、稳定、不可改名)。
    fn kind(&self) -> &'static str;

    /// 管理端展示名。
    fn label(&self) -> &'static str;

    /// 管理端展示的说明文字。
    fn description(&self) -> &'static str {
        ""
    }

    /// 是否为占位 / 实验性实现(真实协议尚未接入),管理端会加提示。
    fn experimental(&self) -> bool {
        false
    }

    /// 给定上游协议的端点路径(相对 Base URL)。
    fn endpoint_path(&self, protocol: &str) -> &'static str {
        default_endpoint_path(protocol)
    }

    /// 对话请求的完整上游 URL。
    fn chat_url(&self, provider: &Provider, protocol: &str) -> String {
        crate::channel::transport::build_api_url(
            crate::models::base_url_for(provider, protocol),
            self.endpoint_path(protocol),
        )
    }

    /// 模型列表 / 健康探测 URL。只拿 base_url:管理端「获取模型」在渠道
    /// 落库之前就会调用,没有完整的 Provider。
    fn models_url(&self, base_url: &str, _protocol: &str) -> String {
        crate::channel::transport::build_api_url(base_url, "/models")
    }

    /// 写入鉴权头。默认:anthropic 协议同时带 x-api-key 与 Bearer
    /// (部分 Anthropic 兼容上游只认 Authorization),其余协议带 Bearer。
    fn apply_auth(&self, req: RequestBuilder, api_key: &str, protocol: &str) -> RequestBuilder {
        default_auth(req, api_key, protocol)
    }

    /// 发送前修补上游请求体(已完成协议转换与模型映射)。
    fn prepare_request(&self, _provider: &Provider, _protocol: &str, _body: &mut Value) {}

    /// 非流式 200 响应体在校验 / 转换前的修补(例如把厂商私有信封
    /// 拆成标准协议形状)。
    fn normalize_response(&self, _protocol: &str, _body: &mut Value) {}

    /// 流式响应中每个 SSE 事件在协议转换前的修补。
    fn normalize_stream_event(&self, _protocol: &str, _event: &mut SseEvent) {}
}

/// 各协议的标准端点路径。
pub fn default_endpoint_path(protocol: &str) -> &'static str {
    if protocol == PROTOCOL_ANTHROPIC {
        "/messages"
    } else if protocol == PROTOCOL_RESPONSES {
        "/responses"
    } else {
        "/chat/completions"
    }
}

/// 默认鉴权头,与重构前 proxy/client.rs 的 apply_auth 完全一致。
pub fn default_auth(req: RequestBuilder, api_key: &str, protocol: &str) -> RequestBuilder {
    if protocol == PROTOCOL_ANTHROPIC {
        req.header("x-api-key", api_key)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("anthropic-version", "2023-06-01")
    } else {
        req.header("Authorization", format!("Bearer {}", api_key))
    }
}
