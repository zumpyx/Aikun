//! 失败分类与故障转移硬边界(阶段 2 拆分出的第三个可单测单元,9.4)。
//!
//! **硬边界:未向客户端发出任何字节才允许换渠道。** [`ClientOutput`]
//! 记录客户端输出是否已开始;[`failover_allowed`] 是唯一允许"再 attempt
//! 下一条渠道"的判定入口,`Started` 一律拒绝——上游失败只能报错收尾,
//! 不得拼接两个渠道的输出。
//!
//! 管线里的对应结构保证(候选迭代 `dispatch::attempt_loop` 的形状使然):
//! 单次 attempt 只有两种出口——返回最终响应(流式在 200+SSE 头时把整个
//! ctx move 进生成器,`attempt_loop` 随即返回),或返回 [`GatewayError`]
//! 交给本模块判定。**不存在**"已向客户端写出后再回到候选迭代"的分支:
//! 流内失败(stall / 传输错误 / 错误帧)全部留在流转发单元收尾。

/// 失败的三分类(9.4):可重试 / 不可重试 / 凭证死亡。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    /// 可换渠道重试:传输失败、408/429/5xx、200+坏体。
    Retryable,
    /// 客户端请求的错(400/404 等):快速失败,原样回传。
    Fatal,
    /// 渠道凭证死亡(上游 401/403):本渠道立即跳过并计失败,
    /// 只要还没向客户端写出字节就换下一条渠道。
    CredentialDead,
}

impl FailureKind {
    /// 上游 HTTP 状态 → 失败分类(与旧 dispatch/passthrough 的重试条件
    /// 逐条对齐:401/403/408/429/>=500 可换渠道,其余 4xx 快速失败)。
    pub(crate) fn from_status(status: u16) -> Self {
        match status {
            401 | 403 => FailureKind::CredentialDead,
            408 | 429 => FailureKind::Retryable,
            s if s >= 500 => FailureKind::Retryable,
            _ => FailureKind::Fatal,
        }
    }

    /// 是否允许作为"换下一条渠道"的理由(不含输出边界)。
    pub(crate) fn may_failover(self) -> bool {
        matches!(self, FailureKind::Retryable | FailureKind::CredentialDead)
    }
}

/// 客户端输出状态(9.4 硬边界的另一维)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientOutput {
    /// 尚未向客户端发出任何字节(响应头未提交):允许换渠道。
    NotStarted,
    /// 首个字节(或 SSE 头)已写出:只能报错收尾,不得再 attempt。
    ///
    /// 生产路径**从不构造**该值——流式移交后候选迭代即返回(见
    /// `dispatch::attempt_loop`),不存在"已写出后再 attempt"的分支;
    /// 它存在是为了让边界判定([`failover_allowed`])与单测把"绝不"
    /// 写成可执行断言。
    #[cfg_attr(not(test), allow(dead_code))]
    Started,
}

/// 故障转移决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailoverDecision {
    /// 换下一条渠道。
    NextChannel,
    /// 报错收尾(不可重试,或已向客户端写出)。
    FinishWithError,
}

/// 硬边界判定:唯一允许继续尝试下一条渠道的入口。
pub(crate) fn failover_allowed(kind: FailureKind, output: ClientOutput) -> FailoverDecision {
    match (kind, output) {
        // 核心边界:只要已向客户端写出,任何失败都只能收尾。
        (_, ClientOutput::Started) => FailoverDecision::FinishWithError,
        (kind, ClientOutput::NotStarted) if kind.may_failover() => FailoverDecision::NextChannel,
        (_, ClientOutput::NotStarted) => FailoverDecision::FinishWithError,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 9.4 验收:响应头尚未发出时,可重试错误换渠道。
    #[test]
    fn retryable_errors_switch_channel_before_any_output() {
        for status in [0u16, 408, 429, 500, 502, 503] {
            let kind = if status == 0 {
                FailureKind::Retryable
            } else {
                FailureKind::from_status(status)
            };
            assert_eq!(
                failover_allowed(kind, ClientOutput::NotStarted),
                FailoverDecision::NextChannel,
                "status={status}"
            );
        }
    }

    /// 9.4 验收:第一个 SSE/字节写出后,再遇到可重试失败也不再选路。
    #[test]
    fn no_channel_switch_after_first_byte_written() {
        for kind in [
            FailureKind::Retryable,
            FailureKind::CredentialDead,
            FailureKind::Fatal,
        ] {
            assert_eq!(
                failover_allowed(kind, ClientOutput::Started),
                FailoverDecision::FinishWithError,
                "kind={kind:?}"
            );
        }
    }

    /// 不可重试(其余 4xx)在写出前后都不换渠道。
    #[test]
    fn fatal_client_errors_never_fail_over() {
        for status in [400u16, 402, 404, 422] {
            assert_eq!(FailureKind::from_status(status), FailureKind::Fatal);
            assert_eq!(
                failover_allowed(FailureKind::from_status(status), ClientOutput::NotStarted),
                FailoverDecision::FinishWithError
            );
        }
    }

    /// 凭证死亡(401/403)在未输出时允许换渠道,语义上仍属"可故障转移"。
    #[test]
    fn dead_credentials_skip_channel_but_allow_failover() {
        for status in [401u16, 403] {
            let kind = FailureKind::from_status(status);
            assert_eq!(kind, FailureKind::CredentialDead);
            assert!(kind.may_failover());
            assert_eq!(
                failover_allowed(kind, ClientOutput::NotStarted),
                FailoverDecision::NextChannel
            );
        }
    }

    /// 分类口径快照(与旧实现的重试条件逐条对齐)。
    #[test]
    fn status_classification_snapshot() {
        let cases: &[(u16, FailureKind)] = &[
            (200, FailureKind::Fatal), // 200 不该走到分类,防御性 Fatal
            (400, FailureKind::Fatal),
            (401, FailureKind::CredentialDead),
            (403, FailureKind::CredentialDead),
            (408, FailureKind::Retryable),
            (429, FailureKind::Retryable),
            (500, FailureKind::Retryable),
            (502, FailureKind::Retryable),
            (504, FailureKind::Retryable),
        ];
        for (status, want) in cases {
            assert_eq!(&FailureKind::from_status(*status), want, "status={status}");
        }
    }
}
