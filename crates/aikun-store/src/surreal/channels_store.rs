//! # SurrealDB 渠道域实现桩(阶段 S)
//!
//! **指派:并行工人 · Channels**(分工边界与纪律见
//! `docs/surreal-migration.md` §七)。
//!
//! ## 你负责的域
//!
//! `provider` 与 `model_health` 两张表的全部读写:
//!
//! * 渠道拓扑快照(`active_channels`):registry 缓存(`aikun/src/channel/
//!   registry.rs`)的数据源。返回的 [`Provider`](aikun_channels::Provider)
//!   的 `api_key` 保持**库内密文形态**(`enc:v1:…`),选中后由调用方解密
//!   一次(roadmap 决策 4,快照与解密职责分离);
//! * 管理面 providers 组 CRUD + 批量创建(重复检测走 `key_hash`)与
//!   model-health 组;
//! * 健康检查循环与请求路径的 EMA 写(`record_channel_result`:
//!   latency_ms/error_rate/health_status/consecutive_failures/自动禁用
//!   disabled_reason)。
//!
//! ## 实现约定(必读)
//!
//! * 域类型 [`Provider`](aikun_channels::Provider) 的 `models` /
//!   `model_mapping` / `protocols` 是 **JSON 字符串**(registry 契约);
//!   Surreal 侧存的是原生 `array<string>` / `object` / `array<string>`
//!   ——行映射时做 序列化/反序列化 一次(读:array → JSON String;写:
//!   JSON String → array)。类型映射是本次重设计的一部分,见设计文档
//!   §四.provider。
//! * `provider_type` 列**保留**(openapi.yaml 冻结契约 required 字段),
//!   不因"看起来被 protocols 取代"而删。
//! * EMA/健康字段(latency_ms/error_rate/health_status/consecutive_
//!   failures/disabled_reason/last_health_check)**保留在 provider 行上**
//!   ——独立成表的备选已被评审否决,理由见设计文档 §四.provider(拓扑
//!   快照单次读取 + 令牌聚合单表完成)。
//! * 拓扑缓存令牌 `topology_token`:语义 = SQLite 侧 `CacheTokens::
//!   channel_topology_token`(lib.rs 注释:行内容变化 ⇒ 令牌大概率变化,
//!   允许同内容变化)。建议口径:`count + max(updated_at) + count(
//!   health_status='unhealthy')` 一条聚合查询,与 SQLite 指纹同源;
//!   **EMA 热路径写(latency/error_rate)不 bump updated_at**,避免每
//!   请求触发全量重载(与 SQLite 指纹口径一致)。
//! * 管理端 CRUD 写必须 `updated_at = time::now()`(令牌可见性依赖)。
//! * `model_health` 是数组 record id 表(`model_health:[provider, model]`),
//!   UPSERT 定位用复合 id;`provider` 字段冗余存链接(列表查询用)。
//! * 加密/哈希沿用 `crate::crypto::{KeyCipher, decrypt_or_plain}` 与
//!   `crate::password::hash_api_key`(key_hash = sha256(明文));密文
//!   形态与 SQLite 侧逐字节一致,迁移无需重加密。
//! * 语义基准 = SQLite 实现(`aikun/src/main.rs` 的 `DbProviderStore`/
//!   `GatewayProbes` 适配器 + `provider_row.rs`);对外契约不变。

use aikun_channels::Provider;

use crate::surreal::SurrealHandle;
use crate::{CacheToken, StoreError};

/// 渠道域能力:async trait(async 化决策见设计文档 §六)。
#[async_trait::async_trait]
pub trait ChannelStore: Send + Sync {
    /// 全部启用渠道(is_active = true),含健康度/EMA 字段库内现值;
    /// `Provider.api_key` 为密文形态(见文件头)。
    async fn active_channels(&self) -> Result<Vec<Provider>, StoreError>;

    /// 渠道拓扑缓存令牌(见文件头令牌口径)。
    async fn topology_token(&self) -> Result<CacheToken, StoreError>;

    /// 单渠道读取(管理面 PATCH 回读/探测路径)。
    async fn get_provider(&self, id: &str) -> Result<Option<Provider>, StoreError>;

