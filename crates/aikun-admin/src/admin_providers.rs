//! Admin tag · providers 方法组(渠道 CRUD / 批量 / 探测 / model-health /
//! channel-kinds)。业务逻辑迁自 `aikun/src/api/admin/providers.rs`(旧
//! handler 由集成工人随删旧统一处理),对齐 `openapi.yaml` v0.5.0 新契约:
//! 路径去 `/admin` 段、列表 `{items,total}` + 分页、PATCH 回资源、DELETE
//! 204、批量统一 200、problem+json、时间出参 RFC3339。
//!
//! # 装配契约(main.rs 侧,见 docs/migration-playbook.md §五)
//!
//! * **401/403 在中间件**:本组全部端点由集成工人包 `auth_middleware` +
//!   `require_admin`(与旧 `/api/admin/**` 同一守护),错误体按新 spec 用
//!   problem+json(401 invalid_token / unauthorized,403 forbidden)。
//!   impl 内不产 401/403,脱敏口径(不泄漏资源存在性)由中间件承担。
//! * **响应枚举缺 5xx 变体的端点**(create / batch / update / delete /
//!   duplicate / test / model-tests / fetch-models):DB 写失败的旧 500
//!   语义无法表达。过渡折衷:**fail-closed** 映射到最近的 4xx 变体 +
//!   `tracing::error!` 留痕(spec 缺口记入批次 notes,待裁决补 5xx 后
//!   重生成)。仅列表端点(providers / model-health)枚举自带
//!   InternalServerError,保持旧 500 行为。
//! * **SSE(model-tests stream=true)**:生成枚举只提供
//!   `CreatedStream(Sse<ServerEventStream>)` 骨架;事件内容由
//!   [`ProviderProbes::probe_model_stream`] 的适配器手工产出(事件契约
//!   见该方法文档;architecture-v0.5 §三.4:管理面 SSE 事件内容永不类型化)。
//! * **PATCH 的读-改-写不再是同一把写锁**:旧实现在持写锁全程内完成
//!   "读库存 → 校验最终状态 → UPDATE";端口化后为三段调用,并发 PATCH
//!   同一渠道存在校验窗口(管理端低频操作,风险可接受;阶段 3 store
//!   收编时可回归事务语义)。
//!
//! # 过渡期端口(aikun-store 就位前)
//!
//! aikun-admin 不依赖 rusqlite / reqwest / uuid / sha2(依赖纯净度,
//! architecture §三):DB 行级读写、key 加密/哈希、上游 HTTP 探测分别经
//! [`ProviderStore`] / [`ProviderProbes`] 端口注入,适配器写在 main.rs
//! (参照 Auth 批的 `DbAuthStore` 模式),由集成工人装配;阶段 3 store
//! 收编时适配器整体删除。渠道域类型(Provider 行、协议校验、base_url
//! 互为回退、适配器注册表)走 `aikun-channels`。

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use axum::response::sse::Sse;
use serde_json::Value;

use aikun_channels::Provider as ChannelRow;
use aikun_channels::{
    channel_protocol, channel_protocols, first_defaultable_protocol, has_defaultable_protocol,
    pick_base_url, valid_default_protocol, valid_protocol_list,
};
use aikun_openapi::openapi::server::errors::{
    AdminCreateProviderResponse, AdminCreateProvidersBatchResponse, AdminDeleteProviderResponse,
    AdminDuplicateProviderResponse, AdminFetchUpstreamModelsResponse, AdminGetProviderResponse,
    AdminListChannelKindsResponse, AdminListModelHealthResponse, AdminListProvidersResponse,
    AdminTestProviderModelResponse, AdminTestProviderResponse, AdminUpdateProviderResponse,
    ServerEventStream,
};
use aikun_openapi::openapi::types::{
    BatchFailureItem, ChannelKind as ApiChannelKind, ChannelKindPage, CreateProviderRequest,
    CreateProvidersBatchRequest, FetchModelsRequest, FetchedModels, ModelHealth as ApiModelHealth,
    ModelHealthPage, ModelHealthStatus, ModelTestRequest, ModelTestResult, Problem,
    Provider as ApiProvider, ProviderBatchResult, ProviderDefaultProtocol, ProviderHealthStatus,
    ProviderModelMapping, ProviderPage, ProviderProtocolsItem, ProviderProviderType,
    ProviderTestResult, ProviderTestResultStatus, UpdateProviderRequest,
};

use crate::problem;

// ---------- 过渡期端口 ----------

/// 建行/复制所需的全部配置列(id 由 insert 另传;name 随配置走,复制时
/// 整体换新)。`api_key_stored` 是**库内形态**(enc:v1: 密文或历史明文),
/// 明文只在请求内存中存在,永不经本结构出参。
#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    pub name: String,
    pub provider_type: String,
    pub openai_base_url: String,
    pub anthropic_base_url: String,
    pub api_key_stored: String,
    /// key 的 sha256(创建/换 key 时由调用方算好;空 key 为空串,不参与查重)。
    pub key_hash: String,
    pub models_json: String,
    pub priority: i64,
    pub weight: f64,
    pub max_retries: i64,
    pub timeout_secs: i64,
    pub proxy_url: String,
    pub model_mapping_json: String,
    pub protocols_json: String,
    pub default_protocol: String,
    pub note: String,
    pub website_url: String,
    pub channel_kind: String,
}

/// 部分更新载荷:`Some` 字段才进 SET(等价旧实现的动态 SQL);全 None =
/// 无可更新字段。数值上限校验在 impl 侧完成后才落进来。
#[derive(Debug, Clone, Default)]
pub struct ProviderPatch {
    pub name: Option<String>,
    pub provider_type: Option<String>,
    pub openai_base_url: Option<String>,
    pub anthropic_base_url: Option<String>,
    pub note: Option<String>,
    pub website_url: Option<String>,
    pub channel_kind: Option<String>,
    /// 密文与哈希成对更新(换 key)。
    pub api_key_stored: Option<String>,
    pub key_hash: Option<String>,
    pub models_json: Option<String>,
    pub priority: Option<i64>,
    pub weight: Option<f64>,
    /// Some(true) 时适配器同时清 disabled_reason / consecutive_failures
    /// (手动重启用,旧实现语义)。
    pub is_active: Option<bool>,
    pub max_retries: Option<i64>,
    pub timeout_secs: Option<i64>,
    /// 显式传入即更新,空串 = 清除代理。
    pub proxy_url: Option<String>,
    pub model_mapping_json: Option<String>,
    pub protocols_json: Option<String>,
    pub default_protocol: Option<String>,
}

impl ProviderPatch {
    /// 是否携带任一可更新字段(旧实现 `updates.is_empty()` 的等价判断)。
    fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.provider_type.is_none()
            && self.openai_base_url.is_none()
            && self.anthropic_base_url.is_none()
            && self.note.is_none()
            && self.website_url.is_none()
            && self.channel_kind.is_none()
            && self.api_key_stored.is_none()
            && self.key_hash.is_none()
            && self.models_json.is_none()
            && self.priority.is_none()
            && self.weight.is_none()
            && self.is_active.is_none()
            && self.max_retries.is_none()
            && self.timeout_secs.is_none()
            && self.proxy_url.is_none()
            && self.model_mapping_json.is_none()
            && self.protocols_json.is_none()
            && self.default_protocol.is_none()
    }
}

/// model_health 一行(checked_at 为库内原文,SQLite datetime 或 RFC3339)。
pub struct ModelHealthRow {
    pub provider_id: String,
    pub model: String,
    pub status: String,
    pub latency_ms: f64,
    pub error: String,
    pub checked_at: Option<String>,
}

