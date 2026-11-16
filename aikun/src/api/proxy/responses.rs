use axum::{Json, extract::State, response::Response};
use serde_json::Value;

use crate::AppState;
use crate::auth::{AuthContext, Claims};
use aikun_protocol::Protocol;

/// OpenAI Responses API endpoint.
/// Channels that declare the "responses" protocol receive the request
/// verbatim (native pass-through); other channels are served by converting
/// to their native protocol (chat/completions, or Anthropic via composition).
pub async fn responses(
    State(state): State<AppState>,
    claims: Claims,
    auth_ctx: AuthContext,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    crate::pipeline::proxy_completion(state, claims, auth_ctx, Protocol::Responses, body, &headers)
        .await
}
