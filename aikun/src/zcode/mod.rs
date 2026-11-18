//! ZCode 应用在二进制侧的 HTTP 端口实现(渠道业务模块化,2026-11-18):
//! OAuth 在线登录(`oauth`)+ 免费额度领取(`claim`)。均注入
//! `aikun-admin` 的端口 trait;后续每应用一个目录模块的结构以此为范本。

pub mod claim;
pub mod oauth;
pub mod scheduler;

pub use claim::ZcodeClaimImpl;
pub use oauth::ZcodeOAuthImpl;