    /// 渠道列表(is_active 过滤,q 名字模糊;ORDER BY created_at DESC),
    /// 返回 (total, 页)。
    async fn list_providers(
        &self,
        q: &str,
        active_only: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<Provider>), StoreError>;

    /// 创建渠道(api_key 落库前经 KeyCipher 加密、key_hash = sha256 明文)。
    async fn create_provider(&self, row: NewProvider) -> Result<Provider, StoreError>;

    /// 部分更新(管理面 PATCH;updated_at 必须 bump)。返回更新后的行。
    async fn update_provider(
        &self,
        id: &str,
        patch: ProviderPatch,
    ) -> Result<Option<Provider>, StoreError>;

    /// 删除渠道;级联清理 model_health(数组 id 以该 provider 为前缀,
    /// Surreal 无 FK 级联,需显式 DELETE)。channel_affinity 的残留行
    /// 由选路时自然失效(设计文档 §四),不在此清理。
    async fn delete_provider(&self, id: &str) -> Result<bool, StoreError>;

    /// 批量创建(逐条结果,成败全在返回值;重复检测 = key_hash 点查,
    /// `idx_provider_key_hash` 索引)。
    async fn create_providers_batch(
        &self,
        rows: Vec<NewProvider>,
    ) -> Vec<Result<Provider, StoreError>>;

    /// 按 key_hash 查已存在的渠道(跨批次重复检测)。
    async fn find_by_key_hash(&self, key_hash: &str) -> Result<Vec<Provider>, StoreError>;

    /// 健康检查/请求结果回写(EMA 字段;consecutive_failures 达阈值时
    /// 自动禁用 + disabled_reason,阈值判定在调用方,这里只落值)。
    async fn record_channel_result(
        &self,
        id: &str,
        latency_ms: f64,
        error_rate: f64,
        health_status: &str,
        consecutive_failures: i32,
        disabled_reason: Option<&str>,
    ) -> Result<(), StoreError>;

    /// 模型级健康 UPSERT(数组 id `model_health:[provider, model]`)。
    async fn upsert_model_health(&self, row: ModelHealthRow) -> Result<(), StoreError>;

    /// 某渠道的全部模型健康;渠道删除前的清理也用它拿 id 集。
    async fn list_model_health(&self, provider_id: &str)
    -> Result<Vec<ModelHealthRow>, StoreError>;
}

/// 新建渠道(与 SQLite 管理面入参对齐;models/mapping/protocols 已是
/// JSON 字符串,落库前转原生 array/object)。
#[derive(Debug, Clone)]
pub struct NewProvider {
    pub id: Option<String>,
    pub name: String,
    pub provider_type: String,
    pub channel_kind: String,
    pub openai_base_url: String,
    pub anthropic_base_url: String,
    /// 明文 key;落库前 KeyCipher::encrypt。
    pub api_key_plain: String,
    pub models_json: String,
    pub model_mapping_json: String,
    pub protocols_json: String,
    pub default_protocol: String,
    pub priority: i32,
    pub weight: f64,
    pub max_retries: i32,
    pub timeout_secs: i32,
    pub proxy_url: String,
    pub note: String,
    pub website_url: String,
}

/// 渠道部分更新(None = 不动)。base_url 三态(None=不动/Some(Some(v)=设)/
/// Some(None)=清空)对齐管理面 PATCH 语义。
#[derive(Debug, Clone, Default)]
pub struct ProviderPatch {
    pub name: Option<String>,
    pub provider_type: Option<String>,
    pub channel_kind: Option<String>,
    pub anthropic_base_url: Option<Option<String>>,
    pub api_key_plain: Option<String>,
    pub models_json: Option<String>,
    pub model_mapping_json: Option<String>,
    pub protocols_json: Option<String>,
    pub default_protocol: Option<String>,
    pub priority: Option<i32>,
    pub weight: Option<f64>,
    pub is_active: Option<bool>,
    pub max_retries: Option<i32>,
    pub timeout_secs: Option<i32>,
    pub proxy_url: Option<String>,
    pub note: Option<String>,
    pub website_url: Option<String>,
}