/// providers / model_health 行级数据端口 + key 加密原语。
///
/// `Err(())` = 存储不可用(连接锁中毒 / SQL 失败),细节不进端口,
/// 可观测性由 impl 侧 `tracing` 负责;"行不存在"用 `Ok(None)` /
/// 受影响行数 0 表达。适配器应把持锁操作派发到 blocking 线程池。
#[async_trait]
pub trait ProviderStore: Send + Sync + 'static {
    /// 新渠道 ID(uuid v4)。
    fn new_provider_id(&self) -> String;
    /// api_key 入库静态加密(enc:v1: 前缀,随机 nonce——同明文两次密文不同)。
    fn encrypt_key(&self, plaintext: &str) -> String;
    /// 出库解密:历史明文原样返回;密文用当前密钥解不开(多半换了
    /// AIKUN_ENCRYPTION_KEY / JWT secret)时返回空串并留痕。
    fn decrypt_key(&self, stored: &str) -> String;
    /// key 的 sha256 hex(重复检测;与 api key 哈希同算法)。
    fn hash_key(&self, key: &str) -> String;

    /// 渠道列表页:`ORDER BY priority DESC, name ASC` + 总数(未分页)。
    async fn list_providers(&self, limit: i64, offset: i64) -> Result<(Vec<ChannelRow>, i64), ()>;
    /// 单行(完整列;api_key 为库内原文——密文或历史明文)。
    async fn find_provider(&self, id: &str) -> Result<Option<ChannelRow>, ()>;
    /// 复制源:读回建行所需的全部配置列(密文 key 与 key_hash 原样)。
    async fn read_provider_config(&self, id: &str) -> Result<Option<ProviderConfig>, ()>;
    /// 插入完整渠道行(运行态列走库默认)。
    async fn insert_provider(&self, id: &str, config: &ProviderConfig) -> Result<(), ()>;
    /// key_hash 归属查询:返回已使用该 hash 的渠道名;`exclude_id` 非空时
    /// 排除该渠道(换 key 场景排除自身)。
    async fn find_key_owner(&self, key_hash: &str, exclude_id: &str) -> Result<Option<String>, ()>;
    /// 部分更新(Some 字段才进 SET;顺带 `updated_at`)。返回受影响行数。
    async fn update_provider(&self, id: &str, patch: &ProviderPatch) -> Result<u64, ()>;
    /// 物理删除(`DELETE FROM providers`),返回受影响行数。
    async fn delete_provider(&self, id: &str) -> Result<u64, ()>;
    /// 健康探测写回(health_status / last_health_check)。**刻意不更新
    /// latency_ms**——ping 延迟不代表真实请求延迟,覆盖会污染选路 EMA
    /// (与后台健康检查循环同口径)。
    async fn update_health(&self, id: &str, status: &str, checked_at: &str) -> Result<(), ()>;
    /// (渠道, 模型) 实测落库:model_health upsert + 渠道 EMA 记账
    /// (与代理转发路径同口径;尽力而为,失败只留痕,不向上抛)。
    async fn record_model_health(
        &self,
        provider_id: &str,
        model: &str,
        ok: bool,
        latency_ms: f64,
        error: Option<&str>,
    );
    /// model_health 列表页(`ORDER BY provider_id, model`,分页稳定序)+ 总数。
    async fn list_model_health(
        &self,
        provider_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ModelHealthRow>, i64), ()>;
    /// 编辑态回退用:库存 (api_key 原文, proxy_url)。
    async fn find_provider_credentials(&self, id: &str) -> Result<Option<(String, String)>, ()>;
}

/// 一次连通性探测的输入(base_url 已按协议选定;api_key 为明文)。
pub struct HealthProbe {
    pub base_url: String,
    pub api_key: String,
    /// "openai" / "anthropic"。
    pub protocol: String,
    pub timeout_secs: u64,
    pub proxy_url: String,
    pub channel_kind: String,
}

/// 一次上游模型列表拉取的输入(api_key 为明文)。
pub struct FetchModelsProbe {
    pub base_url: String,
    pub api_key: String,
    pub protocol: String,
    pub proxy_url: String,
    pub channel_kind: String,
}

/// 模型列表拉取失败(impl 侧映射 502 的几种形态)。
pub enum FetchModelsError {
    /// 连接失败:`message` 已去掉 URL(reqwest without_url);host 由 impl 拼,
    /// 适配器只需带回错误描述。
    Connect { message: String },
    /// 响应体读取失败(8MB 上限 / 非 JSON)。
    ReadFailed(String),
    /// 上游非 200;`message` 已从错误体提取(无则默认文案)。
    UpstreamStatus { status: u16, message: String },
    /// 200 但没有可解析的模型列表。
    NoModels,
}

/// 单(渠道,模型)探针结果(口径同旧 `channel::model_test::ModelProbe`)。
pub struct ModelProbeOutcome {
    pub ok: bool,
    pub latency_ms: f64,
    /// 上游 HTTP 状态(传输错误为 0)。
    pub status: u16,
    pub error: Option<String>,
    pub snippet: Option<String>,
    /// 上游原始响应体(非流式)或流式汇总 {"stream":true,"events":N,"content":...}。
    pub response: Option<Value>,
}

/// 上游探测端口(HTTP;适配器复用二进制的 client 缓存、渠道适配器与
/// 传输层,包括 8MB 响应上限与流式 deadline / 缓冲上限)。
#[async_trait]
pub trait ProviderProbes: Send + Sync + 'static {
    /// 渠道连通性探测(GET {models_url};口径迁自
    /// `channel::health::check_provider_health`)。返回 (状态串
    /// healthy / degraded / unhealthy, 延迟 ms)。
    async fn check_health(&self, probe: HealthProbe) -> (String, f64);
    /// 上游模型列表拉取;URL 与鉴权头由渠道适配器决定
    /// (与对话 / 健康检查同口径)。
    async fn fetch_upstream_models(
        &self,
        probe: FetchModelsProbe,
    ) -> Result<Vec<String>, FetchModelsError>;
    /// 单(渠道,模型)真实对话探针(非流式;口径迁自
    /// `channel::model_test::probe_model`,含响应形状校验)。
    /// `provider.api_key` 已是明文。
    async fn probe_model(
        &self,
        provider: ChannelRow,
        protocol: String,
        model: String,
        messages: Vec<Value>,
        max_tokens: u32,
    ) -> ModelProbeOutcome;
    /// 流式探针 → SSE 响应(201 text/event-stream)。**事件契约**
    /// (前端 test.js 与 e2e_admin_providers.rs 依赖,适配器必须遵守):
    /// * 每个上游增量一个 `event: delta`,data 为 JSON `{"text":"..."}`;
    /// * 流结束发一个 `event: result`,data 为 ModelTestResult 形状的
    ///   JSON(ok / latency_ms / protocol / stream / snippet / status / error);
    /// * 流结束时适配器自行把结果落 model_health + EMA 记账
    ///   (与非流式路径同口径)。
    /// deadline / SSE 缓冲 / 可见文本截断上限同 probe_model 的流式分支。
    fn probe_model_stream(
        &self,
        provider: ChannelRow,
        protocol: String,
        model: String,
        messages: Vec<Value>,
        max_tokens: u32,
    ) -> Sse<ServerEventStream>;
}

// ---------- 服务 ----------

/// providers 方法组服务:业务编排(校验顺序、默认值、跨字段联动、错误
/// 映射、脱敏)全部在此;数据与网络经端口注入。
pub struct AdminProvidersService {
    store: Arc<dyn ProviderStore>,
    probes: Arc<dyn ProviderProbes>,
}

impl AdminProvidersService {
    pub fn new(store: Arc<dyn ProviderStore>, probes: Arc<dyn ProviderProbes>) -> Self {
        Self { store, probes }
    }
}

impl Clone for AdminProvidersService {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            probes: Arc::clone(&self.probes),
        }
    }
}

// ---------- 纯校验 / 映射助手 ----------

/// timeout 0 = 无超时会卡死探针与流,负值会在下游 u64 回绕——都拒绝。
pub fn valid_timeout_secs(t: i64) -> bool {
    (1..=600).contains(&t)
}

/// weight 必须有限且在 0.1..=1000:inf / NaN 会持久化并让选路
/// random_range panic(1e400 反序列化为 inf)。
pub fn valid_weight(w: f64) -> bool {
    w.is_finite() && (0.1..=1000.0).contains(&w)
}

pub fn valid_priority(p: i64) -> bool {
    (0..=100).contains(&p)
}

pub fn valid_max_retries(r: i64) -> bool {
    (0..=10).contains(&r)
}

/// channel_kind:空串(= generic)或已注册适配器(注册表在 aikun-channels)。
pub fn valid_channel_kind(k: &str) -> bool {
    aikun_channels::registry().is_valid_kind(k)
}

fn channel_kind_names() -> String {
    aikun_channels::registry()
        .list()
        .iter()
        .map(|i| i.kind)
        .collect::<Vec<_>>()
        .join(" / ")
}

/// 分页参数归一:limit 缺省 50、静默 clamp 1..=200;offset 缺省 0、下限 0
/// (spec `components/parameters/limit`:越界截断而非 400,与 0.4.x 同口径)。
fn page_args(limit: Option<i64>, offset: Option<i64>) -> (i64, i64) {
    (
        limit.unwrap_or(50).clamp(1, 200),
        offset.unwrap_or(0).max(0),
    )
}

