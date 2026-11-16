//! Admin tag · providers 方法组(渠道 CRUD / 批量 / 测试 / model-health /
//! channel-kinds)实现桩。
//!
//! **指派:并行工人·Providers**。当前是 todo!() 桩 —— Auth 试点批只预置
//! 轨道;Providers 批填充方法体。本文件只写**固有方法**(签名 = trait 方法
//! 去掉 self 的参数,返回同名 Response 枚举);`impl AdminApi for
//! AdminService` 的委托在 lib.rs,由 `AdminService.providers` 字段路由到
//! 这里,工人不要改 lib.rs。装配与删旧由集成工人统一做;纪律与写法见
//! docs/migration-playbook.md。**不要改本 crate 的 lib.rs 与其他 impl 文件。**
// 桩文件:todo!() 体不消费参数;工人填充时删除本 allow。
#![allow(unused_variables, unused_imports)]

use aikun_openapi::openapi::server::errors::*;
use aikun_openapi::openapi::types::*;
/// providers 方法组服务。字段与构造函数由 Providers 批工人按需定义
/// (过渡期依赖经端口 trait 注入,参照 auth.rs 的 AuthStore 模式;
/// 适配器写在 main.rs,由集成工人装配)。
#[derive(Clone, Default)]
pub struct AdminProvidersService;

impl AdminProvidersService {
    /// 创建渠道(上游配置 + 加密 key)
    ///
    /// `POST /api/providers`
    pub async fn admin_create_provider(
        &self,
        body: CreateProviderRequest,
    ) -> AdminCreateProviderResponse {
        todo!("Providers 批:POST /api/providers(旧 POST /api/admin/providers)")
    }

    /// 同配置多 key 批量建渠道(key ↔ 账号 1:1)
    ///
    /// `POST /api/providers/batch`
    pub async fn admin_create_providers_batch(
        &self,
        body: CreateProvidersBatchRequest,
    ) -> AdminCreateProvidersBatchResponse {
        todo!("Providers 批:POST /api/providers/batch(状态码统一 200,api-mapping #10)")
    }

    /// 物理删除渠道
    ///
    /// `DELETE /api/providers/{id}`
    pub async fn admin_delete_provider(&self, id: String) -> AdminDeleteProviderResponse {
        todo!("Providers 批:DELETE /api/providers/{{id}}(200 {{message}} → 204)")
    }

    /// 复制渠道(创建一个副本)
    ///
    /// `POST /api/providers/{id}/duplicates`
    pub async fn admin_duplicate_provider(&self, id: String) -> AdminDuplicateProviderResponse {
        todo!("Providers 批:POST /api/providers/{{id}}/duplicates(旧 /duplicate)")
    }

    /// 为(尚未保存的)渠道配置拉取上游模型列表
    ///
    /// `POST /api/providers/fetch-models`
    pub async fn admin_fetch_upstream_models(
        &self,
        body: FetchModelsRequest,
    ) -> AdminFetchUpstreamModelsResponse {
        todo!("Providers 批:POST /api/providers/fetch-models(保留 POST 动作端点)")
    }

    /// 渠道详情
    ///
    /// `GET /api/providers/{id}`
    pub async fn admin_get_provider(&self, id: String) -> AdminGetProviderResponse {
        todo!("Providers 批:GET /api/providers/{{id}}(旧 GET /api/admin/providers/{{id}})")
    }

    /// 渠道适配器类型下拉(目录)
    ///
    /// `GET /api/channel-kinds`
    pub async fn admin_list_channel_kinds(&self) -> AdminListChannelKindsResponse {
        todo!("Providers 批:GET /api/channel-kinds(数据源 aikun-channels 注册表)")
    }

    /// 每(渠道, 模型)对的最近实测健康
    ///
    /// `GET /api/model-health`
    pub async fn admin_list_model_health(
        &self,
        limit: ::std::option::Option<i64>,
        offset: ::std::option::Option<i64>,
        provider_id: ::std::option::Option<String>,
    ) -> AdminListModelHealthResponse {
        todo!("Providers 批:GET /api/model-health({{items,total}} + ?provider_id= 过滤)")
    }

    /// 渠道列表
    ///
    /// `GET /api/providers`
    pub async fn admin_list_providers(
        &self,
        limit: ::std::option::Option<i64>,
        offset: ::std::option::Option<i64>,
    ) -> AdminListProvidersResponse {
        todo!("Providers 批:GET /api/providers({{items,total}} + 分页;proxy_url 脱敏)")
    }

    /// 渠道连通性测试(创建一次探测)
    ///
    /// `POST /api/providers/{id}/tests`
    pub async fn admin_test_provider(&self, id: String) -> AdminTestProviderResponse {
        todo!("Providers 批:POST /api/providers/{{id}}/tests(旧 /test;200 → 201)")
    }

    /// 对渠道 + 模型发真实对话请求(端到端验证,可流式)
    ///
    /// `POST /api/providers/{id}/model-tests`
    pub async fn admin_test_provider_model(
        &self,
        id: String,
        body: ModelTestRequest,
    ) -> AdminTestProviderModelResponse {
        todo!("Providers 批:POST /api/providers/{{id}}/model-tests(旧 /test-model;200 → 201;SSE)")
    }

    /// 部分更新渠道
    ///
    /// `PATCH /api/providers/{id}`
    pub async fn admin_update_provider(
        &self,
        id: String,
        body: UpdateProviderRequest,
    ) -> AdminUpdateProviderResponse {
        todo!("Providers 批:PATCH /api/providers/{{id}}({{message:updated}} → 返回资源)")
    }
}