/// 模型级健康行。
#[derive(Debug, Clone)]
pub struct ModelHealthRow {
    pub provider_id: String,
    pub model: String,
    pub status: String,
    pub latency_ms: f64,
    pub error: String,
    pub checked_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// 渠道域的 SurrealDB 实现(桩:工人按上述约定填 todo!() 体)。
pub struct SurrealChannels {
    handle: SurrealHandle,
    cipher: crate::crypto::KeyCipher,
}

impl SurrealChannels {
    /// cipher 与 SQLite 池同源(`AIKUN_ENCRYPTION_KEY` 派生,见 crypto.rs),
    /// 保证密文形态一致、迁移免重加密。
    pub fn new(handle: SurrealHandle, cipher: crate::crypto::KeyCipher) -> Self {
        Self { handle, cipher }
    }
}

#[async_trait::async_trait]
impl ChannelStore for SurrealChannels {
    async fn active_channels(&self) -> Result<Vec<Provider>, StoreError> {
        let _ = &self.handle;
        todo!("工人·Channels:SELECT * FROM provider WHERE is_active = true(密文原样出库)")
    }

    async fn topology_token(&self) -> Result<CacheToken, StoreError> {
        let _ = &self.handle;
        todo!("工人·Channels:聚合(count/max updated_at/unhealthy 计数)→ CacheToken::new")
    }

    async fn get_provider(&self, id: &str) -> Result<Option<Provider>, StoreError> {
        let _ = (&self.handle, id);
        todo!("工人·Channels:SELECT record provider:<id>")
    }

    async fn list_providers(
        &self,
        q: &str,
        active_only: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<Provider>), StoreError> {
        let _ = (&self.handle, q, active_only, limit, offset);
        todo!("工人·Channels:SELECT ... WHERE ... ORDER created_at DESC LIMIT/START")
    }

    async fn create_provider(&self, row: NewProvider) -> Result<Provider, StoreError> {
        let _ = (&self.handle, &self.cipher, row);
        todo!("工人·Channels:加密 api_key + key_hash + CREATE(ULID key)")
    }

    async fn update_provider(
        &self,
        id: &str,
        patch: ProviderPatch,
    ) -> Result<Option<Provider>, StoreError> {
        let _ = (&self.handle, &self.cipher, id, patch);
        todo!("工人·Channels:UPDATE ... SET ..., updated_at = time::now(),回读")
    }

    async fn delete_provider(&self, id: &str) -> Result<bool, StoreError> {
        let _ = (&self.handle, id);
        todo!("工人·Channels:DELETE provider + 显式删 model_health 前缀行")
    }

    async fn create_providers_batch(
        &self,
        rows: Vec<NewProvider>,
    ) -> Vec<Result<Provider, StoreError>> {
        let _ = (&self.handle, &self.cipher, rows);
        todo!("工人·Channels:逐条 create + key_hash 重复检测,结果逐条返回")
    }

    async fn find_by_key_hash(&self, key_hash: &str) -> Result<Vec<Provider>, StoreError> {
        let _ = (&self.handle, key_hash);
        todo!("工人·Channels:SELECT ... WHERE key_hash = $h")
    }

    async fn record_channel_result(
        &self,
        id: &str,
        latency_ms: f64,
        error_rate: f64,
        health_status: &str,
        consecutive_failures: i32,
        disabled_reason: Option<&str>,
    ) -> Result<(), StoreError> {
        let _ = (
            &self.handle,
            id,
            latency_ms,
            error_rate,
            health_status,
            consecutive_failures,
            disabled_reason,
        );
        todo!("工人·Channels:UPDATE EMA 字段(不 bump updated_at,见文件头)")
    }

    async fn upsert_model_health(&self, row: ModelHealthRow) -> Result<(), StoreError> {
        let _ = (&self.handle, row);
        todo!("工人·Channels:UPSERT model_health:[provider:<id>, $model]")
    }

    async fn list_model_health(
        &self,
        provider_id: &str,
    ) -> Result<Vec<ModelHealthRow>, StoreError> {
        let _ = (&self.handle, provider_id);
        todo!("工人·Channels:SELECT ... WHERE provider = provider:<id>")
    }
}
