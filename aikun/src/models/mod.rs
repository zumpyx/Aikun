// 旧 DTO(user/log 的请求/响应模型)已随旧 handler(api/auth.rs、
// api/logs.rs、api/redemption.rs、api/admin/**)删除:它们的消费者只有
// 旧 handler,新契约的请求/响应类型在 crates/aikun-openapi types.rs。
//
// 渠道域类型已移入 aikun-channels crate;Provider 行映射(PROVIDER_SELECT_
// COLUMNS / row_to_provider)随 db 层吸收进 aikun-store(plan 步骤 8)。
// 显式 re-export 保持 crate::models::Xxx 旧路径稳定(迁移映射:
// architecture-v0.5 §四)。
pub use aikun_channels::{
    Provider, base_url_for, channel_protocol, pick_base_url, upstream_protocol_for,
};
pub use aikun_store::provider_row::{PROVIDER_SELECT_COLUMNS, row_to_provider};
