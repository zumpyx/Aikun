use axum::{extract::State, response::Response, Json};
use serde_json::Value;

use crate::auth::{AuthContext, Claims};
use crate::proxy::convert::PROTOCOL_RESPONSES;
use crate::AppState;

/// OpenAI Responses API endpoint.
/// Channels that declare the "responses" protocol receive the request
/// verbatim (native pass-through); other channels are served by converting
/// to their native protocol (chat/completions, or Anthropic via composition).
pub async fn responses(
    State(state): State<AppState>,
    claims: Claims,
    auth_ctx: AuthContext,
    Json(body): Json<Value>,
) -> Response {
    super::chat::proxy_completion(state, claims, auth_ctx, PROTOCOL_RESPONSES, body).await
}
