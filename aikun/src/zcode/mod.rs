//! ZCode 应用在二进制侧的 HTTP 端口实现(渠道业务模块化,2026-11-18):
//! OAuth 在线登录(`oauth`)+ 免费额度领取(`claim`)。均注入
//! `aikun-admin` 的端口 trait;captcha = 领取的阿里验证码求解(node
//! 子进程,happy-dom;炼化自 Zcode2Api)。

pub mod captcha;
pub mod claim;
pub mod oauth;
pub mod offpeak;
pub mod pat;
pub mod scheduler;

pub use claim::ZcodeClaimImpl;
pub use oauth::ZcodeOAuthImpl;
