//! 代理池业务(渠道出网代理目录:socks5/http)。
//!
//! 渠道的 proxy_url 是**拷贝值**——从池里选用时把 URL 抄进渠道配置,
//! 删除池条目不影响存量渠道。传输层(reqwest socks feature)天然支持
//! socks5:// / socks5h:// / http:// / https://。

use aikun_openapi::openapi::server::errors::{
    AdminCreateProxyResponse, AdminDeleteProxyResponse, AdminListProxiesResponse,
};
use aikun_openapi::openapi::types::{CreateProxyRequest, ProxyPoolItem, ProxyPoolPage};
use aikun_store::surreal::channels_store::NewProxyRow;

use super::AdminProvidersService;
use crate::problem;

/// 合法代理 scheme(socks5h = 远端解析域名)。
const ALLOWED_SCHEMES: [&str; 4] = ["socks5://", "socks5h://", "http://", "https://"];

impl AdminProvidersService {
    /// `GET /api/proxies`——池全量(名称序)。
    pub async fn admin_list_proxies(&self) -> AdminListProxiesResponse {
        match self.store.list_proxies().await {
            Ok(rows) => AdminListProxiesResponse::Ok(ProxyPoolPage {
                items: rows
                    .into_iter()
                    .map(|r| ProxyPoolItem {
                        id: r.id,
                        name: r.name,
                        url: r.url,
                        note: r.note,
                    })
                    .collect(),
            }),
            Err(()) => {
                tracing::error!("list proxies: query failed");
                AdminListProxiesResponse::InternalServerError(problem(
                    500,
                    "query_failed",
                    "查询代理列表失败",
                ))
            }
        }
    }

    /// `POST /api/proxies`——新增(scheme 白名单校验)。
    pub async fn admin_create_proxy(&self, body: CreateProxyRequest) -> AdminCreateProxyResponse {
        let name = body.name.trim().to_string();
        let url = body.url.trim().to_string();
        if name.is_empty() || url.is_empty() {
            return AdminCreateProxyResponse::BadRequest(problem(
                400,
                "invalid_request",
                "name 与 url 必填",
            ));
        }
        if !ALLOWED_SCHEMES.iter().any(|s| url.starts_with(s)) {
            return AdminCreateProxyResponse::BadRequest(problem(
                400,
                "invalid_scheme",
                "url 必须以 socks5:// socks5h:// http:// https:// 开头",
            ));
        }
        match self
            .store
            .create_proxy(NewProxyRow {
                name,
                url,
                note: body.note.unwrap_or_default(),
            })
            .await
        {
            Ok(r) => AdminCreateProxyResponse::Created(ProxyPoolItem {
                id: r.id,
                name: r.name,
                url: r.url,
                note: r.note,
            }),
            Err(()) => {
                AdminCreateProxyResponse::BadRequest(problem(502, "store_error", "代理写入失败"))
            }
        }
    }

    /// `DELETE /api/proxies/{id}`。
    pub async fn admin_delete_proxy(&self, id: String) -> AdminDeleteProxyResponse {
        match self.store.delete_proxy(&id).await {
            Ok(true) => AdminDeleteProxyResponse::NoContent,
            Ok(false) => {
                AdminDeleteProxyResponse::NotFound(problem(404, "not_found", "代理不存在"))
            }
            Err(()) => {
                AdminDeleteProxyResponse::NotFound(problem(502, "store_error", "代理删除失败"))
            }
        }
    }
}
