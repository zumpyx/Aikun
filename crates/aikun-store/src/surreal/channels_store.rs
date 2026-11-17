//! # SurrealDB 渠道域实现(阶段 S)
//!
//! **指派:并行工人 · Channels**(分工边界与纪律见
//! `docs/surreal-migration.md` §七)。
//!
//! ## 负责的域
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
//! ## 实现约定(语义基准 = SQLite 现行为)
//!
//! * 域类型 [`Provider`](aikun_channels::Provider) 的 `models` /
//!   `model_mapping` / `protocols` 是 **JSON 字符串**(registry 契约);
//!   Surreal 侧存的是原生 `array<string>` / `object` / `array<string>`
//!   ——行映射时做 序列化/反序列化 一次(读:array → JSON String;写:
//!   JSON String → array)。类型映射是本次重设计的一部分,见设计文档
//!   §四.provider。坏 JSON 落库按空集/空对象处理(与域侧读取的
//!   `unwrap_or_default` 口径一致)。
//! * `provider_type` 列**保留**(openapi.yaml 冻结契约 required 字段),
//!   不因"看起来被 protocols 取代"而删。
//! * EMA/健康字段(latency_ms/error_rate/health_status/consecutive_
//!   failures/disabled_reason/last_health_check)**保留在 provider 行上**
//!   ——独立成表的备选已被评审否决,理由见设计文档 §四.provider(拓扑
//!   快照单次读取 + 令牌聚合单表完成)。
//! * 拓扑缓存令牌 `topology_token`:语义 = SQLite 侧 `CacheTokens::
//!   channel_topology_token`(lib.rs 注释:行内容变化 ⇒ 令牌大概率变化,
//!   允许同内容变化)。本实现口径:`count(*) | max(updated_at) |
//!   count(health_status='unhealthy')` 三段聚合(一条三语句查询),与
//!   SQLite 指纹(`COUNT|MAX(rowid)|MAX(updated_at)|SUM(unhealthy)`)
//!   同源——Surreal 无 rowid,行数+最大 updated_at 覆盖同一变更面;
//!   datetime 为微秒精度,同秒撞车比 SQLite 更少。
//!   **EMA 热路径写(latency/error_rate)不 bump updated_at**,避免每
//!   请求触发全量重载(与 SQLite 指纹口径一致);自动禁用写 bump
//!   updated_at(同 selector.rs 的禁用 UPDATE)。
//! * 管理端 CRUD 写必须 `updated_at = time::now()`(令牌可见性依赖)。
//! * `record_channel_result` 是 SQLite 侧 `record_request_result` /
//!   `record_failure` / 健康循环 `update_health` 三条写路径的合并缝:
//!   EMA 数值与阈值判定都在调用侧,本方法只落值;`last_health_check`
//!   随每次回写刷新为 time::now()(该字段在本 trait 里没有其他写点,
//!   语义 = 最近一次观测时间)。
//! * `model_health` 是数组 record id 表(`model_health:[provider, model]`),
//!   UPSERT 定位用复合 id;`provider` 字段冗余存链接(列表查询用)。
//! * 加密/哈希沿用 `crate::crypto::KeyCipher` 与
//!   `crate::password::hash_api_key`(key_hash = sha256(明文) 十六进制);
//!   密文形态与 SQLite 侧逐字节一致(同一 `KeyCipher` 派生),迁移免重加密。
//!   空 key:密文仍落库(与 admin `insert_checked` 无条件加密一致),
//!   key_hash 为空串、不参与查重。
//! * SurrealQL 4.0.1 实测口径(mem:// 探针,见 git 提交说明):
//!   record id 运行期构造用 `type::record`(旧文档的 `type::thing` 在
//!   4.0 已更名);`math::max` 不收 datetime(聚合最大 updated_at 用
//!   `ORDER BY updated_at DESC LIMIT 1` 替代);`LIMIT $l START $o`
//!   可绑定参数;datetime 经 JSON 序列化为 RFC3339 字符串,record id
//!   为 `"provider:<key>"` 字符串。
//!
//! ## 集成批补充(默认切换装配面)
//!
//! 工人桩交付后,集成批在本文件补充了装配所需的方法(不改既有签名):
//! `record_health_ping`(健康循环只写 health_status/last_health_check,
//! ping 延迟不覆盖选路 EMA)、`provider_metrics`(EMA 读侧,调用侧算
//! 好新值经 `record_channel_result` 落库)、`StoredProviderConfig` 三件
//! (管理面 PATCH/复制的**密文原样**读写——避免"适配器先解密再重加密"
//! 的无谓往返与 nonce 漂移)、`list_providers_by_priority`(管理面列表
//! 的 `ORDER BY priority DESC, name` 旧序)、`all_channel_models`/
//! `find_key_owner`(计费页 price-in-use 与换 key 查重)。
//! `model_mapping` 的 FLEXIBLE 已落 SCHEMA_SURQL(原桩 TODO 结案)。

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

    /// 模型健康分页(None = 全部渠道;ORDER BY provider, model 稳定序,
    /// 管理面列表口径)。返回 (total, 页)。
    async fn list_model_health_scoped(
        &self,
        provider_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<ModelHealthRow>), StoreError>;

    // ---- 集成批补充(默认切换装配面;见文件头「集成批补充」) ----

    /// 后台健康循环的 ping 写回:**只**更新 health_status 与
    /// last_health_check——ping 延迟不代表真实请求延迟,不得覆盖选路
    /// EMA 的 latency_ms(SQLite 旧 UPDATE 同列集)。不 bump updated_at
    /// (令牌口径同 EMA 写)。
    async fn record_health_ping(&self, id: &str, status: &str) -> Result<(), StoreError>;

    /// EMA 读侧:请求路径回写前读当前 (latency_ms, error_rate,
    /// consecutive_failures),调用侧按旧 SQL 公式算好新值经
    /// [`ChannelStore::record_channel_result`] 落库(读-算-写两往返,
    /// fire-and-forget 路径可接受;并发丢一次更新只影响平滑指标的
    /// 瞬时精度,不影响健康判定与自动禁用的阈值语义)。
    async fn provider_metrics(&self, id: &str) -> Result<Option<ProviderMetrics>, StoreError>;

    /// 管理面复制/编辑态回读:**密文与 key_hash 原样**(不在此加解密)。
    async fn read_provider_stored(
        &self,
        id: &str,
    ) -> Result<Option<StoredProviderConfig>, StoreError>;

    /// 管理面建行/复制(id 由调用方生成;密文与 key_hash 原样落库,
    /// 与 SQLite 侧 `INSERT INTO providers (id, …)` 同构)。唯一冲突
    /// (name 无唯一索引,理论无冲突面)按 Err 带出。
    async fn insert_provider_stored(
        &self,
        id: &str,
        config: &StoredProviderConfig,
    ) -> Result<Provider, StoreError>;

    /// 管理面 PATCH(密文 key 与 key_hash 成对原样更新;Some(true)
    /// 重启用清 disabled_reason/consecutive_failures;恒 bump
    /// updated_at)。返回更新后的行;未命中 None。
    async fn apply_provider_patch(
        &self,
        id: &str,
        patch: StoredProviderPatch,
    ) -> Result<Option<Provider>, StoreError>;

    /// 管理面渠道列表的旧序:`ORDER BY priority DESC, name ASC`
    /// (+ total)。
    async fn list_providers_by_priority(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<Provider>), StoreError>;

    /// 全部渠道(含禁用)的 models 集合(计费页 price-in-use 判定;
    /// 查询失败由调用方按旧行为静默空集)。
    async fn all_channel_models(&self) -> Result<std::collections::HashSet<String>, StoreError>;

    /// key_hash 归属:已使用该 hash 的**其他**渠道名(exclude_id 排除
    /// 自身;换 key 场景)。无归属 None。
    async fn find_key_owner(
        &self,
        key_hash: &str,
        exclude_id: &str,
    ) -> Result<Option<String>, StoreError>;
}

/// EMA 读侧快照(`provider_metrics` 出参)。
#[derive(Debug, Clone, Copy, Default)]
pub struct ProviderMetrics {
    pub latency_ms: f64,
    pub error_rate: f64,
    pub consecutive_failures: i32,
}

