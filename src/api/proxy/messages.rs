use axum::{extract::State, response::Response, Json};
use serde_json::Value;

use crate::auth::{AuthContext, Claims};
use crate::proxy::convert::PROTOCOL_ANTHROPIC;
use crate::AppState;

/// Anthropic Messages endpoint.
/// Accepts Anthropic-format requests and routes them through the same proxy
/// pipeline, converting to the selected provider's native protocol as needed.
pub async fn messages(
    State(state): State<AppState>,
    claims: Claims,
    auth_ctx: AuthContext,
    Json(body): Json<Value>,
) -> Response {
    super::chat::proxy_completion(state, claims, auth_ctx, PROTOCOL_ANTHROPIC, body).await
}
