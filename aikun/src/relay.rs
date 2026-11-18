//! Proxy tag(/v1 生成端点)的装配层:实现 `aikun_openapi` 的 [`ProxyApi`]。
//!
//! 装配契约(与 User tag 同一模式,见 `crates/aikun-admin/src/user.rs`
//! 文件头):生成 trait 方法签名只有已解码的 body/path/query,拿不到请求
//! 上下文;/v1 管线需要的 `Claims` / `AuthContext` / 请求头(Cache-Control
//! 缓存判断)由 main.rs 的中间件链提供——`auth_middleware` 验证并塞
//! extensions → [`relay_scope`] 取出连同请求头克隆放进 task_local,impl 内
//! [`relay_ctx()`] 读取,缺失即 fail-closed(装配错误,不可放行)。
//!
//! 请求体是生成的混合宽松类型(核心字段 optional + `#[serde(flatten)]`
//! 扩展字段),经 `to_body` 序列化回 `Value` 进管线——管线全程以 Value 树
//! 为枢纽(roadmap 决策 3),生成层只负责入口校验与路由。
//! 响应方向:管线产出完整动态 `Response`(SSE 流、`X-Request-Id`、
//! `x-aikun-cache`、厂商格式错误体),生成枚举的 typed 变体装不下,一律走
//! wrapper 注入的 `Raw` 变体透传。

use axum::{
    Json,
    extract::Request,
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::{Value, json};

use aikun_openapi::openapi::server::api::ProxyApi;
use aikun_openapi::openapi::server::errors::{
    CallMcpServerResponse, CreateChatCompletionResponse, CreateEmbeddingResponse,
    CreateImageResponse, CreateMessageResponse, CreateModerationResponse, CreateRerankResponse,
    CreateResponseResponse, ListMcpServersResponse, ListModelsResponse,
};
use aikun_openapi::openapi::types::{
    ChatCompletionRequest, EmbeddingsRequest, ImagesRequest, ListMcpServersResult, McpRequest,
    McpServerInfo, MessagesRequest, ModelList, ModelListDataItem, ModelListDataItemObject,
    ModelListObject, ModerationsRequest, RerankRequest, ResponsesRequest,
};
use aikun_protocol::Protocol;

use crate::AppState;
use crate::auth::{AuthContext, Claims};
use crate::channel::selector::list_available_models_async;
use crate::pipeline::admission::model_allowed;
use crate::pipeline::{passthrough_completion, proxy_completion};

/// 一次 /v1 请求的装配层上下文。
#[derive(Clone)]
pub struct RelayCtx {
    pub claims: Claims,
    pub auth_ctx: AuthContext,
    pub headers: axum::http::HeaderMap,
}

tokio::task_local! {
    static RELAY_CTX: RelayCtx;
}

/// 鉴权上下文 task-local 注入中间件(main.rs 挂在生成 proxy 路由上,
/// 位于 auth_middleware 之后):读取 extensions 里的 `Claims` /
/// `AuthContext` 放进 task-local 再执行下游;缺失(装配错误)fail-closed
/// 401(网关扁平体,与 /v1 鉴权失败口径一致)。
pub async fn relay_scope(req: Request, next: Next) -> Response {
    let (Some(claims), Some(auth_ctx)) = (
        req.extensions().get::<Claims>().cloned(),
        req.extensions().get::<AuthContext>().cloned(),
    ) else {
        return unauthorized();
    };
    let headers = req.headers().clone();
    RELAY_CTX
        .scope(
            RelayCtx {
                claims,
                auth_ctx,
                headers,
            },
            next.run(req),
        )
        .await
}

/// impl 内取当前请求上下文;task-local 缺失(`Err(())`,未走 `relay_scope`,
/// 装配错误)时调用方以 [`unauthorized`] fail-closed 401。错误分支用 `()`
/// 而非 `Response`(clippy result_large_err:Response 体积大,不该当错误值
/// 携带)。
fn relay_ctx() -> Result<RelayCtx, ()> {
    RELAY_CTX.try_with(|ctx| ctx.clone()).map_err(|_| ())
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "unauthorized", "message": "Not authenticated"})),
    )
        .into_response()
}

/// 生成校验链的 problem+json 错误体 → OpenAI 厂商格式改写中间件。
///
/// 生成 handler 对非法 JSON(400)/schema 校验失败(422,混合宽松 schema 下
/// 仅真畸形触发)/media-type 不符(415)/body 超 2 MiB(413)输出 RFC 9457
/// problem+json;`/v1` 客户端是厂商 SDK,错误体须为
/// `{"error":{message,type,param,code}}` 形状。管线自产错误(厂商格式)与
/// auth_middleware 401(扁平体)的 Content-Type 均非 problem+json,原样放行。
/// 422 归一为 400(厂商 invalid_request_error 语义;手写时代无 422 路径)。
pub async fn problem_to_vendor_error(req: Request, next: Next) -> Response {
    let resp = next.run(req).await;
    let is_problem = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/problem+json"));
    if !is_problem {
        return resp;
    }
    let status = resp.status();
    let status = if status == StatusCode::UNPROCESSABLE_ENTITY {
        StatusCode::BAD_REQUEST
    } else {
        status
    };
    // 错误体很小;上限只是防御异常路径的内存放大。
    let bytes = match axum::body::to_bytes(resp.into_body(), 64 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return vendor_error(status, "invalid_request_error", None);
        }
    };
    let problem: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let code = problem["code"].as_str().unwrap_or("invalid_request");
    let message = problem["errors"][0]["message"]
        .as_str()
        .or_else(|| problem["title"].as_str())
        .unwrap_or("Invalid request");
    vendor_error(status, "invalid_request_error", Some((code, message)))
}

