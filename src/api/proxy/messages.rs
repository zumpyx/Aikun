use axum::{Json, extract::State, response::Response};
use serde_json::Value;

use crate::AppState;
use crate::auth::{AuthContext, Claims};
use crate::protocol::Protocol;

/// Anthropic Messages endpoint.
/// Accepts Anthropic-format requests and routes them through the same proxy
/// pipeline, converting to the selected provider's native protocol as needed.
pub async fn messages(
    State(state): State<AppState>,
    claims: Claims,
    auth_ctx: AuthContext,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    crate::pipeline::proxy_completion(state, claims, auth_ctx, Protocol::Anthropic, body, &headers)
        .await
}