/// proxy_url 出参脱敏:可能内嵌凭证(userinfo),只回显 scheme://host:port。
/// (迁自 `channel::transport::sanitize_proxy_url`,纯字符串操作。)
pub fn sanitize_proxy_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
            format!("{scheme}://{host}")
        }
        None => match url.rsplit_once('@') {
            Some((_, host)) => host.to_string(),
            None => url.to_string(),
        },
    }
}

/// URL 的 host 部分(含端口),用于错误消息——只回显 host,避免泄露
/// 完整 URL(可能含敏感路径/参数)。
fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    rest.split('/').next().unwrap_or(rest)
}

/// 库内 health_status → spec 枚举(词表收敛:库 "unhealthy" → API "down")。
fn api_health_status(s: &str) -> ProviderHealthStatus {
    match s {
        "healthy" => ProviderHealthStatus::Healthy,
        "degraded" => ProviderHealthStatus::Degraded,
        "unhealthy" => ProviderHealthStatus::Down,
        _ => ProviderHealthStatus::Unknown,
    }
}

fn api_test_status(s: &str) -> ProviderTestResultStatus {
    match s {
        "healthy" => ProviderTestResultStatus::Healthy,
        "degraded" => ProviderTestResultStatus::Degraded,
        "unhealthy" => ProviderTestResultStatus::Down,
        _ => ProviderTestResultStatus::Unknown,
    }
}

/// 库内 provider_type → spec 枚举。历史 azure 行(CHECK 容忍)词表已
/// 刻意排除,映射为 custom 并留痕——出参不撞枚举,前端按自定义渠道展示。
fn api_provider_type(s: &str) -> ProviderProviderType {
    match s {
        "openai" => ProviderProviderType::Openai,
        "anthropic" => ProviderProviderType::Anthropic,
        "custom" => ProviderProviderType::Custom,
        other => {
            tracing::warn!("provider_type '{other}' 不在 spec 枚举内,出参按 custom 处理");
            ProviderProviderType::Custom
        }
    }
}

fn api_default_protocol(s: &str) -> ProviderDefaultProtocol {
    if s == "anthropic" {
        ProviderDefaultProtocol::Anthropic
    } else {
        ProviderDefaultProtocol::Openai
    }
}

fn api_protocols_item(s: &str) -> ProviderProtocolsItem {
    match s {
        "anthropic" => ProviderProtocolsItem::Anthropic,
        "responses" => ProviderProtocolsItem::Responses,
        _ => ProviderProtocolsItem::Openai,
    }
}

/// 渠道行 → spec Provider 出参。api_key 永不出参;proxy_url 脱敏;
/// 时间统一 RFC3339(库内 SQLite datetime / RFC3339 双兼容)。
fn to_api_provider(row: &ChannelRow) -> ApiProvider {
    let models: Vec<String> = serde_json::from_str(&row.models).unwrap_or_default();
    let mapping: BTreeMap<String, String> =
        serde_json::from_str(&row.model_mapping).unwrap_or_default();
    let protocols: Vec<ProviderProtocolsItem> = channel_protocols(row)
        .into_iter()
        .map(|p| api_protocols_item(&p))
        .collect();
    // spec 要求 openai_base_url 为非空 uri;anthropic-only 渠道该列为空串,
    // 按"协议专属地址为空时互为回退"的文档语义回显生效地址。
    let openai_effective = if row.openai_base_url.is_empty() {
        row.anthropic_base_url.clone()
    } else {
        row.openai_base_url.clone()
    };
    let openai_url = url::Url::parse(&openai_effective).unwrap_or_else(|_| {
        // 两路地址皆空(历史脏行,创建/更新校验都不会再产出):兜底占位并留痕。
        tracing::warn!(
            "provider {} 两路 base_url 均为空,出参回退 about:blank",
            row.id
        );
        url::Url::parse("about:blank").expect("静态 URL 必可解析")
    });
    ApiProvider {
        id: row.id.clone(),
        name: row.name.clone(),
        provider_type: api_provider_type(&row.provider_type),
        openai_base_url: openai_url,
        anthropic_base_url: Some(row.anthropic_base_url.clone()),
        models,
        priority: i64::from(row.priority),
        weight: row.weight,
        is_active: row.is_active,
        health_status: api_health_status(&row.health_status),
        latency_ms: Some(row.latency_ms),
        error_rate: Some(row.error_rate),
        last_health_check: row
            .last_health_check
            .as_deref()
            .map(|s| Some(crate::sqlite_utc(s))),
        max_retries: Some(i64::from(row.max_retries)),
        timeout_secs: Some(i64::from(row.timeout_secs)),
        proxy_url: Some(sanitize_proxy_url(&row.proxy_url)),
        model_mapping: Some(ProviderModelMapping {
            additional_properties: mapping,
        }),
        disabled_reason: Some(row.disabled_reason.clone()),
        protocols: Some(protocols),
        default_protocol: Some(api_default_protocol(channel_protocol(row).as_str())),
        note: Some(row.note.clone()),
        website_url: Some(row.website_url.clone()),
        channel_kind: Some(row.channel_kind.clone()),
        created_at: crate::sqlite_utc(&row.created_at),
        updated_at: crate::sqlite_utc(&row.updated_at),
    }
}

/// 新建行的出参(运行态重置:启用 / unknown / 零延迟零错误率)。
fn fresh_row_from_config(id: &str, config: &ProviderConfig) -> ChannelRow {
    let now = chrono::Utc::now().to_rfc3339();
    ChannelRow {
        id: id.to_string(),
        name: config.name.clone(),
        provider_type: config.provider_type.clone(),
        openai_base_url: config.openai_base_url.clone(),
        anthropic_base_url: config.anthropic_base_url.clone(),
        api_key: config.api_key_stored.clone(),
        key_decrypt_failed: false,
        models: config.models_json.clone(),
        priority: config.priority.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        weight: config.weight,
        is_active: true,
        health_status: "unknown".to_string(),
        latency_ms: 0.0,
        error_rate: 0.0,
        last_health_check: None,
        max_retries: config.max_retries.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        timeout_secs: config.timeout_secs.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        created_at: now.clone(),
        updated_at: now,
        proxy_url: config.proxy_url.clone(),
        model_mapping: config.model_mapping_json.clone(),
        consecutive_failures: 0,
        disabled_reason: String::new(),
        protocols: config.protocols_json.clone(),
        default_protocol: config.default_protocol.clone(),
        note: config.note.clone(),
        website_url: config.website_url.clone(),
        channel_kind: config.channel_kind.clone(),
    }
}

/// create(单个与批量项)共用的请求形态(生成类型的两套枚举在此归一为
/// 字符串,校验逻辑只写一份)。
struct ProviderCandidate {
    name: String,
    provider_type: Option<String>,
    openai_base_url: Option<String>,
    anthropic_base_url: Option<String>,
    api_key: String,
    models: Vec<String>,
    priority: Option<i64>,
    weight: Option<f64>,
    max_retries: Option<i64>,
    timeout_secs: Option<i64>,
    proxy_url: Option<String>,
    model_mapping: BTreeMap<String, String>,
    protocols: Option<Vec<String>>,
    default_protocol: Option<String>,
    note: Option<String>,
    website_url: Option<String>,
    channel_kind: Option<String>,
}

impl ProviderCandidate {
    fn from_create(b: &CreateProviderRequest) -> Self {
        Self {
            name: b.name.clone(),
            provider_type: b.provider_type.as_ref().map(|t| t.as_str().to_string()),
            openai_base_url: b.openai_base_url.clone(),
            anthropic_base_url: b.anthropic_base_url.clone(),
            api_key: b.api_key.clone(),
            models: b.models.clone(),
            priority: b.priority,
            weight: b.weight,
            max_retries: b.max_retries,
            timeout_secs: b.timeout_secs,
            proxy_url: b.proxy_url.clone(),
            model_mapping: b
                .model_mapping
                .as_ref()
                .map(|m| m.additional_properties.clone())
                .unwrap_or_default(),
            protocols: b
                .protocols
                .as_ref()
                .map(|l| l.iter().map(|p| p.as_str().to_string()).collect()),
            default_protocol: b.default_protocol.as_ref().map(|d| d.as_str().to_string()),
            note: b.note.clone(),
            website_url: b.website_url.clone(),
            channel_kind: b.channel_kind.clone(),
        }
    }

