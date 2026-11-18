//! [`RequestCtx`]:一次 /v1 请求贯穿管线的全部上下文(阶段 2)。
//!
//! 收敛此前 dispatch(14 参)/ stream(11 参)的长参数列表:auth 维度、
//! key 限额状态(并发槽位 guard)、已试渠道顺序、取消令牌、计费所需的
//! request_id / request_type 都挂在这里,沿"入口构造 → attempt_loop →
//! stream_response → 生成器 Drop"一条所有权链移动,不再手工 take()。

use std::sync::{Arc, Mutex};
use std::time::Instant;

use aikun_protocol::Protocol;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::pipeline::admission::ConcurrencyGuard;

/// 转换模式:标准端点走完整协议转换;通用端点(embeddings 等)作为
/// `NoConversion + OpenAiOnly + NoStream` 并入管线(阶段 2)。
#[derive(Debug, Clone)]
pub(crate) enum Conversion {
    /// chat / messages / responses:按渠道协议双向转换。
    Standard,
    /// 字节级透传:无协议转换、仅声明 openai 协议的渠道、恒非流式。
    /// `path` 是 /v1/ 之后的白名单路径(如 "embeddings"),兼作日志的
    /// request_type。
    Passthrough { path: String },
}

impl Conversion {
    /// 日志 request_type:标准端点按客户端协议名,透传按端点路径。
    pub(crate) fn request_type(&self, client_protocol: Protocol) -> String {
        match self {
            Conversion::Standard => client_protocol.as_str().to_string(),
            Conversion::Passthrough { path } => path.clone(),
        }
    }
}

/// 一次 /v1 请求的完整上下文。构造于管线入口,随故障转移循环一路移动;
/// 流式路径下整个 ctx 被 move 进 SSE 生成器——并发槽位、取消令牌与
/// 499 补账(StreamAbortGuard 的数据源)因此与生成器同生命周期。
pub(crate) struct RequestCtx {
    /// 请求追踪号(UUID v4,9.6):贯穿 span、request_logs 与 X-Request-Id。
    pub request_id: String,
    /// 客户端身份:用户 id / api key id(JWT 会话无 key 维度)。
    pub user_id: String,
    pub api_key_id: Option<String>,
    pub client_protocol: Protocol,
    /// 客户端请求的模型名(映射前的原名)。
    pub model: String,
    /// 转换模式(标准 / 字节级透传)。
    pub conversion: Conversion,
    /// 原始客户端请求体(未转换;每次 attempt 从它派生上游体)。
    pub body: Value,
    /// 是否流式(透传模式恒 false)。
    pub is_stream: bool,
    /// 已尝试渠道(按选择顺序):整体 deadline 取消时对最后在试渠道补记
    /// 失败;一次多渠道重试链按 request_id + 该顺序拼出(9.6)。
    pub tried_order: Arc<Mutex<Vec<String>>>,
    /// API key 并发槽位:Drop 归还。流式路径随 ctx move 进生成器,持有到
    /// 流结束 / 客户端断连 / 进程关停。
    pub concurrency_guard: Option<ConcurrencyGuard>,
    /// 响应缓存键(透传与流式为 None)。
    pub cache_key: Option<String>,
    /// 取消令牌(9.2):关停与客户端断开都触发;流转发循环 select 它,
    /// 上游请求随生成器 Drop 一并中止。子令牌由入口从全局 shutdown 派生。
    pub cancel: CancellationToken,
    /// 请求级 tracing span(9.6):request_id / 模型 / 渠道 / 缓存命中 /
    /// TTFT / 总耗时经它打点;`Span` 克隆廉价,入口持一份记总耗时,
    /// ctx 这份随流式路径进生成器记 TTFT。
    pub span: tracing::Span,
    /// 请求起点(总耗时口径,9.6)。
    pub started: Instant,
}

impl RequestCtx {
    /// 日志 request_type(见 [`Conversion::request_type`])。
    pub(crate) fn request_type(&self) -> String {
        self.conversion.request_type(self.client_protocol)
    }

    /// 是否字节级透传模式。
    pub(crate) fn is_passthrough(&self) -> bool {
        matches!(self.conversion, Conversion::Passthrough { .. })
    }

    /// 透传路径(Passthrough 模式);Standard 模式返回 None。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn passthrough_path(&self) -> Option<&str> {
        match &self.conversion {
            Conversion::Passthrough { path } => Some(path),
            Conversion::Standard => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_body() -> Value {
        serde_json::json!({"model": "m"})
    }

    fn test_ctx(conversion: Conversion) -> RequestCtx {
        RequestCtx {
            request_id: "req-1".into(),
            user_id: "u1".into(),
            api_key_id: Some("k1".into()),
            client_protocol: Protocol::OpenAi,
            model: "m".into(),
            conversion,
            body: ctx_body(),
            is_stream: false,
            tried_order: Arc::new(Mutex::new(Vec::new())),
            concurrency_guard: None,
            cache_key: None,
            cancel: CancellationToken::new(),
            span: tracing::Span::none(),
            started: Instant::now(),
        }
    }

    #[test]
    fn request_type_follows_conversion_mode() {
        // 标准端点:协议名
        let c = test_ctx(Conversion::Standard);
        assert_eq!(c.request_type(), "openai");
        assert!(!c.is_passthrough());
        // 透传:路径名
        let c = test_ctx(Conversion::Passthrough {
            path: "embeddings".into(),
        });
        assert_eq!(c.request_type(), "embeddings");
        assert_eq!(c.passthrough_path(), Some("embeddings"));
        assert!(c.is_passthrough());
    }

    #[test]
    fn anthropic_client_reports_anthropic_request_type() {
        let mut c = test_ctx(Conversion::Standard);
        c.client_protocol = Protocol::Anthropic;
        assert_eq!(c.request_type(), "anthropic");
    }
}
