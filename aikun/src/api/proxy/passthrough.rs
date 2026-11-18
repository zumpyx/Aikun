//! 通用端点字节级透传的 HTTP 入口(阶段 2 并入管线后只剩薄壳):
//! 白名单判定与全部管线口径(准入 / 选路 / 故障转移 / 记账 /
//! request_id / X-Request-Id / 脱敏)都在 `pipeline::passthrough_completion`
//! ——`NoConversion + OpenAiOnly + NoStream` 模式,无重复实现。

use axum::{
    Json,
    extract::{Path, State},
    response::Response,
};
use serde_json::Value;

use crate::AppState;
use crate::auth::{AuthContext, Claims};

/// POST /v1/{*path} 的兜底 handler(显式路由优先于通配,chat/messages/
/// responses/models 不走这里)。客户端协议按 OpenAI 处理。
pub async fn passthrough(
    State(state): State<AppState>,
    claims: Claims,
    auth_ctx: AuthContext,
    Path(path): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    crate::pipeline::passthrough_completion(state, claims, auth_ctx, path, body, &headers).await
}