pub(crate) fn vendor_error(
    status: StatusCode,
    err_type: &str,
    code_msg: Option<(&str, &str)>,
) -> Response {
    let (code, message) = code_msg
        .map(|(c, m)| (c.to_string(), m.to_string()))
        .unwrap_or_else(|| (err_type.to_string(), "Invalid request".to_string()));
    let mut resp = (
        status,
        Json(json!({
            "error": {"message": message, "type": err_type, "param": null, "code": code}
        })),
    )
        .into_response();
    // IntoResponse 的 Json 已带 application/json;显式覆写以防未来变化。
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

/// 生成的混合宽松请求类型 → 管线枢纽的 `Value`(flatten 摊回扩展字段)。
/// 类型只含可序列化成员,失败仅理论可能;兜底 Null 会被管线按缺 model
/// 拒绝(400 invalid_request_error),不静默放行。
fn to_body<T: Serialize>(body: T) -> Value {
    serde_json::to_value(body).unwrap_or(Value::Null)
}

/// 代理面服务(装配层实现生成 trait)。Clone 供生成 router 的
/// `.with_state(api)` per-request 提取(与管理面 facade 同一模式)。
#[derive(Clone)]
pub struct RelayService(pub AppState);

#[async_trait::async_trait]
impl ProxyApi for RelayService {
    async fn create_chat_completion(
        &self,
        body: ChatCompletionRequest,
    ) -> CreateChatCompletionResponse {
        CreateChatCompletionResponse::Raw(self.proxy(Protocol::OpenAi, to_body(body)).await)
    }

    async fn create_message(&self, body: MessagesRequest) -> CreateMessageResponse {
        CreateMessageResponse::Raw(self.proxy(Protocol::Anthropic, to_body(body)).await)
    }

    async fn create_response(&self, body: ResponsesRequest) -> CreateResponseResponse {
        CreateResponseResponse::Raw(self.proxy(Protocol::Responses, to_body(body)).await)
    }

    async fn list_models(&self) -> ListModelsResponse {
        let Ok(ctx) = relay_ctx() else {
            return ListModelsResponse::Raw(unauthorized());
        };
        let models = list_available_models_async(self.0.store.clone()).await;
        let data: Vec<ModelListDataItem> = models
            .into_iter()
            .filter(|id| model_allowed(&ctx.auth_ctx.allowed_models, id))
            .map(|id| ModelListDataItem {
                id,
                object: ModelListDataItemObject::Model,
                created: Some(chrono::Utc::now().timestamp()),
                owned_by: Some("aikun".to_string()),
            })
            .collect();
        ListModelsResponse::Raw(
            (
                StatusCode::OK,
                Json(ModelList {
                    object: ModelListObject::List,
                    data,
                }),
            )
                .into_response(),
        )
    }

    async fn create_embedding(&self, body: EmbeddingsRequest) -> CreateEmbeddingResponse {
        CreateEmbeddingResponse::Raw(self.passthrough("embeddings", to_body(body)).await)
    }

    async fn create_moderation(&self, body: ModerationsRequest) -> CreateModerationResponse {
        CreateModerationResponse::Raw(self.passthrough("moderations", to_body(body)).await)
    }

    async fn create_image(&self, body: ImagesRequest) -> CreateImageResponse {
        CreateImageResponse::Raw(self.passthrough("images/generations", to_body(body)).await)
    }

    async fn create_rerank(&self, body: RerankRequest) -> CreateRerankResponse {
        CreateRerankResponse::Raw(self.passthrough("rerank", to_body(body)).await)
    }

    async fn list_mcp_servers(&self) -> ListMcpServersResponse {
        if relay_ctx().is_err() {
            return ListMcpServersResponse::Raw(unauthorized());
        }
        ListMcpServersResponse::Ok(ListMcpServersResult {
            servers: aikun_channels::adapters::zcode::MCP_SERVERS
                .iter()
                .map(|(key, description)| McpServerInfo {
                    key: (*key).to_string(),
                    description: (*description).to_string(),
                })
                .collect(),
        })
    }

    async fn call_mcp_server(&self, server_key: String, body: McpRequest) -> CallMcpServerResponse {
        match relay_ctx() {
            Ok(ctx) => CallMcpServerResponse::Raw(
                crate::mcp::call_mcp(&self.0, &ctx, &server_key, to_body(body)).await,
            ),
            Err(()) => CallMcpServerResponse::Raw(unauthorized()),
        }
    }
}

impl RelayService {
    async fn proxy(&self, protocol: Protocol, body: Value) -> Response {
        match relay_ctx() {
            Ok(ctx) => {
                proxy_completion(
                    self.0.clone(),
                    ctx.claims,
                    ctx.auth_ctx,
                    protocol,
                    body,
                    &ctx.headers,
                )
                .await
            }
            Err(()) => unauthorized(),
        }
    }

    async fn passthrough(&self, path: &str, body: Value) -> Response {
        match relay_ctx() {
            Ok(ctx) => {
                passthrough_completion(
                    self.0.clone(),
                    ctx.claims,
                    ctx.auth_ctx,
                    path.to_string(),
                    body,
                    &ctx.headers,
                )
                .await
            }
            Err(()) => unauthorized(),
        }
    }
}
