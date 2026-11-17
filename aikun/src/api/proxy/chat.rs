use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::{AuthContext, Claims};
use crate::channel::selector::list_available_models_async;
use crate::pipeline::admission::model_allowed;
use crate::pipeline::proxy_completion;
use aikun_protocol::Protocol;

/// Unified chat completions endpoint (OpenAI-compatible).
pub async fn chat_completion(
    State(state): State<AppState>,
    claims: Claims,
    auth_ctx: AuthContext,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    proxy_completion(state, claims, auth_ctx, Protocol::OpenAi, body, &headers).await
}

/// List available models aggregated from all healthy providers.
/// Returns OpenAI-compatible model list format.
/// 受限 API key 只返回其白名单内的模型,过滤口径与 chat 请求的
/// model_not_allowed 判定一致(精确匹配或 `*` 后缀前缀匹配)。
pub async fn list_models(
    State(state): State<AppState>,
    _claims: Claims,
    auth_ctx: AuthContext,
) -> impl IntoResponse {
    let models = list_available_models_async(state.store.clone()).await;

    let data: Vec<Value> = models
        .into_iter()
        .filter(|id| model_allowed(&auth_ctx.allowed_models, id))
        .map(|id| {
            json!({
                "id": id,
                "object": "model",
                "created": chrono::Utc::now().timestamp(),
                "owned_by": "aikun"
            })
        })
        .collect();

    (
        StatusCode::OK,
        Json(json!({
            "object": "list",
            "data": data
        })),
    )
}