/// 管理面建行/复制的配置列(**密文原样**;与 aikun-admin 的
/// ProviderConfig 一一对应,id 另传)。models/mapping/protocols 是
/// JSON 字符串(域侧契约),落库转原生 array/object。
#[derive(Debug, Clone, Default)]
pub struct StoredProviderConfig {
    pub name: String,
    pub provider_type: String,
    pub openai_base_url: String,
    pub anthropic_base_url: String,
    /// enc:v1: 密文(或历史明文)原样。
    pub api_key_stored: String,
    /// sha256(明文) hex;空 key 为空串。
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

/// 管理面 PATCH(Some 才进 SET;`api_key_stored`/`key_hash` 成对)。
#[derive(Debug, Clone, Default)]
pub struct StoredProviderPatch {
    pub name: Option<String>,
    pub provider_type: Option<String>,
    pub openai_base_url: Option<String>,
    pub anthropic_base_url: Option<String>,
    pub note: Option<String>,
    pub website_url: Option<String>,
    pub channel_kind: Option<String>,
    pub api_key_stored: Option<String>,
    pub key_hash: Option<String>,
    pub models_json: Option<String>,
    pub priority: Option<i64>,
    pub weight: Option<f64>,
    /// Some(true) 重启用:同时清 disabled_reason / consecutive_failures。
    pub is_active: Option<bool>,
    pub max_retries: Option<i64>,
    pub timeout_secs: Option<i64>,
    pub proxy_url: Option<String>,
    pub model_mapping_json: Option<String>,
    pub protocols_json: Option<String>,
    pub default_protocol: Option<String>,
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

/// 渠道域的 SurrealDB 实现。
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

// ---------- 内部:JSON 行映射(读侧) ----------
//
// 读路径统一 `SELECT *` + `take::<Vec<serde_json::Value>>`:schemafull 保证
// 字段集稳定,映射按键取值(免为每表 derive SurrealValue);JSON 形态
// (id/datetime 序列化为字符串、array/object 原生)已经 mem:// 探针核实。

/// `"<table>:<key>"` 形态的 record id JSON 值 → 不透明 key 字符串。
/// 防御性处理对象形态({"tb":..,"id":..})与数值 key。含连字符等特殊
/// 字符的**字符串 key**(迁移进来的旧 uuid)序列化时被反引号包裹
/// (`` provider:`550e…` ``,与真 Uuid 类型的 `u'…'` 区分),剥掉还原
/// 裸 key(identity.rs 的 unquote_key 同款;key 本体不含反引号)。
fn record_key_suffix(v: &serde_json::Value, table: &str) -> String {
    let unquote = |s: &str| -> String {
        s.strip_prefix('`')
            .and_then(|x| x.strip_suffix('`'))
            .unwrap_or(s)
            .to_string()
    };
    match v {
        serde_json::Value::String(s) => {
            let prefix = format!("{table}:");
            unquote(s.strip_prefix(&prefix).unwrap_or(s))
        }
        serde_json::Value::Object(o) => record_key_suffix(o.get("id").unwrap_or(&null()), table),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn null() -> serde_json::Value {
    serde_json::Value::Null
}

fn j_str(v: &serde_json::Value, k: &str) -> String {
    v.get(k)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn j_opt_str(v: &serde_json::Value, k: &str) -> Option<String> {
    match v.get(k) {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn j_f64(v: &serde_json::Value, k: &str) -> f64 {
    v.get(k).and_then(serde_json::Value::as_f64).unwrap_or(0.0)
}

fn j_i32(v: &serde_json::Value, k: &str) -> i32 {
    v.get(k)
        .and_then(serde_json::Value::as_i64)
        .map(|x| x as i32)
        .unwrap_or(0)
}

/// is_active 缺失按 true(schema DEFAULT true;SQLite 侧 NOT NULL DEFAULT 1)。
fn j_bool_true(v: &serde_json::Value, k: &str) -> bool {
    v.get(k)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

/// array/object 字段 → 域侧 JSON 字符串(registry 契约;紧凑序列化,
/// 与 admin 写入口径一致)。空集序列化为 `"[]"` / `"{}"`,域侧
/// parse 失败与空串语义等价。
fn j_json_string(v: &serde_json::Value, k: &str) -> String {
    match v.get(k) {
        Some(value @ (serde_json::Value::Array(_) | serde_json::Value::Object(_))) => {
            serde_json::to_string(value).unwrap_or_else(|_| match value {
                serde_json::Value::Array(_) => "[]".to_string(),
                _ => "{}".to_string(),
            })
        }
        _ => String::new(),
    }
}

/// provider JSON 行 → 域类型。api_key 保持库内密文形态,key_decrypt_failed
/// 由选中后的解密方设置(同 provider_row.rs:26-28)。
fn provider_from_json(v: &serde_json::Value) -> Option<Provider> {
    if !v.is_object() {
        return None;
    }
    Some(Provider {
        id: record_key_suffix(v.get("id")?, "provider"),
        name: j_str(v, "name"),
        provider_type: j_str(v, "provider_type"),
        openai_base_url: j_str(v, "openai_base_url"),
        anthropic_base_url: j_str(v, "anthropic_base_url"),
        api_key: j_str(v, "api_key"),
        key_decrypt_failed: false,
        models: j_json_string(v, "models"),
        priority: j_i32(v, "priority"),
        weight: j_f64(v, "weight"),
        is_active: j_bool_true(v, "is_active"),
        health_status: j_str(v, "health_status"),
        latency_ms: j_f64(v, "latency_ms"),
        error_rate: j_f64(v, "error_rate"),
        last_health_check: j_opt_str(v, "last_health_check"),
        max_retries: j_i32(v, "max_retries"),
        timeout_secs: j_i32(v, "timeout_secs"),
        proxy_url: j_str(v, "proxy_url"),
        model_mapping: j_json_string(v, "model_mapping"),
        consecutive_failures: j_i32(v, "consecutive_failures"),
        disabled_reason: j_str(v, "disabled_reason"),
        protocols: j_json_string(v, "protocols"),
        default_protocol: j_str(v, "default_protocol"),
        note: j_str(v, "note"),
        website_url: j_str(v, "website_url"),
        channel_kind: j_str(v, "channel_kind"),
        created_at: j_str(v, "created_at"),
        updated_at: j_str(v, "updated_at"),
    })
}

/// RFC3339 字符串(datetime 字段的 JSON 形态)→ DateTime<Utc>。
fn parse_datetime(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc))
}

// ---------- 内部:写侧辅助 ----------

/// JSON 字符串 → Vec<String>(坏 JSON ⇒ 空,域侧读取同口径)。
fn parse_json_array(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

/// JSON 字符串 → object 值(坏 JSON ⇒ {})。
fn parse_json_object(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!({}))
}

/// surrealdb 错误 → 存储错误(细节只进日志,不进对外错误体)。
fn qerr(e: surrealdb::Error) -> StoreError {
    StoreError::Failed(e.to_string())
}

/// 单条查询取回 provider 行集。
async fn query_providers(
    db: &surrealdb::Surreal<surrealdb::engine::any::Any>,
    sql: &str,
) -> Result<Vec<Provider>, StoreError> {
    let mut res = db.query(sql).await.map_err(qerr)?;
    let rows: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
    Ok(rows.iter().filter_map(provider_from_json).collect())
}

impl SurrealChannels {
    /// 新行 record key:显式 id(迁移/复制)优先,否则 ULID(时间可排序)。
    fn new_key(explicit: &Option<String>) -> String {
        explicit
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| ulid::Ulid::new().to_string())
    }

    /// 建行(加密 + key_hash + 落库 + 回读),单条与批量共用。
    async fn insert_row(&self, row: NewProvider) -> Result<Provider, StoreError> {
        let key = Self::new_key(&row.id);
        // 无条件加密(含空 key;与 admin insert_checked 一致,密文与
        // SQLite 逐字节同构)。key_hash:空 key 为空串不参与查重。
        let api_key_stored = self.cipher.encrypt(&row.api_key_plain);
        let key_hash = if row.api_key_plain.is_empty() {
            String::new()
        } else {
            crate::password::hash_api_key(&row.api_key_plain)
        };
        let mut res = self
            .handle
            .db()
            .query(
                "CREATE type::record('provider', $key) SET
                    name = $name,
                    provider_type = $provider_type,
                    channel_kind = $channel_kind,
                    openai_base_url = $openai_base_url,
                    anthropic_base_url = $anthropic_base_url,
                    api_key = $api_key,
                    key_hash = $key_hash,
                    models = $models,
                    model_mapping = $model_mapping,
                    protocols = $protocols,
                    default_protocol = $default_protocol,
                    priority = $priority,
                    weight = $weight,
                    max_retries = $max_retries,
                    timeout_secs = $timeout_secs,
                    proxy_url = $proxy_url,
                    note = $note,
                    website_url = $website_url,
                    created_at = time::now(),
                    updated_at = time::now()
                 RETURN AFTER",
            )
            .bind(("key", key))
            .bind(("name", row.name))
            .bind(("provider_type", row.provider_type))
            .bind(("channel_kind", row.channel_kind))
            .bind(("openai_base_url", row.openai_base_url))
            .bind(("anthropic_base_url", row.anthropic_base_url))
            .bind(("api_key", api_key_stored))
            .bind(("key_hash", key_hash))
            .bind(("models", parse_json_array(&row.models_json)))
            .bind(("model_mapping", parse_json_object(&row.model_mapping_json)))
            .bind(("protocols", parse_json_array(&row.protocols_json)))
            .bind(("default_protocol", row.default_protocol))
            .bind(("priority", row.priority))
            .bind(("weight", row.weight))
            .bind(("max_retries", row.max_retries))
            .bind(("timeout_secs", row.timeout_secs))
            .bind(("proxy_url", row.proxy_url))
            .bind(("note", row.note))
            .bind(("website_url", row.website_url))
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
        rows.first()
            .and_then(provider_from_json)
            .ok_or_else(|| StoreError::Failed("create provider: 库未返回新行".to_string()))
    }
}

#[async_trait::async_trait]
impl ChannelStore for SurrealChannels {
    async fn active_channels(&self) -> Result<Vec<Provider>, StoreError> {
        query_providers(
            self.handle.db(),
            "SELECT * FROM provider WHERE is_active = true",
        )
        .await
    }

    async fn topology_token(&self) -> Result<CacheToken, StoreError> {
        // 三语句单往返。count() 在 WHERE 命中 0 行时返回 [](实测),
        // 一律按 0 处理;max(updated_at) 用 ORDER BY DESC LIMIT 1 取
        // (math::max 不收 datetime,实测)。
        let mut res = self
            .handle
            .db()
            .query(
                "SELECT count() AS n FROM provider GROUP ALL;
                 SELECT updated_at FROM provider ORDER BY updated_at DESC LIMIT 1;
                 SELECT count() AS u FROM provider WHERE health_status = 'unhealthy' GROUP ALL;",
            )
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        let count_rows: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
        let latest_rows: Vec<serde_json::Value> = res.take(1).map_err(qerr)?;
        let unhealthy_rows: Vec<serde_json::Value> = res.take(2).map_err(qerr)?;
        let n = count_rows
            .first()
            .and_then(|r| r.get("n"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let latest = latest_rows
            .first()
            .and_then(|r| j_opt_str(r, "updated_at"))
            .unwrap_or_default();
        let u = unhealthy_rows
            .first()
            .and_then(|r| r.get("u"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        Ok(CacheToken::new(format!("{n}|{latest}|{u}")))
    }

    async fn get_provider(&self, id: &str) -> Result<Option<Provider>, StoreError> {
        let mut res = self
            .handle
            .db()
            .query("SELECT * FROM type::record('provider', $id)")
            .bind(("id", id))
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
        Ok(rows.first().and_then(provider_from_json))
    }

    async fn list_providers(
        &self,
        q: &str,
        active_only: Option<bool>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<Provider>), StoreError> {
        // 动态 WHERE:字段名与结构都是静态片段,值全部走绑定参数。
        let mut cond: Vec<&'static str> = Vec::new();
        if !q.is_empty() {
            // SQLite LIKE 对 ASCII 不区分大小写;两侧 lowercase + CONTAINS 等价。
            cond.push("string::lowercase(name) CONTAINS $q");
        }
        if active_only.is_some() {
            cond.push("is_active = $active");
        }
        let where_clause = if cond.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", cond.join(" AND "))
        };
        let mut res = self
            .handle
            .db()
            .query(format!(
                "SELECT * FROM provider {where_clause}
                 ORDER BY created_at DESC LIMIT $limit START $offset;
                 SELECT count() AS total FROM provider {where_clause} GROUP ALL;"
            ))
            .bind(("q", q.to_ascii_lowercase()))
            .bind(("active", active_only))
            .bind(("limit", limit))
            .bind(("offset", offset))
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        let page: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
        let total_rows: Vec<serde_json::Value> = res.take(1).map_err(qerr)?;
        let total = total_rows
            .first()
            .and_then(|r| r.get("total"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        Ok((total, page.iter().filter_map(provider_from_json).collect()))
    }

    async fn create_provider(&self, row: NewProvider) -> Result<Provider, StoreError> {
        self.insert_row(row).await
    }

    async fn update_provider(
        &self,
        id: &str,
        patch: ProviderPatch,
    ) -> Result<Option<Provider>, StoreError> {
        // 动态 SET:字段名静态、值经 $pN 绑定;updated_at 恒 bump(令牌
        // 可见性,与 SQLite 动态 UPDATE 同构)。
        let mut sets: Vec<String> = Vec::new();
        let mut params = serde_json::Map::new();
        let p = |sets: &mut Vec<String>,
                 params: &mut serde_json::Map<String, serde_json::Value>,
                 name: &str,
                 v: serde_json::Value| {
            let var = format!("$p{}", params.len());
            params.insert(var.trim_start_matches('$').to_string(), v);
            sets.push(format!("{name} = {var}"));
        };
        if let Some(v) = patch.name {
            p(&mut sets, &mut params, "name", serde_json::Value::String(v));
        }
        if let Some(v) = patch.provider_type {
            p(
                &mut sets,
                &mut params,
                "provider_type",
                serde_json::Value::String(v),
            );
        }
        if let Some(v) = patch.channel_kind {
            p(
                &mut sets,
                &mut params,
                "channel_kind",
                serde_json::Value::String(v),
            );
        }
        // 三态:Some(None) = 清空,Some(Some(v)) = 设值(None = 不动)。
        if let Some(v) = patch.anthropic_base_url {
            let value = v.unwrap_or_default();
            p(
                &mut sets,
                &mut params,
                "anthropic_base_url",
                serde_json::Value::String(value),
            );
        }
        if let Some(plain) = patch.api_key_plain {
            // 换 key:密文与 key_hash 成对更新(同 admin PATCH)。
            let hash = if plain.is_empty() {
                String::new()
            } else {
                crate::password::hash_api_key(&plain)
            };
            p(
                &mut sets,
                &mut params,
                "api_key",
                serde_json::Value::String(self.cipher.encrypt(&plain)),
            );
            p(
                &mut sets,
                &mut params,
                "key_hash",
                serde_json::Value::String(hash),
            );
        }
        if let Some(v) = patch.models_json {
            p(
                &mut sets,
                &mut params,
                "models",
                serde_json::Value::Array(
                    parse_json_array(&v)
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
        }
        if let Some(v) = patch.model_mapping_json {
            p(
                &mut sets,
                &mut params,
                "model_mapping",
                parse_json_object(&v),
            );
        }
        if let Some(v) = patch.protocols_json {
            p(
                &mut sets,
                &mut params,
                "protocols",
                serde_json::Value::Array(
                    parse_json_array(&v)
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
        }
        if let Some(v) = patch.default_protocol {
            p(
                &mut sets,
                &mut params,
                "default_protocol",
                serde_json::Value::String(v),
            );
        }
        if let Some(v) = patch.priority {
            p(
                &mut sets,
                &mut params,
                "priority",
                serde_json::Value::from(v),
            );
        }
        if let Some(v) = patch.weight {
            p(&mut sets, &mut params, "weight", serde_json::Value::from(v));
        }
        if let Some(v) = patch.max_retries {
            p(
                &mut sets,
                &mut params,
                "max_retries",
                serde_json::Value::from(v),
            );
        }
        if let Some(v) = patch.timeout_secs {
            p(
                &mut sets,
                &mut params,
                "timeout_secs",
                serde_json::Value::from(v),
            );
        }
        if let Some(v) = patch.proxy_url {
            p(
                &mut sets,
                &mut params,
                "proxy_url",
                serde_json::Value::String(v),
            );
        }
        if let Some(v) = patch.note {
            p(&mut sets, &mut params, "note", serde_json::Value::String(v));
        }
        if let Some(v) = patch.website_url {
            p(
                &mut sets,
                &mut params,
                "website_url",
                serde_json::Value::String(v),
            );
        }
        if let Some(active) = patch.is_active {
            p(
                &mut sets,
                &mut params,
                "is_active",
                serde_json::Value::Bool(active),
            );
            if active {
                // 手动重启用:清自动禁用状态(SQLite 旧语义,main.rs 适配器)。
                sets.push("disabled_reason = ''".to_string());
                sets.push("consecutive_failures = 0".to_string());
            }
        }
        sets.push("updated_at = time::now()".to_string());
        let sql = format!(
            "UPDATE type::record('provider', $id) SET {} RETURN AFTER",
            sets.join(", ")
        );
        let mut res = self
            .handle
            .db()
            .query(&sql)
            .bind(("id", id))
            .bind(serde_json::Value::Object(params))
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
        Ok(rows.first().and_then(provider_from_json))
    }

    async fn delete_provider(&self, id: &str) -> Result<bool, StoreError> {
        // 显式级联删 model_health(Surreal 无 FK CASCADE);亲和残留行
        // 由选路自然失效(设计文档 §四)。RETURN BEFORE 取被删行判存在性
        // (二次删除返回 [])。
        let mut res = self
            .handle
            .db()
            .query(
                "DELETE model_health WHERE provider = type::record('provider', $id);
                 DELETE FROM type::record('provider', $id) RETURN BEFORE;",
            )
            .bind(("id", id))
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        let deleted: Vec<serde_json::Value> = res.take(1).map_err(qerr)?;
        Ok(!deleted.is_empty())
    }

    async fn create_providers_batch(
        &self,
        rows: Vec<NewProvider>,
    ) -> Vec<Result<Provider, StoreError>> {
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            // 重复检测 = key_hash 点查(空 key 不参与)。查重失败按
            // "无重复"处理并留痕,继续建行(admin insert_checked 的
            // `.ok()` 语义)。
            let plain = row.api_key_plain.clone();
            let duplicated = if plain.is_empty() {
                false
            } else {
                match self
                    .find_by_key_hash(&crate::password::hash_api_key(&plain))
                    .await
                {
                    Ok(owners) => !owners.is_empty(),
                    Err(e) => {
                        tracing::error!("create_providers_batch: key-hash lookup failed: {e}");
                        false
                    }
                }
            };
            if duplicated {
                out.push(Err(StoreError::Failed(
                    "duplicate provider key: 该 key 已用于其他渠道".to_string(),
                )));
                continue;
            }
            out.push(self.insert_row(row).await);
        }
        out
    }

    async fn find_by_key_hash(&self, key_hash: &str) -> Result<Vec<Provider>, StoreError> {
        if key_hash.is_empty() {
            // 空 key 不参与查重(否则命中全部无 key 渠道)。
            return Ok(Vec::new());
        }
        let mut res = self
            .handle
            .db()
            .query("SELECT * FROM provider WHERE key_hash = $h")
            .bind(("h", key_hash))
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
        Ok(rows.iter().filter_map(provider_from_json).collect())
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
        // EMA/健康字段写不 bump updated_at(文件头令牌口径);自动禁用
        // 写额外置 is_active/disabled_reason 并 bump updated_at(与
        // selector.rs 的禁用 UPDATE 一致,令牌立即可见)。
        let disable_clause = disabled_reason.is_some().then_some(
            "is_active = false,
                    disabled_reason = $reason,
                    updated_at = time::now()",
        );
        let sql = format!(
            "UPDATE type::record('provider', $id) SET
                    latency_ms = $latency,
                    error_rate = $error_rate,
                    health_status = $status,
                    consecutive_failures = $failures,
                    last_health_check = time::now(){}",
            disable_clause
                .map(|c| format!(",\n                    {c}"))
                .unwrap_or_default()
        );
        let mut q = self
            .handle
            .db()
            .query(&sql)
            .bind(("id", id))
            .bind(("latency", latency_ms))
            .bind(("error_rate", error_rate))
            .bind(("status", health_status))
            .bind(("failures", consecutive_failures));
        if let Some(reason) = disabled_reason {
            q = q.bind(("reason", reason));
        }
        q.await.map_err(qerr)?.check().map_err(qerr)?;
        Ok(())
    }

    async fn upsert_model_health(&self, row: ModelHealthRow) -> Result<(), StoreError> {
        // 数组 record id:[provider:<key>, '<model>'];provider 链接冗余存
        // (列表查询用)。UPSERT 幂等覆盖 = SQLite ON CONFLICT DO UPDATE。
        self.handle
            .db()
            .query(
                "UPSERT type::record('model_health', [type::record('provider', $pid), $model]) SET
                    provider = type::record('provider', $pid),
                    status = $status,
                    latency_ms = $latency,
                    error = $error,
                    checked_at = $checked_at",
            )
            .bind(("pid", row.provider_id))
            .bind(("model", row.model))
            .bind(("status", row.status))
            .bind(("latency", row.latency_ms))
            .bind(("error", row.error))
            .bind(("checked_at", row.checked_at))
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        Ok(())
    }

    async fn list_model_health(
        &self,
        provider_id: &str,
    ) -> Result<Vec<ModelHealthRow>, StoreError> {
        let mut res = self
            .handle
            .db()
            .query("SELECT * FROM model_health WHERE provider = type::record('provider', $pid)")
            .bind(("pid", provider_id))
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
        // schema 未存 model 字段(设计文档 §五:model 名在数组 id 第二段),
        // 从 id 字符串还原;provider key 从冗余 link 字段剥前缀。
        let mut out: Vec<ModelHealthRow> = rows
            .iter()
            .filter_map(|v| {
                let id = v.get("id")?.as_str()?;
                Some(ModelHealthRow {
                    provider_id: record_key_suffix(v.get("provider")?, "provider"),
                    model: model_from_health_id(id),
                    status: j_str(v, "status"),
                    latency_ms: j_f64(v, "latency_ms"),
                    error: j_str(v, "error"),
                    checked_at: j_opt_str(v, "checked_at")
                        .as_deref()
                        .and_then(parse_datetime),
                })
            })
            .collect();
        // 稳定序(SQLite 侧 ORDER BY provider_id, model;此处按 model)。
        out.sort_by_key(|r| r.model.clone());
        Ok(out)
    }

    async fn list_model_health_scoped(
        &self,
        provider_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<ModelHealthRow>), StoreError> {
        let (r#where, bs) = match provider_id.filter(|p| !p.is_empty()) {
            Some(_) => (
                "WHERE provider = type::record('provider', $pid) ".to_string(),
                true,
            ),
            None => (String::new(), false),
        };
        let mut q = self
            .handle
            .db()
            .query(format!(
                "SELECT * FROM model_health {W}                 ORDER BY provider, id LIMIT $limit START $offset;                 \nSELECT count() AS c FROM model_health {W}GROUP ALL;",
                W = r#where
            ))
            .bind(("limit", limit))
            .bind(("offset", offset));
        if bs {
            q = q.bind(("pid", provider_id.unwrap_or("")));
        }
        let mut res = q.await.map_err(qerr)?.check().map_err(qerr)?;
        let page: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
        let counts: Vec<serde_json::Value> = res.take(1).map_err(qerr)?;
        let total = counts
            .first()
            .and_then(|r| r.get("c"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let mut out: Vec<ModelHealthRow> = page
            .iter()
            .filter_map(|v| {
                let id = v.get("id")?.as_str()?;
                Some(ModelHealthRow {
                    provider_id: record_key_suffix(v.get("provider")?, "provider"),
                    model: model_from_health_id(id),
                    status: j_str(v, "status"),
                    latency_ms: j_f64(v, "latency_ms"),
                    error: j_str(v, "error"),
                    checked_at: j_opt_str(v, "checked_at")
                        .as_deref()
                        .and_then(parse_datetime),
                })
            })
            .collect();
        // provider 冗余字段是 record 链接,排序键取裸 key。
        out.sort_by(|a, b| (&a.provider_id, &a.model).cmp(&(&b.provider_id, &b.model)));
        Ok((total, out))
    }

    // ---- 集成批补充(默认切换装配面) ----

    async fn record_health_ping(&self, id: &str, status: &str) -> Result<(), StoreError> {
        // 只写 health_status + last_health_check(不碰 latency_ms/EMA,
        // 不 bump updated_at——与 SQLite 健康循环的 UPDATE 同列集)。
        self.handle
            .db()
            .query(
                "UPDATE type::record('provider', $id) SET
                    health_status = $status,
                    last_health_check = time::now()",
            )
            .bind(("id", id))
            .bind(("status", status))
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        Ok(())
    }

    async fn provider_metrics(&self, id: &str) -> Result<Option<ProviderMetrics>, StoreError> {
        let mut res = self
            .handle
            .db()
            .query(
                "SELECT latency_ms, error_rate, consecutive_failures \
                 FROM type::record('provider', $id)",
            )
            .bind(("id", id))
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
        Ok(rows.first().map(|r| ProviderMetrics {
            latency_ms: j_f64(r, "latency_ms"),
            error_rate: j_f64(r, "error_rate"),
            consecutive_failures: j_i32(r, "consecutive_failures"),
        }))
    }

    async fn read_provider_stored(
        &self,
        id: &str,
    ) -> Result<Option<StoredProviderConfig>, StoreError> {
        let mut res = self
            .handle
            .db()
            .query("SELECT * FROM type::record('provider', $id)")
            .bind(("id", id))
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
        Ok(rows.first().and_then(|r| {
            Some(StoredProviderConfig {
                name: j_str(r, "name"),
                provider_type: j_str(r, "provider_type"),
                openai_base_url: j_str(r, "openai_base_url"),
                anthropic_base_url: j_str(r, "anthropic_base_url"),
                api_key_stored: j_str(r, "api_key"),
                key_hash: j_str(r, "key_hash"),
                models_json: j_json_string(r, "models"),
                priority: r.get("priority").and_then(serde_json::Value::as_i64)?,
                weight: j_f64(r, "weight"),
                max_retries: r.get("max_retries").and_then(serde_json::Value::as_i64)?,
                timeout_secs: r.get("timeout_secs").and_then(serde_json::Value::as_i64)?,
                proxy_url: j_str(r, "proxy_url"),
                model_mapping_json: j_json_string(r, "model_mapping"),
                protocols_json: j_json_string(r, "protocols"),
                default_protocol: j_str(r, "default_protocol"),
                note: j_str(r, "note"),
                website_url: j_str(r, "website_url"),
                channel_kind: j_str(r, "channel_kind"),
            })
        }))
    }

    async fn insert_provider_stored(
        &self,
        id: &str,
        config: &StoredProviderConfig,
    ) -> Result<Provider, StoreError> {
        // 与 insert_row 同构,但密文/key_hash 由调用方给(管理面已加密)。
        let mut res = self
            .handle
            .db()
            .query(
                "CREATE type::record('provider', $key) SET
                    name = $name,
                    provider_type = $provider_type,
                    channel_kind = $channel_kind,
                    openai_base_url = $openai_base_url,
                    anthropic_base_url = $anthropic_base_url,
                    api_key = $api_key,
                    key_hash = $key_hash,
                    models = $models,
                    model_mapping = $model_mapping,
                    protocols = $protocols,
                    default_protocol = $default_protocol,
                    priority = $priority,
                    weight = $weight,
                    max_retries = $max_retries,
                    timeout_secs = $timeout_secs,
                    proxy_url = $proxy_url,
                    note = $note,
                    website_url = $website_url,
                    created_at = time::now(),
                    updated_at = time::now()
                 RETURN AFTER",
            )
            .bind(("key", id))
            .bind(("name", config.name.clone()))
            .bind(("provider_type", config.provider_type.clone()))
            .bind(("channel_kind", config.channel_kind.clone()))
            .bind(("openai_base_url", config.openai_base_url.clone()))
            .bind(("anthropic_base_url", config.anthropic_base_url.clone()))
            .bind(("api_key", config.api_key_stored.clone()))
            .bind(("key_hash", config.key_hash.clone()))
            .bind(("models", parse_json_array(&config.models_json)))
            .bind((
                "model_mapping",
                parse_json_object(&config.model_mapping_json),
            ))
            .bind(("protocols", parse_json_array(&config.protocols_json)))
            .bind(("default_protocol", config.default_protocol.clone()))
            .bind(("priority", config.priority))
            .bind(("weight", config.weight))
            .bind(("max_retries", config.max_retries))
            .bind(("timeout_secs", config.timeout_secs))
            .bind(("proxy_url", config.proxy_url.clone()))
            .bind(("note", config.note.clone()))
            .bind(("website_url", config.website_url.clone()))
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
        rows.first()
            .and_then(provider_from_json)
            .ok_or_else(|| StoreError::Failed("insert_provider_stored: 库未返回新行".to_string()))
    }

    async fn apply_provider_patch(
        &self,
        id: &str,
        patch: StoredProviderPatch,
    ) -> Result<Option<Provider>, StoreError> {
        // 动态 SET(同 update_provider 的手法);密文 key 与 hash 成对;
        // Some(true) 重启用清禁用态;恒 bump updated_at。
        let mut sets: Vec<String> = Vec::new();
        let mut params = serde_json::Map::new();
        let p = |sets: &mut Vec<String>,
                 params: &mut serde_json::Map<String, serde_json::Value>,
                 name: &str,
                 v: serde_json::Value| {
            let var = format!("$p{}", params.len());
            params.insert(var.trim_start_matches('$').to_string(), v);
            sets.push(format!("{name} = {var}"));
        };
        if let Some(v) = patch.name {
            p(&mut sets, &mut params, "name", serde_json::Value::String(v));
        }
        if let Some(v) = patch.provider_type {
            p(
                &mut sets,
                &mut params,
                "provider_type",
                serde_json::Value::String(v),
            );
        }
        if let Some(v) = patch.openai_base_url {
            p(
                &mut sets,
                &mut params,
                "openai_base_url",
                serde_json::Value::String(v),
            );
        }
        if let Some(v) = patch.anthropic_base_url {
            p(
                &mut sets,
                &mut params,
                "anthropic_base_url",
                serde_json::Value::String(v),
            );
        }
        if let Some(v) = patch.note {
            p(&mut sets, &mut params, "note", serde_json::Value::String(v));
        }
        if let Some(v) = patch.website_url {
            p(
                &mut sets,
                &mut params,
                "website_url",
                serde_json::Value::String(v),
            );
        }
        if let Some(v) = patch.channel_kind {
            p(
                &mut sets,
                &mut params,
                "channel_kind",
                serde_json::Value::String(v),
            );
        }
        if let Some(v) = patch.api_key_stored {
            p(
                &mut sets,
                &mut params,
                "api_key",
                serde_json::Value::String(v),
            );
        }
        if let Some(v) = patch.key_hash {
            p(
                &mut sets,
                &mut params,
                "key_hash",
                serde_json::Value::String(v),
            );
        }
        if let Some(v) = patch.models_json {
            p(
                &mut sets,
                &mut params,
                "models",
                serde_json::Value::Array(
                    parse_json_array(&v)
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
        }
        if let Some(v) = patch.model_mapping_json {
            p(
                &mut sets,
                &mut params,
                "model_mapping",
                parse_json_object(&v),
            );
        }
        if let Some(v) = patch.protocols_json {
            p(
                &mut sets,
                &mut params,
                "protocols",
                serde_json::Value::Array(
                    parse_json_array(&v)
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
        }
        if let Some(v) = patch.default_protocol {
            p(
                &mut sets,
                &mut params,
                "default_protocol",
                serde_json::Value::String(v),
            );
        }
        if let Some(v) = patch.priority {
            p(
                &mut sets,
                &mut params,
                "priority",
                serde_json::Value::from(v),
            );
        }
        if let Some(v) = patch.weight {
            p(&mut sets, &mut params, "weight", serde_json::Value::from(v));
        }
        if let Some(v) = patch.max_retries {
            p(
                &mut sets,
                &mut params,
                "max_retries",
                serde_json::Value::from(v),
            );
        }
        if let Some(v) = patch.timeout_secs {
            p(
                &mut sets,
                &mut params,
                "timeout_secs",
                serde_json::Value::from(v),
            );
        }
        if let Some(v) = patch.proxy_url {
            p(
                &mut sets,
                &mut params,
                "proxy_url",
                serde_json::Value::String(v),
            );
        }
        if let Some(active) = patch.is_active {
            p(
                &mut sets,
                &mut params,
                "is_active",
                serde_json::Value::Bool(active),
            );
            if active {
                sets.push("disabled_reason = ''".to_string());
                sets.push("consecutive_failures = 0".to_string());
            }
        }
        sets.push("updated_at = time::now()".to_string());
        let sql = format!(
            "UPDATE type::record('provider', $id) SET {} RETURN AFTER",
            sets.join(", ")
        );
        let mut res = self
            .handle
            .db()
            .query(&sql)
            .bind(("id", id))
            .bind(serde_json::Value::Object(params))
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
        Ok(rows.first().and_then(provider_from_json))
    }

    async fn list_providers_by_priority(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<Provider>), StoreError> {
        let mut res = self
            .handle
            .db()
            .query(
                "SELECT * FROM provider ORDER BY priority DESC, name ASC \
                 LIMIT $limit START $offset;\
                 \nSELECT count() AS total FROM provider GROUP ALL;",
            )
            .bind(("limit", limit))
            .bind(("offset", offset))
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        let page: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
        let total_rows: Vec<serde_json::Value> = res.take(1).map_err(qerr)?;
        let total = total_rows
            .first()
            .and_then(|r| r.get("total"))
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        Ok((total, page.iter().filter_map(provider_from_json).collect()))
    }

    async fn all_channel_models(&self) -> Result<std::collections::HashSet<String>, StoreError> {
        let mut res = self
            .handle
            .db()
            .query("SELECT models FROM provider")
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
        let mut set = std::collections::HashSet::new();
        for r in &rows {
            if let Some(arr) = r.get("models").and_then(serde_json::Value::as_array) {
                set.extend(arr.iter().filter_map(|m| m.as_str().map(str::to_string)));
            }
        }
        Ok(set)
    }

    async fn find_key_owner(
        &self,
        key_hash: &str,
        exclude_id: &str,
    ) -> Result<Option<String>, StoreError> {
        if key_hash.is_empty() {
            return Ok(None);
        }
        let mut res = self
            .handle
            .db()
            .query(
                "SELECT name FROM provider \
                 WHERE key_hash = $h AND id != type::record('provider', $ex) LIMIT 1",
            )
            .bind(("h", key_hash))
            .bind(("ex", exclude_id))
            .await
            .map_err(qerr)?
            .check()
            .map_err(qerr)?;
        let rows: Vec<serde_json::Value> = res.take(0).map_err(qerr)?;
        Ok(rows
            .first()
            .map(|r| j_str(r, "name"))
            .filter(|s| !s.is_empty()))
    }
}

/// `"model_health:[provider:xx, 'gpt-4']"` → `"gpt-4"`。model 是数组 id
/// 的末段:首个 ", " 之后到结尾(内层 provider record key 为 ULID/uuid,
/// 不含逗号与空格);字符串 key 序列化可能带单引号包裹,一并剥掉。
fn model_from_health_id(id: &str) -> String {
    let inner = id
        .strip_prefix("model_health:[")
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or("");
    match inner.split_once(", ") {
        Some((_, model)) => model.trim_matches('\'').to_string(),
        None => String::new(),
    }
}

// ---------- 单测(mem:// 进程内实例,设计文档 §七第 2 层) ----------

#[cfg(test)]
mod tests {
    use super::*;

    /// mem:// 实例 + schema(FLEXIBLE 已落 SCHEMA_SURQL,无需夹具补丁)。
    async fn fixture() -> SurrealChannels {
        let ep = crate::surreal::Endpoint::parse("mem://").unwrap();
        let handle = crate::surreal::SurrealHandle::connect(&ep).await.unwrap();
        handle.ensure_schema().await.unwrap();
        SurrealChannels::new(handle, crate::crypto::KeyCipher::from_secret("test-secret"))
    }

    /// 嵌入式引擎查询在调用任务栈上内联递归,须在大栈线程跑(同
    /// surreal/mod.rs 的 mem_connect_schema_and_gate)。
    fn big_stack<T>(fut: impl std::future::Future<Output = T> + Send + 'static) -> T
    where
        T: Send + 'static,
    {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("test runtime");
                rt.block_on(fut)
            })
            .expect("spawn big-stack thread")
            .join()
            .expect("join test thread")
    }

    fn new_provider(name: &str, key: &str) -> NewProvider {
        NewProvider {
            id: None,
            name: name.to_string(),
            provider_type: "openai".to_string(),
            channel_kind: String::new(),
            openai_base_url: "http://upstream".to_string(),
            anthropic_base_url: String::new(),
            api_key_plain: key.to_string(),
            models_json: r#"["gpt-4","gpt-4o"]"#.to_string(),
            model_mapping_json: r#"{"claude-":"gpt-4"}"#.to_string(),
            protocols_json: r#"["openai"]"#.to_string(),
            default_protocol: "openai".to_string(),
            priority: 5,
            weight: 1.5,
            max_retries: 2,
            timeout_secs: 30,
            proxy_url: String::new(),
            note: String::new(),
            website_url: String::new(),
        }
    }

    /// CRUD 全链路:密文形态、key_hash、JSON 字段往返、查重、缺失读。
    #[test]
    fn crud_roundtrip_ciphertext_and_json_fields() {
        big_stack(async {
            let store = fixture().await;
            let created = store
                .create_provider(new_provider("p1", "sk-plain-1"))
                .await
                .unwrap();

            // 密文形态:enc:v1: 前缀、不含明文、同 cipher 可解回(密文与
            // SQLite 侧逐字节同构的前提)。
            assert!(
                created.api_key.starts_with("enc:v1:"),
                "got {}",
                created.api_key
            );
            assert!(!created.api_key.contains("sk-plain-1"));
            let cipher = crate::crypto::KeyCipher::from_secret("test-secret");
            assert_eq!(
                cipher.decrypt(&created.api_key).as_deref(),
                Some("sk-plain-1")
            );

            // key_hash = sha256(明文) 十六进制。
            assert_eq!(
                store
                    .find_by_key_hash(&crate::password::hash_api_key("sk-plain-1"))
                    .await
                    .unwrap()
                    .len(),
                1
            );
            // 空 key 不参与查重。
            assert!(store.find_by_key_hash("").await.unwrap().is_empty());

            // JSON 字段往返:array/object → 域侧 JSON 字符串(紧凑形态)。
            assert_eq!(created.models, r#"["gpt-4","gpt-4o"]"#);
            assert_eq!(created.model_mapping, r#"{"claude-":"gpt-4"}"#);
            assert_eq!(created.protocols, r#"["openai"]"#);
            assert_eq!(created.provider_type, "openai");
            assert_eq!(created.priority, 5);
            assert_eq!(created.weight, 1.5);
            assert_eq!(created.max_retries, 2);
            assert_eq!(created.timeout_secs, 30);
            assert!(created.is_active);
            assert_eq!(created.health_status, "unknown");
            assert_eq!(created.latency_ms, 0.0);
            assert_eq!(created.consecutive_failures, 0);

            // 单读:命中与缺失。
            let got = store.get_provider(&created.id).await.unwrap().unwrap();
            assert_eq!(got.id, created.id);
            assert_eq!(got.api_key, created.api_key);
            assert!(!got.key_decrypt_failed);
            assert!(store.get_provider("no-such-id").await.unwrap().is_none());

            // 删除:存在 ⇒ true + 消失;再删 ⇒ false。
            assert!(store.delete_provider(&created.id).await.unwrap());
            assert!(store.get_provider(&created.id).await.unwrap().is_none());
            assert!(!store.delete_provider(&created.id).await.unwrap());
        });
    }

    /// PATCH 语义:三态 base_url、换 key 成对更新、重启清禁用态。
    #[test]
    fn patch_semantics_three_state_and_reenable() {
        big_stack(async {
            let store = fixture().await;
            let p = store
                .create_provider(new_provider("p1", "sk-1"))
                .await
                .unwrap();

            // 常规字段 + 三态:设值。
            let updated = store
                .update_provider(
                    &p.id,
                    ProviderPatch {
                        name: Some("renamed".into()),
                        anthropic_base_url: Some(Some("http://anthro".into())),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(updated.name, "renamed");
            assert_eq!(updated.anthropic_base_url, "http://anthro");

            // 三态:清空。
            let updated = store
                .update_provider(
                    &p.id,
                    ProviderPatch {
                        anthropic_base_url: Some(None),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(updated.anthropic_base_url, "");

            // 换 key:密文 + key_hash 成对更新。
            store
                .update_provider(
                    &p.id,
                    ProviderPatch {
                        api_key_plain: Some("sk-2".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                store
                    .find_by_key_hash(&crate::password::hash_api_key("sk-1"))
                    .await
                    .unwrap()
                    .len(),
                0,
                "旧 hash 不再命中"
            );
            assert_eq!(
                store
                    .find_by_key_hash(&crate::password::hash_api_key("sk-2"))
                    .await
                    .unwrap()
                    .len(),
                1
            );

            // 先打上禁用态,再 Some(true) 重启:清 disabled_reason 与
            // consecutive_failures(SQLite 旧语义)。
            store
                .record_channel_result(&p.id, 10.0, 0.1, "unhealthy", 5, Some("测试禁用"))
                .await
                .unwrap();
            let disabled = store.get_provider(&p.id).await.unwrap().unwrap();
            assert!(!disabled.is_active);
            assert_eq!(disabled.disabled_reason, "测试禁用");
            assert_eq!(disabled.consecutive_failures, 5);

            let reenabled = store
                .update_provider(
                    &p.id,
                    ProviderPatch {
                        is_active: Some(true),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .unwrap();
            assert!(reenabled.is_active);
            assert_eq!(reenabled.disabled_reason, "");
            assert_eq!(reenabled.consecutive_failures, 0);

            // 缺失 id:None。
            assert!(
                store
                    .update_provider("no-such", ProviderPatch::default())
                    .await
                    .unwrap()
                    .is_none()
            );

            // updated_at 恒 bump:PATCH 后行内时间戳非空。
            assert!(!reenabled.updated_at.is_empty());
        });
    }

    /// 令牌口径:行数/最大 updated_at/unhealthy 计数;EMA 热路径写不
    /// bump updated_at(令牌不变),健康跃迁与 CRUD 写可见。
    #[test]
    fn topology_token_tracks_changes_without_ema_bump() {
        big_stack(async {
            let store = fixture().await;
            let t0 = store.topology_token().await.unwrap();
            assert_eq!(t0, store.topology_token().await.unwrap(), "空表令牌稳定");

            let p = store
                .create_provider(new_provider("p1", "sk-1"))
                .await
                .unwrap();
            let t1 = store.topology_token().await.unwrap();
            assert_ne!(t0, t1, "建行可见");

            store
                .update_provider(
                    &p.id,
                    ProviderPatch {
                        name: Some("renamed".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let t2 = store.topology_token().await.unwrap();
            assert_ne!(t1, t2, "管理端写(updated_at bump)可见");

            // EMA 写:不动 updated_at、不动行数、health 不跃迁 ⇒ 令牌不变
            // (每请求一条 EMA 写不能触发全量重载)。
            store
                .record_channel_result(&p.id, 123.0, 0.05, "healthy", 0, None)
                .await
                .unwrap();
            assert_eq!(
                t2,
                store.topology_token().await.unwrap(),
                "EMA 热路径写不进令牌"
            );

            // 健康跃迁(unhealthy 计数)可见。
            store
                .record_channel_result(&p.id, 123.0, 0.05, "unhealthy", 1, None)
                .await
                .unwrap();
            let t3 = store.topology_token().await.unwrap();
            assert_ne!(t2, t3, "健康跃迁可见");

            assert!(store.delete_provider(&p.id).await.unwrap());
            assert_ne!(t3, store.topology_token().await.unwrap(), "删行可见");
        });
    }

    /// 批量创建:逐条结果;key_hash 重复检测(库里已有同 key ⇒ 该项失败)。
    #[test]
    fn batch_create_duplicate_detection() {
        big_stack(async {
            let store = fixture().await;
            let results = store
                .create_providers_batch(vec![
                    new_provider("a", "sk-dup"),
                    new_provider("b", "sk-dup"),
                    new_provider("c", "sk-uniq"),
                ])
                .await;
            assert_eq!(results.len(), 3);
            assert!(results[0].is_ok());
            assert!(results[1].is_err(), "批内第二个同 key 被点查拦下");
            assert!(results[2].is_ok());
            assert_eq!(
                store
                    .find_by_key_hash(&crate::password::hash_api_key("sk-dup"))
                    .await
                    .unwrap()
                    .len(),
                1
            );
            // 空 key 不参与查重:两个无 key 渠道都建得成。
            let results = store
                .create_providers_batch(vec![new_provider("x", ""), new_provider("y", "")])
                .await;
            assert!(results.iter().all(|r| r.is_ok()));
        });
    }

    /// EMA/健康回写与自动禁用:数值原样落库;禁用写 is_active=false +
    /// disabled_reason + updated_at bump(令牌/选路面立即可见)。
    #[test]
    fn record_channel_result_autodisable() {
        big_stack(async {
            let store = fixture().await;
            let p = store
                .create_provider(new_provider("p1", "sk-1"))
                .await
                .unwrap();

            store
                .record_channel_result(&p.id, 88.5, 0.05, "degraded", 2, None)
                .await
                .unwrap();
            let row = store.get_provider(&p.id).await.unwrap().unwrap();
            assert_eq!(row.latency_ms, 88.5, "EMA 由调用侧计算,这里只落值");
            assert_eq!(row.error_rate, 0.05);
            assert_eq!(row.health_status, "degraded");
            assert_eq!(row.consecutive_failures, 2);
            assert!(row.last_health_check.is_some(), "观测时间随写刷新");
            assert!(row.is_active, "无禁用原因不禁用");
            assert_eq!(row.disabled_reason, "");

            // 自动禁用(阈值判定在调用方)。
            store
                .record_channel_result(&p.id, 90.0, 0.2, "unhealthy", 5, Some("连续失败 5 次"))
                .await
                .unwrap();
            let row = store.get_provider(&p.id).await.unwrap().unwrap();
            assert!(!row.is_active);
            assert_eq!(row.disabled_reason, "连续失败 5 次");
            assert_eq!(row.latency_ms, 90.0);
            assert!(
                store.active_channels().await.unwrap().is_empty(),
                "禁用渠道不进快照"
            );
        });
    }

    /// 模型健康:UPSERT 幂等覆盖、model 名从数组 id 还原、渠道删除级联清理。
    #[test]
    fn model_health_upsert_list_and_delete_cascade() {
        big_stack(async {
            let store = fixture().await;
            let p = store
                .create_provider(new_provider("p1", "sk-1"))
                .await
                .unwrap();
            let other = store
                .create_provider(new_provider("p2", "sk-2"))
                .await
                .unwrap();

            let row = ModelHealthRow {
                provider_id: p.id.clone(),
                model: "gpt-4".into(),
                status: "healthy".into(),
                latency_ms: 120.5,
                error: String::new(),
                checked_at: Some(chrono::Utc::now()),
            };
            store.upsert_model_health(row.clone()).await.unwrap();
            store
                .upsert_model_health(ModelHealthRow {
                    status: "unhealthy".into(),
                    latency_ms: 999.0,
                    error: "boom".into(),
                    checked_at: None,
                    ..row.clone()
                })
                .await
                .unwrap();
            store
                .upsert_model_health(ModelHealthRow {
                    model: "gpt-4o".into(),
                    status: "healthy".into(),
                    latency_ms: 50.0,
                    error: String::new(),
                    checked_at: None,
                    ..row.clone()
                })
                .await
                .unwrap();

            let list = store.list_model_health(&p.id).await.unwrap();
            assert_eq!(list.len(), 2, "同 (provider, model) 覆盖不重复");
            assert!(list.iter().all(|r| r.provider_id == p.id));
            let gpt4 = list.iter().find(|r| r.model == "gpt-4").unwrap();
            assert_eq!(gpt4.status, "unhealthy");
            assert_eq!(gpt4.latency_ms, 999.0);
            assert_eq!(gpt4.error, "boom");
            assert!(gpt4.checked_at.is_none());
            let gpt4o = list.iter().find(|r| r.model == "gpt-4o").unwrap();
            assert_eq!(gpt4o.status, "healthy");
            assert!(store.list_model_health(&other.id).await.unwrap().is_empty());

            // 删除渠道 ⇒ model_health 级联清理(数组 id 前缀行)。
            assert!(store.delete_provider(&p.id).await.unwrap());
            assert!(store.list_model_health(&p.id).await.unwrap().is_empty());
        });
    }

    /// 列表:q 大小写不敏感模糊、is_active 过滤、分页与 total、
    /// created_at DESC 序(最新建的在最前)。
    #[test]
    fn list_providers_filter_paging_and_order() {
        big_stack(async {
            let store = fixture().await;
            let a = store
                .create_provider(new_provider("Alpha One", "k1"))
                .await
                .unwrap();
            let _b = store
                .create_provider(new_provider("beta two", "k2"))
                .await
                .unwrap();
            let c = store
                .create_provider(new_provider("Gamma Three", "k3"))
                .await
                .unwrap();

            let (total, page) = store.list_providers("", None, 10, 0).await.unwrap();
            assert_eq!(total, 3);
            assert_eq!(page.len(), 3);
            // created_at DESC:最后建的最前(微秒级时间戳,顺序可判)。
            assert_eq!(page[0].id, c.id);
            assert_eq!(page[2].id, a.id);

            // 大小写不敏感模糊。
            let (total, page) = store.list_providers("aLpH", None, 10, 0).await.unwrap();
            assert_eq!(total, 1);
            assert_eq!(page[0].name, "Alpha One");

            // is_active 过滤。
            store
                .update_provider(
                    &c.id,
                    ProviderPatch {
                        is_active: Some(false),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let (total, page) = store.list_providers("", Some(true), 10, 0).await.unwrap();
            assert_eq!(total, 2);
            assert!(page.iter().all(|p| p.is_active));
            let (total, page) = store.list_providers("", Some(false), 10, 0).await.unwrap();
            assert_eq!(total, 1);
            assert_eq!(page[0].id, c.id);

            // 分页。
            let (total, page) = store.list_providers("", None, 2, 0).await.unwrap();
            assert_eq!((total, page.len()), (3, 2));
            let (total, page) = store.list_providers("", None, 2, 2).await.unwrap();
            assert_eq!((total, page.len()), (3, 1));

            // active_channels 只含启用渠道。
            assert_eq!(store.active_channels().await.unwrap().len(), 2);
        });
    }

    /// 集成批补充方法:密文原样建行/回读、PATCH、priority 序、模型集合、
    /// key_hash 归属、健康 ping 与 EMA 读侧。
    #[test]
    fn integration_stored_config_patch_and_helpers() {
        big_stack(async {
            let store = fixture().await;
            // 密文原样建行(不经 encrypt;模拟管理面已加密的入库路径)
            let cfg = StoredProviderConfig {
                name: "p1".into(),
                provider_type: "openai".into(),
                openai_base_url: "http://up".into(),
                api_key_stored: "enc:v1:cipher-blob".into(),
                key_hash: "hash-1".into(),
                models_json: r#"["gpt-4"]"#.into(),
                model_mapping_json: r#"{"claude-":"gpt-4"}"#.into(),
                protocols_json: r#"["openai"]"#.into(),
                priority: 7,
                ..Default::default()
            };
            let created = store.insert_provider_stored("prov-a", &cfg).await.unwrap();
            assert_eq!(created.id, "prov-a");
            assert_eq!(created.api_key, "enc:v1:cipher-blob", "密文原样落库");

            // 回读:密文 + key_hash + JSON 字段原样往返
            let stored = store.read_provider_stored("prov-a").await.unwrap().unwrap();
            assert_eq!(stored.api_key_stored, "enc:v1:cipher-blob");
            assert_eq!(stored.key_hash, "hash-1");
            assert_eq!(stored.models_json, r#"["gpt-4"]"#);
            assert_eq!(stored.model_mapping_json, r#"{"claude-":"gpt-4"}"#);
            assert_eq!(stored.priority, 7);
            assert!(
                store
                    .read_provider_stored("missing")
                    .await
                    .unwrap()
                    .is_none()
            );

            // PATCH:密文成对换 + 重启用清禁用态
            store
                .record_channel_result("prov-a", 5.0, 0.2, "unhealthy", 3, Some("禁用"))
                .await
                .unwrap();
            let updated = store
                .apply_provider_patch(
                    "prov-a",
                    StoredProviderPatch {
                        api_key_stored: Some("enc:v2:new".into()),
                        key_hash: Some("hash-2".into()),
                        is_active: Some(true),
                        priority: Some(9),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .unwrap();
            assert!(updated.is_active);
            assert_eq!(updated.disabled_reason, "", "重启用清禁用态");
            assert_eq!(updated.consecutive_failures, 0);
            assert_eq!(
                store
                    .read_provider_stored("prov-a")
                    .await
                    .unwrap()
                    .unwrap()
                    .key_hash,
                "hash-2"
            );

            // priority 序:name 并列决胜
            store
                .insert_provider_stored(
                    "prov-b",
                    &StoredProviderConfig {
                        name: "b".into(),
                        openai_base_url: "http://up".into(),
                        priority: 3,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let (total, page) = store.list_providers_by_priority(10, 0).await.unwrap();
            assert_eq!(total, 2);
            assert_eq!(page[0].id, "prov-a", "priority 9 > 3");

            // 模型集合(含禁用渠道)与 key 归属
            let models = store.all_channel_models().await.unwrap();
            assert!(models.contains("gpt-4"));
            store
                .insert_provider_stored(
                    "prov-c",
                    &StoredProviderConfig {
                        name: "c".into(),
                        openai_base_url: "http://up".into(),
                        api_key_stored: "enc".into(),
                        key_hash: "hash-1".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                store.find_key_owner("hash-1", "prov-a").await.unwrap(),
                Some("c".into())
            );
            assert_eq!(
                store.find_key_owner("hash-2", "prov-a").await.unwrap(),
                None,
                "排除自身后无归属"
            );

            // 健康 ping:只动 health_status/last_health_check
            store
                .record_health_ping("prov-a", "degraded")
                .await
                .unwrap();
            let m = store.provider_metrics("prov-a").await.unwrap().unwrap();
            assert_eq!(m.latency_ms, 5.0, "ping 不覆盖 EMA");
            let after = store.get_provider("prov-a").await.unwrap().unwrap();
            assert_eq!(after.health_status, "degraded");
            assert!(store.provider_metrics("missing").await.unwrap().is_none());
        });
    }
}