    /// 批量项:shared 的扁平字段 + 逐 key 覆盖 api_key + 自动编号名
    /// (shared 无 api_key 字段,schema 层已隔离)。
    fn from_batch_item(
        shared: &CreateProvidersBatchRequest,
        api_key: String,
        name: String,
    ) -> Self {
        Self {
            name,
            api_key,
            provider_type: shared
                .provider_type
                .as_ref()
                .map(|t| t.as_str().to_string()),
            openai_base_url: shared.openai_base_url.clone(),
            anthropic_base_url: shared.anthropic_base_url.clone(),
            models: shared.models.clone(),
            priority: shared.priority,
            weight: shared.weight,
            max_retries: shared.max_retries,
            timeout_secs: shared.timeout_secs,
            proxy_url: shared.proxy_url.clone(),
            model_mapping: shared
                .model_mapping
                .as_ref()
                .map(|m| m.additional_properties.clone())
                .unwrap_or_default(),
            protocols: shared
                .protocols
                .as_ref()
                .map(|l| l.iter().map(|p| p.as_str().to_string()).collect()),
            default_protocol: shared
                .default_protocol
                .as_ref()
                .map(|d| d.as_str().to_string()),
            note: shared.note.clone(),
            website_url: shared.website_url.clone(),
            channel_kind: shared.channel_kind.clone(),
        }
    }
}

/// create 的校验 + 组装核心(批量复用)。校验顺序与旧 `insert_provider`
/// 一致:protocols → default_protocol → name → 两路地址(按勾选协议
/// 必填对应地址,空则回退另一路)→ 数值界 → channel_kind。
/// key 的加密 / 哈希 / 查重在调用方(需要 store);本函数产出的
/// `api_key_stored` / `key_hash` 为占位空串。
// Problem 是生成类型的统一错误载荷(响应枚举按值收),装箱只会把成本
// 挪到每个 match 点——按仓库先例(gen-api.sh)对内部辅助函数放行。
#[allow(clippy::result_large_err)]
fn validate_candidate(cand: &ProviderCandidate) -> Result<ProviderConfig, Problem> {
    let bad = |code: &str, detail: &str| problem(400, code, detail);
    // Protocols the channel can speak; default to openai-only when omitted.
    let protocols = match &cand.protocols {
        Some(list) => {
            if !valid_protocol_list(list) {
                return Err(bad(
                    "invalid_protocols",
                    "protocols 不能为空，且仅支持 openai / anthropic / responses",
                ));
            }
            if !has_defaultable_protocol(list) {
                return Err(bad(
                    "invalid_protocols",
                    "responses 为附加协议，需同时勾选 openai 或 anthropic",
                ));
            }
            list.clone()
        }
        None => vec!["openai".to_string()],
    };
    let default_protocol = match &cand.default_protocol {
        Some(d) if valid_default_protocol(d, &protocols) => d.clone(),
        Some(_) => {
            return Err(bad(
                "invalid_default_protocol",
                "default_protocol 必须是已勾选的 openai / anthropic 协议之一",
            ));
        }
        None => first_defaultable_protocol(&protocols),
    };
    // Legacy provider_type 列与默认协议保持同步,旧回退路径不漂移。
    // (白名单本身已由 schema enum openai/anthropic/custom 在生成层拦下;
    // 历史 azure 只在库中,不可能经 API 写入。)
    let provider_type = cand
        .provider_type
        .clone()
        .unwrap_or_else(|| default_protocol.clone());
    let priority = cand.priority.unwrap_or(0);
    let weight = cand.weight.unwrap_or(1.0);
    let max_retries = cand.max_retries.unwrap_or(3);
    let timeout_secs = cand.timeout_secs.unwrap_or(120);

    let name = cand.name.trim().to_string();
    if name.is_empty() {
        return Err(bad("invalid_name", "name 不能为空"));
    }
    let openai_base_url = cand
        .openai_base_url
        .as_deref()
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .to_string();
    let anthropic_base_url = cand
        .anthropic_base_url
        .as_deref()
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .to_string();
    // 勾选了哪个协议就必须填对应的上游地址;responses 复用 openai 地址。
    if protocols.iter().any(|p| p == "openai" || p == "responses") && openai_base_url.is_empty() {
        return Err(bad(
            "invalid_base_url",
            "勾选 openai / responses 协议时必须填写 openai_base_url",
        ));
    }
    if protocols.iter().any(|p| p == "anthropic") && anthropic_base_url.is_empty() {
        return Err(bad(
            "invalid_base_url",
            "勾选 anthropic 协议时必须填写 anthropic_base_url",
        ));
    }
    if !valid_priority(priority) {
        return Err(bad(
            "invalid_priority",
            "priority must be between 0 and 100",
        ));
    }
    if !valid_weight(weight) {
        return Err(bad(
            "invalid_weight",
            "weight must be finite and between 0.1 and 1000",
        ));
    }
    if !valid_max_retries(max_retries) {
        return Err(bad(
            "invalid_max_retries",
            "max_retries must be between 0 and 10",
        ));
    }
    if !valid_timeout_secs(timeout_secs) {
        return Err(bad(
            "invalid_timeout",
            "timeout_secs must be between 1 and 600",
        ));
    }
    let channel_kind = cand
        .channel_kind
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_string();
    if !valid_channel_kind(&channel_kind) {
        return Err(bad(
            "invalid_channel_kind",
            &format!("channel_kind 仅支持: {}", channel_kind_names()),
        ));
    }
    Ok(ProviderConfig {
        name,
        provider_type,
        openai_base_url,
        anthropic_base_url,
        api_key_stored: String::new(),
        key_hash: String::new(),
        models_json: serde_json::to_string(&cand.models).unwrap_or_else(|_| "[]".to_string()),
        priority,
        weight,
        max_retries,
        timeout_secs,
        proxy_url: cand.proxy_url.clone().unwrap_or_default(),
        model_mapping_json: serde_json::to_string(&cand.model_mapping)
            .unwrap_or_else(|_| "{}".to_string()),
        protocols_json: serde_json::to_string(&protocols)
            .unwrap_or_else(|_| "[\"openai\"]".to_string()),
        default_protocol,
        note: cand.note.as_deref().unwrap_or_default().trim().to_string(),
        website_url: cand
            .website_url
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string(),
        channel_kind,
    })
}

/// create / 批量项共用的"加密 + 查重 + 落库"尾段。Ok(新行) / Err(problem)。
/// 查重查询失败按"无重复"处理并留痕(旧实现 `.ok()` 语义)。
#[allow(clippy::result_large_err)]
async fn insert_checked(
    store: &dyn ProviderStore,
    cand: &ProviderCandidate,
    mut config: ProviderConfig,
) -> Result<ChannelRow, Problem> {
    // 静态加密落库(enc:v1:);key 的 sha256 用于重复检测——密文每次随机
    // nonce 无法比对,哈希可以。空 key 不参与(空串哈希会让所有无 key
    // 渠道互相误判重复)。
    config.api_key_stored = store.encrypt_key(&cand.api_key);
    config.key_hash = if cand.api_key.is_empty() {
        String::new()
    } else {
        store.hash_key(&cand.api_key)
    };
    if !config.key_hash.is_empty() {
        match store.find_key_owner(&config.key_hash, "").await {
            Ok(Some(owner)) => {
                return Err(problem(
                    409,
                    "duplicate_key",
                    &format!(
                        "该 key 已用于渠道「{owner}」;同一 key 建多个渠道会失去 key↔账号的对应关系"
                    ),
                ));
            }
            Ok(None) => {}
            Err(()) => tracing::error!("create provider: key-hash lookup failed"),
        }
    }
    let id = store.new_provider_id();
    match store.insert_provider(&id, &config).await {
        Ok(()) => Ok(fresh_row_from_config(&id, &config)),
        Err(()) => {
            tracing::error!("create provider: insert failed");
            // 响应枚举无 5xx 变体(spec 缺口,见文件头):fail-closed 400。
            Err(problem(400, "create_failed", "Internal server error"))
        }
    }
}

// ---------- 固有方法(AdminService facade 一行委托到这) ----------

impl AdminProvidersService {
    /// 渠道列表(分页 + 总数;proxy_url 脱敏、api_key 永不出参)。
    ///
    /// `GET /api/providers`
    pub async fn admin_list_providers(
        &self,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> AdminListProvidersResponse {
        let (limit, offset) = page_args(limit, offset);
        match self.store.list_providers(limit, offset).await {
            Ok((rows, total)) => AdminListProvidersResponse::Ok(ProviderPage {
                items: rows.iter().map(to_api_provider).collect(),
                total,
            }),
            // 查询失败报 500 而不是空列表——DB 故障不能伪装成"没有渠道"。
            Err(()) => {
                tracing::error!("list providers: query failed");
                AdminListProvidersResponse::InternalServerError(problem(
                    500,
                    "query_failed",
                    "查询渠道列表失败",
                ))
            }
        }
    }

    /// 创建渠道(201 保留;全部跨字段校验与 409 duplicate_key 保留)。
    ///
    /// `POST /api/providers`
    pub async fn admin_create_provider(
        &self,
        body: CreateProviderRequest,
    ) -> AdminCreateProviderResponse {
        let cand = ProviderCandidate::from_create(&body);
        let config = match validate_candidate(&cand) {
            Ok(c) => c,
            Err(p) => return AdminCreateProviderResponse::BadRequest(p),
        };
        match insert_checked(self.store.as_ref(), &cand, config).await {
            Ok(row) => AdminCreateProviderResponse::Created(to_api_provider(&row)),
            Err(p) => match p.status {
                409 => AdminCreateProviderResponse::Conflict(p),
                _ => AdminCreateProviderResponse::BadRequest(p),
            },
        }
    }

    /// 同配置多 key 批量建渠道(key↔账号 1:1)。**状态码统一 200**
    /// (旧:全败 400 / 部分 201),成败全在 body;失败项 `error` → `code`
    /// (api-mapping #10)。逐 key 独立校验插入,单项失败不影响其余;
    /// 批内重复 key 归失败组。
    ///
    /// `POST /api/providers/batch`
    pub async fn admin_create_providers_batch(
        &self,
        body: CreateProvidersBatchRequest,
    ) -> AdminCreateProvidersBatchResponse {
        let bad = |code: &str, detail: &str| {
            AdminCreateProvidersBatchResponse::BadRequest(problem(400, code, detail))
        };
        let keys: Vec<String> = body
            .api_keys
            .iter()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect();
        if keys.is_empty() || keys.len() > 100 {
            return bad("invalid_api_keys", "api_keys 须为 1..=100 个非空 key");
        }
        let base_name = body.name.trim().to_string();
        if base_name.is_empty() {
            return bad("invalid_name", "name 不能为空");
        }
        let mut created: Vec<ChannelRow> = Vec::new();
        let mut failed: Vec<BatchFailureItem> = Vec::new();
        // 批内去重:同一 key 建出两个配置完全相同的渠道会破坏 key↔账号的
        // 1:1 约定(同 key 双倍参与选路/计并发),重复的归入失败组。
        let mut seen: HashSet<String> = HashSet::new();
        for (i, key) in keys.iter().enumerate() {
            if !seen.insert(key.clone()) {
                failed.push(BatchFailureItem {
                    index: i as i64,
                    status: 400,
                    code: "duplicate_key".to_string(),
                    message: Some("批次内重复的 key,已跳过".to_string()),
                });
                continue;
            }
            let cand = ProviderCandidate::from_batch_item(
                &body,
                key.clone(),
                format!("{}-{:02}", base_name, i + 1),
            );
            match validate_candidate(&cand) {
                Err(p) => failed.push(BatchFailureItem {
                    index: i as i64,
                    status: p.status,
                    code: p.code.clone(),
                    message: p.detail.clone(),
                }),
                Ok(config) => match insert_checked(self.store.as_ref(), &cand, config).await {
                    Ok(row) => created.push(row),
                    Err(p) => failed.push(BatchFailureItem {
                        index: i as i64,
                        status: p.status,
                        code: p.code.clone(),
                        message: p.detail.clone(),
                    }),
                },
            }
        }
        AdminCreateProvidersBatchResponse::Ok(ProviderBatchResult {
            created: created.len() as i64,
            failed,
            providers: created.iter().map(to_api_provider).collect(),
        })
    }

    /// 渠道详情(proxy_url 脱敏)。查不到 / 存储不可用统一 404(旧口径:
    /// 查询错误也按 404,不泄漏内部状态)。
    ///
    /// `GET /api/providers/{id}`
    pub async fn admin_get_provider(&self, id: String) -> AdminGetProviderResponse {
        match self.store.find_provider(&id).await {
            Ok(Some(row)) => AdminGetProviderResponse::Ok(to_api_provider(&row)),
            _ => {
                tracing::error!("get provider {id}: not found or store unavailable");
                AdminGetProviderResponse::NotFound(problem(404, "not_found", "资源不存在"))
            }
        }
    }

    /// 部分更新渠道。**成功返回更新后的资源**(旧 `{message:'updated'}`)。
    /// 校验顺序与旧实现一致;protocols / default_protocol 对库存值解析后
    /// 按**最终状态**联动校验(防止造出必失败渠道);proxy_url 空串 =
    /// 清除代理;is_active=true 顺带清自动禁用状态(适配器侧)。
    ///
    /// `PATCH /api/providers/{id}`
    pub async fn admin_update_provider(
        &self,
        provider_id: String,
        body: UpdateProviderRequest,
    ) -> AdminUpdateProviderResponse {
        let bad = |code: &str, detail: &str| {
            AdminUpdateProviderResponse::BadRequest(problem(400, code, detail))
        };
        let not_found =
            || AdminUpdateProviderResponse::NotFound(problem(404, "not_found", "资源不存在"));

        let mut patch = ProviderPatch::default();
        if let Some(name) = &body.name {
            let name = name.trim();
            if name.is_empty() {
                return bad("invalid_name", "name 不能为空");
            }
            patch.name = Some(name.to_string());
        }
        // provider_type 白名单由 schema enum 拦下;此处只透传。
        if let Some(t) = &body.provider_type {
            patch.provider_type = Some(t.as_str().to_string());
        }
        // 两个地址都允许更新;置空即回退到另一路(base_url_for 回退口径)。
        if let Some(url) = &body.openai_base_url {
            patch.openai_base_url = Some(url.trim().trim_end_matches('/').to_string());
        }
        if let Some(url) = &body.anthropic_base_url {
            patch.anthropic_base_url = Some(url.trim().trim_end_matches('/').to_string());
        }
        if let Some(note) = &body.note {
            patch.note = Some(note.trim().to_string());
        }
        if let Some(website_url) = &body.website_url {
            patch.website_url = Some(website_url.trim().to_string());
        }
        if let Some(kind) = &body.channel_kind {
            let kind = kind.trim().to_string();
            if !valid_channel_kind(&kind) {
                return bad(
                    "invalid_channel_kind",
                    &format!("channel_kind 仅支持: {}", channel_kind_names()),
                );
            }
            patch.channel_kind = Some(kind);
        }
        if let Some(key) = &body.api_key {
            // 换 key:排除自身做重复检测;空 key 哈希置空(退出查重)。
            let key_hash = if key.is_empty() {
                String::new()
            } else {
                self.store.hash_key(key)
            };
            if !key_hash.is_empty() {
                match self.store.find_key_owner(&key_hash, &provider_id).await {
                    Ok(Some(owner)) => {
                        return AdminUpdateProviderResponse::Conflict(problem(
                            409,
                            "duplicate_key",
                            &format!("该 key 已用于渠道「{owner}」"),
                        ));
                    }
                    Ok(None) => {}
                    Err(()) => tracing::error!("update provider: key-hash lookup failed"),
                }
            }
            patch.api_key_stored = Some(self.store.encrypt_key(key));
            patch.key_hash = Some(key_hash);
        }
        if let Some(models) = &body.models {
            patch.models_json =
                Some(serde_json::to_string(models).unwrap_or_else(|_| "[]".to_string()));
        }
        if let Some(priority) = body.priority {
            if !valid_priority(priority) {
                return bad("invalid_priority", "priority must be between 0 and 100");
            }
            patch.priority = Some(priority);
        }
        if let Some(weight) = body.weight {
            if !valid_weight(weight) {
                return bad(
                    "invalid_weight",
                    "weight must be finite and between 0.1 and 1000",
                );
            }
            patch.weight = Some(weight);
        }
        if let Some(active) = body.is_active {
            patch.is_active = Some(active);
        }
        if let Some(retries) = body.max_retries {
            if !valid_max_retries(retries) {
                return bad(
                    "invalid_max_retries",
                    "max_retries must be between 0 and 10",
                );
            }
            patch.max_retries = Some(retries);
        }
        if let Some(timeout) = body.timeout_secs {
            if !valid_timeout_secs(timeout) {
                return bad("invalid_timeout", "timeout_secs must be between 1 and 600");
            }
            patch.timeout_secs = Some(timeout);
        }
        if let Some(proxy) = &body.proxy_url {
            // 显式传入即更新,空串 = 清除代理;字段缺失才不修改。
            patch.proxy_url = Some(proxy.trim().to_string());
        }
        if let Some(mapping) = &body.model_mapping {
            patch.model_mapping_json = Some(
                serde_json::to_string(&mapping.additional_properties)
                    .unwrap_or_else(|_| "{}".to_string()),
            );
        }

        // protocols / default_protocol / 两路地址任一被改动时,联动校验要
        // 对"库存值 + 本次改动"的最终状态进行(旧实现同):改协议列表可能
        // 使库存默认失效,反之亦然;清空地址可能使勾选协议失去上游。
        let needs_stored = body.protocols.is_some()
            || body.default_protocol.is_some()
            || body.openai_base_url.is_some()
            || body.anthropic_base_url.is_some();
        let stored = if needs_stored {
            match self.store.find_provider(&provider_id).await {
                Ok(Some(row)) => Some(row),
                _ => {
                    tracing::error!("update provider {provider_id}: not found or store error");
                    return not_found();
                }
            }
        } else {
            None
        };

        if body.protocols.is_some() || body.default_protocol.is_some() {
            let stored = stored.as_ref().expect("needs_stored 时 stored 必已读取");
            // 旧口径:库内 protocols 解析失败回退 ["openai"](legacy 空串行),
            // 不走 channel_protocols 的 provider_type 回退。
            let stored_protocols: Vec<String> = serde_json::from_str(&stored.protocols)
                .unwrap_or_else(|_| vec!["openai".to_string()]);
            let stored_default = stored.default_protocol.clone();
            let protocols: Vec<String> = match &body.protocols {
                Some(list) => {
                    let list: Vec<String> = list.iter().map(|p| p.as_str().to_string()).collect();
                    if !valid_protocol_list(&list) {
                        return bad(
                            "invalid_protocols",
                            "protocols 不能为空，且仅支持 openai / anthropic / responses",
                        );
                    }
                    if !has_defaultable_protocol(&list) {
                        return bad(
                            "invalid_protocols",
                            "responses 为附加协议，需同时勾选 openai 或 anthropic",
                        );
                    }
                    list
                }
                None => stored_protocols,
            };
            let default_protocol = match &body.default_protocol {
                Some(d) => {
                    let d = d.as_str();
                    if !valid_default_protocol(d, &protocols) {
                        return bad(
                            "invalid_default_protocol",
                            "default_protocol 必须是已勾选的 openai / anthropic 协议之一",
                        );
                    }
                    d.to_string()
                }
                // 库存默认掉出新列表:重置为第一个可作默认的协议。
                None if !protocols.contains(&stored_default) => {
                    first_defaultable_protocol(&protocols)
                }
                None => stored_default,
            };
            if body.protocols.is_some() {
                patch.protocols_json =
                    Some(serde_json::to_string(&protocols).unwrap_or_else(|_| "[]".to_string()));
            }
            patch.default_protocol = Some(default_protocol);
        }

        if body.protocols.is_some()
            || body.openai_base_url.is_some()
            || body.anthropic_base_url.is_some()
        {
            let stored = stored.as_ref().expect("needs_stored 时 stored 必已读取");
            let final_protocols: Vec<String> = match &body.protocols {
                Some(list) => list.iter().map(|p| p.as_str().to_string()).collect(),
                None => serde_json::from_str(&stored.protocols)
                    .unwrap_or_else(|_| vec!["openai".to_string()]),
            };
            let final_openai = body
                .openai_base_url
                .as_deref()
                .unwrap_or(&stored.openai_base_url)
                .trim();
            let final_anthropic = body
                .anthropic_base_url
                .as_deref()
                .unwrap_or(&stored.anthropic_base_url)
                .trim();
            // responses 复用 openai 地址,勾选其一即要求 openai_base_url。
            if final_protocols
                .iter()
                .any(|p| p == "openai" || p == "responses")
                && final_openai.is_empty()
            {
                return bad(
                    "invalid_base_url",
                    "勾选 openai / responses 协议时必须填写 openai_base_url",
                );
            }
            if final_protocols.iter().any(|p| p == "anthropic") && final_anthropic.is_empty() {
                return bad(
                    "invalid_base_url",
                    "勾选 anthropic 协议时必须填写 anthropic_base_url",
                );
            }
        }

        if patch.is_empty() {
            return bad("no_fields_to_update", "没有可更新字段");
        }

        match self.store.update_provider(&provider_id, &patch).await {
            Ok(n) if n > 0 => {}
            Ok(_) => return not_found(),
            Err(()) => {
                tracing::error!("update provider {provider_id}: execute failed");
                // 枚举无 5xx(spec 缺口,见文件头):fail-closed 400。
                return AdminUpdateProviderResponse::BadRequest(problem(
                    400,
                    "update_failed",
                    "Internal server error",
                ));
            }
        }
        // 成功后回读,返回更新后的资源。
        match self.store.find_provider(&provider_id).await {
            Ok(Some(row)) => AdminUpdateProviderResponse::Ok(to_api_provider(&row)),
            _ => {
                tracing::error!("update provider {provider_id}: re-read failed");
                not_found()
            }
        }
    }

    /// 物理删除渠道(200 `{message}` → **204**)。关联请求日志的
    /// provider_id 置 NULL(ON DELETE SET NULL),日志保留。
    ///
    /// `DELETE /api/providers/{id}`
    pub async fn admin_delete_provider(&self, id: String) -> AdminDeleteProviderResponse {
        match self.store.delete_provider(&id).await {
            Ok(n) if n > 0 => AdminDeleteProviderResponse::NoContent,
            Ok(_) => AdminDeleteProviderResponse::NotFound(problem(404, "not_found", "资源不存在")),
            Err(()) => {
                tracing::error!("delete provider {id}: execute failed");
                // 枚举无 5xx(spec 缺口,见文件头):fail-closed 404
                //(删除幂等语义下"当作已不存在"最不误导)。
                AdminDeleteProviderResponse::NotFound(problem(404, "not_found", "资源不存在"))
            }
        }
    }

    /// 复制渠道(GitHub fork 模式):配置(含密文 key)整体复制,运行态
    /// 重置(health unknown、延迟/错误率清零、启用),新名 '{} (副本)'。
    ///
    /// `POST /api/providers/{id}/duplicates`
    pub async fn admin_duplicate_provider(&self, id: String) -> AdminDuplicateProviderResponse {
        let mut config = match self.store.read_provider_config(&id).await {
            Ok(Some(c)) => c,
            _ => {
                tracing::error!("duplicate provider {id}: source not found or store error");
                return AdminDuplicateProviderResponse::NotFound(problem(
                    404,
                    "not_found",
                    "资源不存在",
                ));
            }
        };
        config.name = format!("{} (副本)", config.name);
        let new_id = self.store.new_provider_id();
        match self.store.insert_provider(&new_id, &config).await {
            Ok(()) => AdminDuplicateProviderResponse::Created(to_api_provider(
                &fresh_row_from_config(&new_id, &config),
            )),
            Err(()) => {
                tracing::error!("duplicate provider {id}: insert failed");
                // 枚举无 5xx(spec 缺口,见文件头):fail-closed 404。
                AdminDuplicateProviderResponse::NotFound(problem(404, "not_found", "资源不存在"))
            }
        }
    }

    /// 渠道连通性测试(200 → **201**,创建探测结果)。跑一次健康探测并
    /// 更新 health_status / last_health_check;**刻意不更新 latency_ms**
    /// (ping 延迟不代表真实请求延迟,覆盖会污染选路 EMA)。
    ///
    /// `POST /api/providers/{id}/tests`
    pub async fn admin_test_provider(&self, id: String) -> AdminTestProviderResponse {
        // Step 1: 读库(锁在读侧结束,探测不持锁)。
        let row = match self.store.find_provider(&id).await {
            Ok(Some(r)) => r,
            _ => {
                tracing::error!("test provider {id}: not found or store error");
                return AdminTestProviderResponse::NotFound(problem(
                    404,
                    "not_found",
                    "资源不存在",
                ));
            }
        };
        let api_key = self.store.decrypt_key(&row.api_key);
        // 默认协议列为空时退回 provider_type(azure/custom 等讲 OpenAI);
        // 按渠道默认协议选对应的上游地址,留空的一路回退到另一个。
        let protocol = channel_protocol(&row);
        let base_url =
            pick_base_url(protocol, &row.openai_base_url, &row.anthropic_base_url).to_string();
        // Step 2: 探测(无锁)。
        let (status_raw, latency) = self
            .probes
            .check_health(HealthProbe {
                base_url,
                api_key,
                protocol: protocol.as_str().to_string(),
                timeout_secs: row.timeout_secs.clamp(1, 600) as u64,
                proxy_url: row.proxy_url.clone(),
                channel_kind: row.channel_kind.clone(),
            })
            .await;
        // Step 3: 写回。写失败只留痕不改变响应——探测本身已成功,枚举无
        // 5xx 可表达旧 500 语义(spec 缺口,见文件头)。
        if let Err(()) = self
            .store
            .update_health(&row.id, &status_raw, &chrono::Utc::now().to_rfc3339())
            .await
        {
            tracing::error!("test provider {}: failed to persist health status", row.id);
        }
        let status = api_test_status(&status_raw);
        AdminTestProviderResponse::Created(ProviderTestResult {
            provider_id: row.id.clone(),
            name: row.name.clone(),
            message: format!(
                "Provider {} is {} ({}ms)",
                row.name,
                status.as_str(),
                latency as i64
            ),
            status,
            latency_ms: latency,
        })
    }

    /// 对渠道 + 模型发真实对话请求(端到端验证,可流式;200 → **201**)。
    /// **业务失败仍是成功响应**:HTTP 层只表达"测试已执行",ok:false /
    /// status / error 在 body;结果持久化到 model_health。
    ///
    /// `POST /api/providers/{id}/model-tests`
    pub async fn admin_test_provider_model(
        &self,
        id: String,
        body: ModelTestRequest,
    ) -> AdminTestProviderModelResponse {
        let model = body.model.trim().to_string();
        if model.is_empty() {
            return AdminTestProviderModelResponse::BadRequest(problem(
                400,
                "missing_model",
                "model 不能为空",
            ));
        }
        let mut row = match self.store.find_provider(&id).await {
            Ok(Some(r)) => r,
            _ => {
                tracing::error!("model-test {id}: provider not found or store error");
                return AdminTestProviderModelResponse::NotFound(problem(
                    404,
                    "not_found",
                    "资源不存在",
                ));
            }
        };
        // api_key 出库解密(enc:v1: 密文,明文兼容;解密失败得空串并留痕)。
        row.api_key = self.store.decrypt_key(&row.api_key);
        let declared = channel_protocols(&row);
        let protocol = match &body.protocol {
            Some(p) => {
                let s = p.as_str();
                if !declared.iter().any(|x| x == s) {
                    return AdminTestProviderModelResponse::BadRequest(problem(
                        400,
                        "unsupported_protocol",
                        &format!("该渠道不支持 {s} 协议"),
                    ));
                }
                s.to_string()
            }
            None => channel_protocol(&row).as_str().to_string(),
        };
        // 两种协议共用同一最小探针形状;messages 覆盖单条 message。
        let messages: Vec<Value> = match &body.messages {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
                .collect(),
            _ => {
                let message = body
                    .message
                    .as_deref()
                    .filter(|m| !m.trim().is_empty())
                    .unwrap_or("Say OK");
                vec![serde_json::json!({"role": "user", "content": message})]
            }
        };
        let stream = body.stream.unwrap_or(false);
        if stream {
            // SSE:事件契约见 ProviderProbes::probe_model_stream 文档;
            // 落库由适配器在流结束时完成。
            return AdminTestProviderModelResponse::CreatedStream(
                self.probes
                    .probe_model_stream(row, protocol, model, messages, 1024),
            );
        }
        let probe = self
            .probes
            .probe_model(row, protocol.clone(), model.clone(), messages, 1024)
            .await;
        self.store
            .record_model_health(
                &id,
                &model,
                probe.ok,
                probe.latency_ms,
                probe.error.as_deref(),
            )
            .await;
        AdminTestProviderModelResponse::Created(ModelTestResult {
            ok: probe.ok,
            latency_ms: probe.latency_ms,
            protocol,
            stream,
            snippet: probe.snippet,
            // 出参口径:上游原始响应体序列化为 JSON 字符串(测试页展示
            // 原文;流式为 {"stream":true,"events":N,"content":...} 汇总)。
            response: probe.response.map(|v| v.to_string()),
            status: if probe.ok {
                None
            } else {
                Some(Some(i64::from(probe.status)))
            },
            error: if probe.ok {
                None
            } else {
                Some(Some(
                    probe.error.unwrap_or_else(|| "upstream error".to_string()),
                ))
            },
        })
    }

    /// 为(尚未保存的)渠道配置拉取上游模型列表(保留 POST 动作端点,
    /// x-action: fetch-upstream-models)。编辑态回退:api_key / proxy_url
    /// 留空且带 provider_id 时取库存值(密文先解密);502 只回显 host。
    ///
    /// `POST /api/providers/fetch-models`
    pub async fn admin_fetch_upstream_models(
        &self,
        body: FetchModelsRequest,
    ) -> AdminFetchUpstreamModelsResponse {
        let bad = |code: &str, detail: &str| {
            AdminFetchUpstreamModelsResponse::BadRequest(problem(400, code, detail))
        };
        let base_url = body.base_url.trim().trim_end_matches('/').to_string();
        let mut api_key = body.api_key.clone().unwrap_or_default();
        let protocol = match &body.protocol {
            Some(p) => p.as_str().to_string(),
            None => body
                .provider_type
                .clone()
                .unwrap_or_else(|| "openai".to_string()),
        };
        // 仅 openai / anthropic 可用于拉模型(responses 无独立鉴权语义)。
        match protocol.as_str() {
            "openai" | "anthropic" => {}
            _ => return bad("invalid_protocol", "protocol 仅支持 openai / anthropic"),
        }
        let mut proxy_url = body.proxy_url.clone().unwrap_or_default();

        // 编辑表单里 api_key / 代理都是"留空不修改":任一为空且有
        // provider_id 时回退库存值,否则带认证的代理在编辑态下必失败。
        if (api_key.is_empty() || proxy_url.is_empty()) && body.provider_id.is_some() {
            let pid = body.provider_id.as_deref().unwrap_or_default();
            match self.store.find_provider_credentials(pid).await {
                Ok(Some((stored_key, stored_proxy))) => {
                    if api_key.is_empty() {
                        api_key = self.store.decrypt_key(&stored_key);
                    }
                    if proxy_url.is_empty() {
                        proxy_url = stored_proxy;
                    }
                }
                // 旧行为:查不到(含查询失败)统一 404。
                _ => {
                    tracing::error!("fetch-models: provider {pid} not found or store error");
                    return AdminFetchUpstreamModelsResponse::NotFound(problem(
                        404,
                        "not_found",
                        "资源不存在",
                    ));
                }
            }
        }

        if base_url.is_empty() || api_key.is_empty() {
            return bad("missing_config", "base_url 和 API Key 不能为空");
        }

        let host = url_host(&base_url).to_string();
        let probe = FetchModelsProbe {
            base_url,
            api_key,
            protocol,
            proxy_url,
            channel_kind: body
                .channel_kind
                .as_deref()
                .unwrap_or_default()
                .trim()
                .to_string(),
        };
        match self.probes.fetch_upstream_models(probe).await {
            Ok(models) => AdminFetchUpstreamModelsResponse::Ok(FetchedModels { models }),
            Err(e) => AdminFetchUpstreamModelsResponse::BadGateway(match e {
                // 连接失败只回显 host:完整 URL 可能含敏感路径/参数,
                // 传输层错误对象本身也带 URL(适配器已去)。
                FetchModelsError::Connect { message } => problem(
                    502,
                    "upstream_error",
                    &format!("无法连接上游 {host}: {message}"),
                ),
                FetchModelsError::ReadFailed(msg) => {
                    problem(502, "upstream_error", &format!("上游响应读取失败: {msg}"))
                }
                FetchModelsError::UpstreamStatus { status, message } => problem(
                    502,
                    "upstream_error",
                    &format!("上游返回 {status}: {message}"),
                ),
                FetchModelsError::NoModels => problem(502, "no_models", "上游响应中没有模型列表"),
            }),
        }
    }

    /// 每(渠道, 模型)对的最近实测健康(+ `?provider_id=` 过滤、分页与
    /// total)。查询失败 500 不静默空列表。
    ///
    /// `GET /api/model-health`
    pub async fn admin_list_model_health(
        &self,
        limit: Option<i64>,
        offset: Option<i64>,
        provider_id: Option<String>,
    ) -> AdminListModelHealthResponse {
        let (limit, offset) = page_args(limit, offset);
        match self
            .store
            .list_model_health(provider_id.as_deref(), limit, offset)
            .await
        {
            Ok((rows, total)) => AdminListModelHealthResponse::Ok(ModelHealthPage {
                items: rows
                    .into_iter()
                    .map(|r| ApiModelHealth {
                        provider_id: r.provider_id,
                        model: r.model,
                        // 库内 CHECK 含 unknown(建表默认,现网写入只有
                        // healthy / unhealthy);词表两值之外按 unhealthy 收敛。
                        status: if r.status == "healthy" {
                            ModelHealthStatus::Healthy
                        } else {
                            ModelHealthStatus::Unhealthy
                        },
                        latency_ms: r.latency_ms,
                        error: Some(r.error),
                        checked_at: r.checked_at.as_deref().map(crate::sqlite_utc),
                    })
                    .collect(),
                total,
            }),
            Err(()) => {
                tracing::error!("list model health: query failed");
                AdminListModelHealthResponse::InternalServerError(problem(
                    500,
                    "query_failed",
                    "查询模型健康失败",
                ))
            }
        }
    }

    /// 渠道适配器目录(编译期静态注册,非插件)。刻意不设分页(目录极小,
    /// 分页是纯噪音);响应仍用 {items,total} 信封保持客户端统一。
    ///
    /// `GET /api/channel-kinds`
    pub async fn admin_list_channel_kinds(&self) -> AdminListChannelKindsResponse {
        let items: Vec<ApiChannelKind> = aikun_channels::registry()
            .list()
            .into_iter()
            .map(|i| ApiChannelKind {
                kind: i.kind.to_string(),
                label: i.label.to_string(),
                description: if i.description.is_empty() {
                    None
                } else {
                    Some(i.description.to_string())
                },
                experimental: i.experimental,
            })
            .collect();
        AdminListChannelKindsResponse::Ok(ChannelKindPage {
            total: items.len() as i64,
            items,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_bounds() {
        assert!(!valid_timeout_secs(0)); // 0 = no timeout, rejected
        assert!(!valid_timeout_secs(-1)); // negative wraps downstream
        assert!(valid_timeout_secs(1));
        assert!(valid_timeout_secs(120));
        assert!(valid_timeout_secs(600));
        assert!(!valid_timeout_secs(601));
    }

    #[test]
    fn weight_bounds_and_finite() {
        assert!(valid_weight(1.0));
        assert!(valid_weight(0.1));
        assert!(valid_weight(1000.0));
        assert!(!valid_weight(0.09));
        assert!(!valid_weight(1000.1));
        assert!(!valid_weight(f64::INFINITY)); // 1e400 反序列化为 inf
        assert!(!valid_weight(f64::NAN));
    }

    #[test]
    fn priority_and_retries_bounds() {
        assert!(valid_priority(0));
        assert!(valid_priority(100));
        assert!(!valid_priority(-1));
        assert!(!valid_priority(101));
        assert!(valid_max_retries(0));
        assert!(valid_max_retries(10));
        assert!(!valid_max_retries(-1));
        assert!(!valid_max_retries(11));
    }

    #[test]
    fn proxy_url_sanitized_to_scheme_host() {
        assert_eq!(
            sanitize_proxy_url("socks5://user:pass@127.0.0.1:1080"),
            "socks5://127.0.0.1:1080"
        );
        assert_eq!(
            sanitize_proxy_url("http://proxy.local:8080"),
            "http://proxy.local:8080"
        );
        // 无 scheme 的 userinfo 形态同样剥凭证
        assert_eq!(sanitize_proxy_url("user@host:1"), "host:1");
        assert_eq!(sanitize_proxy_url(""), "");
    }

    #[test]
    fn url_host_extracts_scheme_relative_host() {
        assert_eq!(url_host("https://api.example.com/v1"), "api.example.com");
        assert_eq!(url_host("http://127.0.0.1:9"), "127.0.0.1:9");
        assert_eq!(url_host("api.example.com"), "api.example.com");
    }

    #[test]
    fn pagination_args_clamped_not_rejected() {
        assert_eq!(page_args(None, None), (50, 0));
        assert_eq!(page_args(Some(0), None), (1, 0)); // 越界静默 clamp
        assert_eq!(page_args(Some(999), Some(-5)), (200, 0));
        assert_eq!(page_args(Some(7), Some(13)), (7, 13));
    }

    #[test]
    fn health_status_vocabulary_unhealthy_maps_to_down() {
        assert!(matches!(
            api_health_status("unhealthy"),
            ProviderHealthStatus::Down
        ));
        assert!(matches!(
            api_health_status("healthy"),
            ProviderHealthStatus::Healthy
        ));
        assert!(matches!(
            api_health_status("degraded"),
            ProviderHealthStatus::Degraded
        ));
        assert!(matches!(
            api_health_status("whatever"),
            ProviderHealthStatus::Unknown
        ));
        assert!(matches!(
            api_test_status("unhealthy"),
            ProviderTestResultStatus::Down
        ));
    }

    fn candidate() -> ProviderCandidate {
        ProviderCandidate {
            name: "p".to_string(),
            provider_type: None,
            openai_base_url: Some("http://127.0.0.1:1".to_string()),
            anthropic_base_url: None,
            api_key: "sk-x".to_string(),
            models: vec!["gpt-4".to_string()],
            priority: None,
            weight: None,
            max_retries: None,
            timeout_secs: None,
            proxy_url: None,
            model_mapping: BTreeMap::new(),
            protocols: Some(vec!["openai".to_string()]),
            default_protocol: None,
            note: None,
            website_url: None,
            channel_kind: None,
        }
    }

    /// 校验失败的 code;通过则返回空串(测试里只在预期失败处调用)。
    fn err_code(cand: ProviderCandidate) -> String {
        match validate_candidate(&cand) {
            Err(p) => p.code,
            Ok(_) => String::new(),
        }
    }

    #[test]
    fn candidate_defaults_applied() {
        let config = validate_candidate(&candidate()).expect("valid candidate");
        assert_eq!(config.provider_type, "openai"); // 跟随默认协议
        assert_eq!(config.default_protocol, "openai");
        assert_eq!(config.priority, 0);
        assert_eq!(config.weight, 1.0);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.timeout_secs, 120);
        assert_eq!(config.channel_kind, ""); // generic
        // 尾部斜杠归一
        assert_eq!(config.openai_base_url, "http://127.0.0.1:1");
    }

    #[test]
    fn candidate_rejects_cross_field_violations() {
        // responses 单独成渠道
        let mut c = candidate();
        c.protocols = Some(vec!["responses".to_string()]);
        assert_eq!(err_code(c), "invalid_protocols");
        // 勾选 openai 但地址空白
        let mut c = candidate();
        c.openai_base_url = Some("   ".to_string());
        assert_eq!(err_code(c), "invalid_base_url");
        // 默认协议未勾选
        let mut c = candidate();
        c.default_protocol = Some("anthropic".to_string());
        assert_eq!(err_code(c), "invalid_default_protocol");
        // 名字空白
        let mut c = candidate();
        c.name = "  ".to_string();
        assert_eq!(err_code(c), "invalid_name");
        // weight 非有限(1e400 → inf)
        let mut c = candidate();
        c.weight = Some(f64::INFINITY);
        assert_eq!(err_code(c), "invalid_weight");
        // 未注册渠道类型
        let mut c = candidate();
        c.channel_kind = Some("no-such-kind".to_string());
        assert_eq!(err_code(c), "invalid_channel_kind");
        // anthropic-only 渠道合法(仅需 anthropic 地址)
        let mut c = candidate();
        c.protocols = Some(vec!["anthropic".to_string()]);
        c.openai_base_url = None;
        c.anthropic_base_url = Some("http://anthropic.upstream".to_string());
        assert!(validate_candidate(&c).is_ok());
    }
}
