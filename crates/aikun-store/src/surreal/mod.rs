//! # SurrealDB 后端:连接管理 + schemafull schema(阶段 S 骨架)
//!
//! 设计与决策全量见 `docs/surreal-migration.md`;本文件是三路并行工人的
//! **共享底座**,由设计工人落地,工人**不得修改本文件**(纪律见设计文档
//! §七)——需要新表/新字段/新索引时在设计文档记待办,由集成批统一改。
//!
//! ## 职责边界
//!
//! * [`Endpoint`]:`AIKUN_SURREAL_URL` / `--surreal-url` 的解析结果
//!   (嵌入式 SurrealKV 目录、内存库、远程 ws/wss/http/https)。
//! * [`SurrealHandle`]:Any 引擎连接(地址字符串在运行期决定引擎,
//!   surrealdb 4.0.1 `engine/any/mod.rs` 的 `IntoEndpoint` 实证:
//!   `surrealkv://<dir>` → 嵌入式 SurrealKV,`ws*`/`http*` → 远程),
//!   命名空间/库约定(默认 `aikun`/`aikun`,URL 参数 `?ns=&db=` 覆盖),
//!   远程 root 登录(`?user=&pass=`)。
//! * [`SCHEMA_SURQL`] + [`ensure_schema`]:schemafull 表集与版本闸门
//!   (替代 SQLite `PRAGMA user_version`,降级运行拒绝启动)。
//!
//! ## 选型钉版(证据链见设计文档 §一)
//!
//! `surrealdb = "4.0.1"`(crates.io 2026-11 实查最新稳定线,2026-11-09
//! 发布),features = 默认面(protocol-ws + rustls)+ `kv-surrealkv` +
//! `protocol-http`。4.0 起嵌入式在盘引擎为 SurrealKV(`kv-rocksdb`/
//! `kv-indxdb` 已移除),Rust 原生无 C++ 工具链要求。
//!
//! ## 纪律:SDK 类型不出本模块的公共能力面
//!
//! 与 SQLite 侧同规(`crate::lib` 文件头):能力 trait(`identity.rs` /
//! `channels_store.rs` / `billing_store.rs`)签名只出现领域类型与
//! `&str` 不透明 ID;`Surreal<Any>` / `RecordId` 等 SDK 类型只在
//! 本模块与各 impl 文件**内部**流转。

pub mod billing_store;
pub mod channels_store;
pub mod identity;

use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::opt::auth::Root;

use crate::StoreError;

/// 降级运行的拒绝错误(ensure_schema 闸门两处共用)。
fn newer_schema_error(v: i64) -> StoreError {
    StoreError::Failed(format!(
        "SurrealDB schema 版本 v{v} 比本程序支持的 v{SUPPORTED_SCHEMA_VERSION} 新,请升级 aikun(禁止降级运行)"
    ))
}

/// 默认命名空间与库名(单二进制部署约定;URL `?ns=&db=` 可覆盖)。
pub const DEFAULT_NAMESPACE: &str = "aikun";
pub const DEFAULT_DATABASE: &str = "aikun";

/// Surreal 侧 schema 版本(语义同 SQLite `SCHEMA_VERSION`,schema.rs:
/// "库版本比本程序新即拒绝启动"的降级闸门)。每次不向后兼容的 schema
/// 变更 +1;写入库的唯一位置是 [`ensure_schema`]。
pub const SUPPORTED_SCHEMA_VERSION: i64 = 1;

// ---------- 端点解析 ----------

/// `AIKUN_SURREAL_URL` 的解析结果。地址字符串同时是 Any 引擎的直接
/// 输入(见 `engine/any` 的 connect),二者由 [`Endpoint::parse`] 保证一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// Any 引擎可消费的连接地址(query 参数已剥离)。
    pub address: String,
    /// 命名空间(默认 [`DEFAULT_NAMESPACE`])。
    pub ns: String,
    /// 库名(默认 [`DEFAULT_DATABASE`])。
    pub db: String,
    /// 远程 root 登录凭据(`?user=&pass=`;嵌入式/内存库忽略)。
    pub auth: Option<(String, String)>,
    /// 引擎类别(日志与校验用)。
    pub kind: EndpointKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    /// 嵌入式 SurrealKV(目录;4.0 起在盘引擎唯一选项)。
    SurrealKv,
    /// 进程内内存库(测试用)。
    Mem,
    /// 远程 WebSocket(默认远程协议)。
    Ws,
    /// 远程 WebSocket over TLS。
    Wss,
    /// 远程 HTTP。
    Http,
    /// 远程 HTTPS。
    Https,
}

impl Endpoint {
    /// 解析连接端点。合法形式:
    ///
    /// * `surrealkv://<目录路径>`(嵌入式;如 `surrealkv://./aikun.surreal`)
    /// * `mem://` 或 `memory`(进程内内存库,测试用)
    /// * `ws://host:port` / `wss://host:port` / `http://…` / `https://…`(远程)
    /// * 以上任一形式可带 `?ns=<ns>&db=<db>[&user=<u>&pass=<p>]`
    ///
    /// 非法 scheme、空嵌入式路径、只给 user 不给 pass(或反之)均为
    /// `Err`;调用方(config.rs)以此**拒绝启动**,不静默回退。
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let (base, query) = match raw.split_once('?') {
            Some((b, q)) => (b, q),
            None => (raw, ""),
        };
        if base.is_empty() {
            return Err("SurrealDB 端点为空".to_string());
        }

        let mut ns = DEFAULT_NAMESPACE.to_string();
        let mut db = DEFAULT_DATABASE.to_string();
        let mut user: Option<String> = None;
        let mut pass: Option<String> = None;
        for pair in query.split('&').filter(|s| !s.is_empty()) {
            let (k, v) = pair
                .split_once('=')
                .ok_or_else(|| format!("SurrealDB 端点 query 参数非法(应为 k=v):{pair:?}"))?;
            match k {
                "ns" => ns = v.to_string(),
                "db" => db = v.to_string(),
                "user" => user = Some(v.to_string()),
                "pass" => pass = Some(v.to_string()),
                other => {
                    return Err(format!(
                        "SurrealDB 端点不支持 query 参数 {other:?}(可用:ns/db/user/pass)"
                    ));
                }
            }
        }
        let auth = match (user, pass) {
            (Some(u), Some(p)) => Some((u, p)),
            (None, None) => None,
            _ => {
                return Err("SurrealDB 端点的 user/pass 必须成对出现(只给其一无法登录)".to_string());
            }
        };

        let lower = base.to_ascii_lowercase();
        let (kind, address) = if lower == "memory" || lower == "mem://" {
            (EndpointKind::Mem, "mem://".to_string())
        } else if lower.starts_with("surrealkv://") {
            // 路径从原文取(lower 只用于 scheme 判定,目录名可能大小写敏感)
            let path = &base["surrealkv://".len()..];
            if path.is_empty() {
                return Err(
                    "surrealkv:// 端点缺少目录路径(如 surrealkv://./aikun.surreal)".to_string(),
                );
            }
            (EndpointKind::SurrealKv, format!("surrealkv://{path}"))
        } else if lower.starts_with("ws://") {
            (EndpointKind::Ws, base.to_string())
        } else if lower.starts_with("wss://") {
            (EndpointKind::Wss, base.to_string())
        } else if lower.starts_with("http://") {
            (EndpointKind::Http, base.to_string())
        } else if lower.starts_with("https://") {
            (EndpointKind::Https, base.to_string())
        } else {
            return Err(format!(
                "SurrealDB 端点 scheme 非法:{base:?}(可用:surrealkv://<目录> | mem:// | ws:// | wss:// | http:// | https://)"
            ));
        };
        Ok(Self {
            address,
            ns,
            db,
            auth,
            kind,
        })
    }

    /// 嵌入式默认端点:工作目录下的 `./aikun.surreal` 目录(位置语义沿用
    /// `AIKUN_DATABASE_URL` 的"默认工作目录"约定,见设计文档 §二)。
    pub fn embedded_default() -> Self {
        Self {
            address: "surrealkv://./aikun.surreal".to_string(),
            ns: DEFAULT_NAMESPACE.to_string(),
            db: DEFAULT_DATABASE.to_string(),
            auth: None,
            kind: EndpointKind::SurrealKv,
        }
    }
}

// ---------- 连接管理 ----------

/// 进程唯一的 SurrealDB 句柄。`Clone` 廉价(SDK 内部 Arc);三个能力
/// 实现(identity / channels / billing)各持一份克隆,互不共享可变状态。
#[derive(Clone)]
pub struct SurrealHandle {
    db: Surreal<Any>,
    endpoint: Endpoint,
}

impl std::fmt::Debug for SurrealHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SurrealHandle")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl SurrealHandle {
    /// 连接端点并完成:引擎握手 → (远程)root 登录 → 选定 ns/db。
    /// 任何一步失败即返回错误;调用方(启动路径)据此拒绝启动。
    pub async fn connect(endpoint: &Endpoint) -> Result<Self, surrealdb::Error> {
        let db: Surreal<Any> = surrealdb::engine::any::connect(&endpoint.address).await?;
        if let Some((username, password)) = &endpoint.auth {
            db.signin(Root {
                username: username.clone(),
                password: password.clone(),
            })
            .await?;
        }
        db.use_ns(&endpoint.ns).use_db(&endpoint.db).await?;
        Ok(Self {
            db,
            endpoint: endpoint.clone(),
        })
    }

    /// 已选定 ns/db 的 SDK 句柄(impl 文件内部使用;不进能力 trait 签名)。
    pub fn db(&self) -> &Surreal<Any> {
        &self.db
    }

    /// 解析出的端点(日志/校验用)。
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// 启动期 schema 工作:版本闸门(降级拒绝)→ 幂等 DDL → 首启写版本。
    ///
    /// 闸门语义与 SQLite 侧 `pool.rs` 的 `SCHEMA_VERSION` 检查一致:
    /// 库内版本 > [`SUPPORTED_SCHEMA_VERSION`] 即拒绝(例如金额口径再变
    /// 后,旧二进制继续跑会读错),只防"比本程序新"的库。版本记录在
    /// `store_meta:schema`(user_version 的替代,阶段 S 验收项)。
    ///
    /// 全新库上 schemafull 模式对未定义表的 SELECT 本身报错,无法先读
    /// 版本——因此首读容错(读错视为"表未建"),DDL 建表后**复核**:
    /// 复核失败才是真错误。已初始化的库(表存在)仍先闸门后 DDL,
    /// 不会对未来版本的库执行任何定义语句。
    pub async fn ensure_schema(&self) -> Result<(), StoreError> {
        // 闸门先行:表已存在且版本更新 → 拒绝,DDL 未动库。
        if let Ok(Some(v)) = self.schema_version().await
            && v > SUPPORTED_SCHEMA_VERSION
        {
            return Err(newer_schema_error(v));
        }

        // DDL 全量幂等执行(重复定义同名字段/索引为 no-op)。
        // 语句级错误经 check() 带出。
        self.db
            .query(SCHEMA_SURQL)
            .await
            .map_err(|e| StoreError::Failed(e.to_string()))?
            .check()
            .map_err(|e| StoreError::Failed(e.to_string()))?;

        // 复核(此刻表必然存在,错误即真错误),再走一次闸门——
        // 覆盖首读因"表未建"被容错吞掉的未来版本库极端情形。
        let stored = self.schema_version().await?;
        if let Some(v) = stored.filter(|v| *v > SUPPORTED_SCHEMA_VERSION) {
            return Err(newer_schema_error(v));
        }
        if stored.is_none() {
            self.db
                .query("CREATE store_meta:schema SET version = $version, applied_at = time::now()")
                .bind(("version", SUPPORTED_SCHEMA_VERSION))
                .await
                .map_err(|e| StoreError::Failed(e.to_string()))?
                .check()
                .map_err(|e| StoreError::Failed(e.to_string()))?;
        }
        Ok(())
    }

    /// 读 `store_meta:schema.version`;记录不存在返回 None;
    /// 表未定义(全新库)等查询错误原样返回(`ensure_schema` 首读容错)。
    pub(crate) async fn schema_version(&self) -> Result<Option<i64>, StoreError> {
        let mut res = self
            .db
            .query("SELECT version FROM store_meta:schema")
            .await
            .map_err(|e| StoreError::Failed(e.to_string()))?;
        let row: Option<serde_json::Value> =
            res.take(0).map_err(|e| StoreError::Failed(e.to_string()))?;
        Ok(row.and_then(|v| v.get("version").and_then(serde_json::Value::as_i64)))
    }
}

// ---------- schema(schemafull;工人只读不改,变更走集成批) ----------
//
// 设计原则(docs/surreal-migration.md §四):
// * 金额一律 `int` 微元(1 元 = 1e6 微元)——计费红线,类型即文档;
// * 时间一律原生 `datetime` / `option<datetime>`(消灭 SQLite 的混合
//   TEXT 时间戳);
// * 布尔一律 `bool`(替代 INTEGER 0/1);
// * 数组/对象用原生 `array<string>` / `object`(替代 JSON TEXT 列);
// * 关联用 `record<表>` 链接(request_logs 软外键语义保留:dangling
//   链接读侧降级为 none,SurrealDB 无强制 FK,记账热路径不再需要
//   EXISTS 预检——见设计文档 §四.request_log);
// * 复合主键表(usage_daily / api_key_usage_daily / channel_affinity /
//   model_health)用数组 record id(`表:[key, sub]`),UPSERT 直接
//   定位记录,等价 SQLite 的 ON CONFLICT DO UPDATE;user/date 等查询
//   维度另存字段并建索引(数组 id 不做字段展开查询);
// * 表名单数小写(user / api_key / …),对齐 SDK 文档惯例。

/// SurrealDB schemafull DDL(幂等)。字段↔SQLite 列的新旧映射表见
/// `docs/surreal-migration.md` §五——迁移工具按映射搬运,不逐表复制。
pub const SCHEMA_SURQL: &str = r#"
    // ---- 元数据(版本闸门) ----
    DEFINE TABLE store_meta SCHEMAFULL;
    DEFINE FIELD version ON TABLE store_meta TYPE int;
    DEFINE FIELD applied_at ON TABLE store_meta TYPE option<datetime>;

    // ---- 用户(余额:整数微元,红线) ----
    DEFINE TABLE user SCHEMAFULL;
    DEFINE FIELD username ON TABLE user TYPE string;
    DEFINE FIELD password_hash ON TABLE user TYPE string;
    DEFINE FIELD display_name ON TABLE user TYPE string DEFAULT '';
    DEFINE FIELD role ON TABLE user TYPE string DEFAULT 'user';
    DEFINE FIELD is_active ON TABLE user TYPE bool DEFAULT true;
    DEFINE FIELD token_version ON TABLE user TYPE int DEFAULT 0;
    DEFINE FIELD balance ON TABLE user TYPE int DEFAULT 0;
    DEFINE FIELD created_at ON TABLE user TYPE datetime DEFAULT time::now();
    DEFINE FIELD updated_at ON TABLE user TYPE datetime DEFAULT time::now();
    DEFINE INDEX unique_user_username ON TABLE user FIELDS username UNIQUE;

    // ---- API key(key = sha256 十六进制;明文仅创建时返回一次) ----
    DEFINE TABLE api_key SCHEMAFULL;
    DEFINE FIELD user ON TABLE api_key TYPE record<user>;
    DEFINE FIELD key ON TABLE api_key TYPE string;
    DEFINE FIELD key_suffix ON TABLE api_key TYPE string DEFAULT '';
    DEFINE FIELD name ON TABLE api_key TYPE string DEFAULT '';
    DEFINE FIELD is_active ON TABLE api_key TYPE bool DEFAULT true;
    DEFINE FIELD last_used_at ON TABLE api_key TYPE option<datetime>;
    DEFINE FIELD expires_at ON TABLE api_key TYPE option<datetime>;
    DEFINE FIELD models ON TABLE api_key TYPE array<string> DEFAULT [];
    DEFINE FIELD rate_limit_rpm ON TABLE api_key TYPE int DEFAULT 0;
    DEFINE FIELD quota_daily_tokens ON TABLE api_key TYPE int DEFAULT 0;
    DEFINE FIELD max_concurrent ON TABLE api_key TYPE int DEFAULT 0;
    DEFINE FIELD created_at ON TABLE api_key TYPE datetime DEFAULT time::now();
    DEFINE INDEX unique_api_key_key ON TABLE api_key FIELDS key UNIQUE;
    DEFINE INDEX idx_api_key_user ON TABLE api_key FIELDS user;

    // ---- 渠道(provider_type 保留:openapi.yaml 冻结契约的必填字段,
    //      openapi.yaml Provider schema required 列表含 provider_type;
    //      api_key 为 enc:v1: 密文,key_hash 为明文 key 的 sha256,
    //      供跨批次重复检测——密文随机 nonce 无法直接比对) ----
    DEFINE TABLE provider SCHEMAFULL;
    DEFINE FIELD name ON TABLE provider TYPE string;
    DEFINE FIELD provider_type ON TABLE provider TYPE string DEFAULT 'openai';
    DEFINE FIELD channel_kind ON TABLE provider TYPE string DEFAULT '';
    DEFINE FIELD openai_base_url ON TABLE provider TYPE string;
    DEFINE FIELD anthropic_base_url ON TABLE provider TYPE string DEFAULT '';
    DEFINE FIELD api_key ON TABLE provider TYPE string DEFAULT '';
    DEFINE FIELD key_hash ON TABLE provider TYPE string DEFAULT '';
    DEFINE FIELD models ON TABLE provider TYPE array<string> DEFAULT [];
    DEFINE FIELD model_mapping ON TABLE provider TYPE object DEFAULT {};
    DEFINE FIELD protocols ON TABLE provider TYPE array<string> DEFAULT [];
    DEFINE FIELD default_protocol ON TABLE provider TYPE string DEFAULT '';
    DEFINE FIELD priority ON TABLE provider TYPE int DEFAULT 0;
    DEFINE FIELD weight ON TABLE provider TYPE float DEFAULT 1.0;
    DEFINE FIELD is_active ON TABLE provider TYPE bool DEFAULT true;
    DEFINE FIELD health_status ON TABLE provider TYPE string DEFAULT 'unknown';
    DEFINE FIELD latency_ms ON TABLE provider TYPE float DEFAULT 0.0;
    DEFINE FIELD error_rate ON TABLE provider TYPE float DEFAULT 0.0;
    DEFINE FIELD last_health_check ON TABLE provider TYPE option<datetime>;
    DEFINE FIELD consecutive_failures ON TABLE provider TYPE int DEFAULT 0;
    DEFINE FIELD disabled_reason ON TABLE provider TYPE string DEFAULT '';
    DEFINE FIELD max_retries ON TABLE provider TYPE int DEFAULT 3;
    DEFINE FIELD timeout_secs ON TABLE provider TYPE int DEFAULT 120;
    DEFINE FIELD proxy_url ON TABLE provider TYPE string DEFAULT '';
    DEFINE FIELD note ON TABLE provider TYPE string DEFAULT '';
    DEFINE FIELD website_url ON TABLE provider TYPE string DEFAULT '';
    DEFINE FIELD created_at ON TABLE provider TYPE datetime DEFAULT time::now();
    DEFINE FIELD updated_at ON TABLE provider TYPE datetime DEFAULT time::now();
    DEFINE INDEX idx_provider_key_hash ON TABLE provider FIELDS key_hash;
    DEFINE INDEX idx_provider_health ON TABLE provider FIELDS health_status;

    // ---- 请求日志(高频追加;关联为 record 链接,软外键语义见文件头) ----
    DEFINE TABLE request_log SCHEMAFULL;
    DEFINE FIELD user ON TABLE request_log TYPE option<record<user>>;
    DEFINE FIELD api_key ON TABLE request_log TYPE option<record<api_key>>;
    DEFINE FIELD provider ON TABLE request_log TYPE option<record<provider>>;
    DEFINE FIELD model ON TABLE request_log TYPE string;
    DEFINE FIELD request_type ON TABLE request_log TYPE string DEFAULT 'chat';
    DEFINE FIELD prompt_tokens ON TABLE request_log TYPE int DEFAULT 0;
    DEFINE FIELD completion_tokens ON TABLE request_log TYPE int DEFAULT 0;
    DEFINE FIELD total_tokens ON TABLE request_log TYPE int DEFAULT 0;
    DEFINE FIELD cached_tokens ON TABLE request_log TYPE int DEFAULT 0;
    DEFINE FIELD latency_ms ON TABLE request_log TYPE int DEFAULT 0;
    DEFINE FIELD status_code ON TABLE request_log TYPE int DEFAULT 0;
    DEFINE FIELD success ON TABLE request_log TYPE bool DEFAULT true;
    DEFINE FIELD error_message ON TABLE request_log TYPE option<string>;
    DEFINE FIELD cost ON TABLE request_log TYPE int DEFAULT 0;
    DEFINE FIELD created_at ON TABLE request_log TYPE datetime DEFAULT time::now();
    DEFINE INDEX idx_request_log_user ON TABLE request_log FIELDS user;
    DEFINE INDEX idx_request_log_provider ON TABLE request_log FIELDS provider;
    DEFINE INDEX idx_request_log_created ON TABLE request_log FIELDS created_at;
    DEFINE INDEX idx_request_log_key_created ON TABLE request_log FIELDS api_key, created_at;

    // ---- 模型价格(每 1M tokens 的价格,元;cached 为空 = 按输入价计) ----
    DEFINE TABLE model_price SCHEMAFULL;
    DEFINE FIELD model ON TABLE model_price TYPE string;
    DEFINE FIELD prompt_price ON TABLE model_price TYPE float DEFAULT 0.0;
    DEFINE FIELD completion_price ON TABLE model_price TYPE float DEFAULT 0.0;
    DEFINE FIELD cached_price ON TABLE model_price TYPE option<float>;
    DEFINE FIELD created_at ON TABLE model_price TYPE datetime DEFAULT time::now();
    DEFINE FIELD updated_at ON TABLE model_price TYPE datetime DEFAULT time::now();
    DEFINE INDEX unique_model_price_model ON TABLE model_price FIELDS model UNIQUE;

    // ---- 按键字计数器(id = [api_key, date];明细是唯一真相,可重建) ----
    DEFINE TABLE api_key_usage_daily SCHEMAFULL;
    DEFINE FIELD api_key ON TABLE api_key_usage_daily TYPE record<api_key>;
    DEFINE FIELD date ON TABLE api_key_usage_daily TYPE string;
    DEFINE FIELD tokens_spent ON TABLE api_key_usage_daily TYPE int DEFAULT 0;
    DEFINE INDEX idx_api_key_usage_daily_date ON TABLE api_key_usage_daily FIELDS date;

    // ---- 按用户日汇总(purge 前聚合,余额对账的永久依据) ----
    DEFINE TABLE usage_daily SCHEMAFULL;
    DEFINE FIELD user ON TABLE usage_daily TYPE record<user>;
    DEFINE FIELD date ON TABLE usage_daily TYPE string;
    DEFINE FIELD requests ON TABLE usage_daily TYPE int DEFAULT 0;
    DEFINE FIELD tokens ON TABLE usage_daily TYPE int DEFAULT 0;
    DEFINE FIELD cost ON TABLE usage_daily TYPE int DEFAULT 0;
    DEFINE INDEX idx_usage_daily_user_date ON TABLE usage_daily FIELDS user, date;
    DEFINE INDEX idx_usage_daily_date ON TABLE usage_daily FIELDS date;

    // ---- 渠道亲和(id = [api_key, model];渠道删除后残留行选路时自然失效) ----
    DEFINE TABLE channel_affinity SCHEMAFULL;
    DEFINE FIELD provider ON TABLE channel_affinity TYPE record<provider>;
    DEFINE FIELD updated_at ON TABLE channel_affinity TYPE datetime DEFAULT time::now();

    // ---- 管理员调账流水(amount/balance_after:整数微元,红线) ----
    DEFINE TABLE billing_transaction SCHEMAFULL;
    DEFINE FIELD user ON TABLE billing_transaction TYPE record<user>;
    DEFINE FIELD amount ON TABLE billing_transaction TYPE int;
    DEFINE FIELD balance_after ON TABLE billing_transaction TYPE int;
    DEFINE FIELD kind ON TABLE billing_transaction TYPE string;
    DEFINE FIELD note ON TABLE billing_transaction TYPE string DEFAULT '';
    DEFINE FIELD created_at ON TABLE billing_transaction TYPE datetime DEFAULT time::now();
    DEFINE INDEX idx_billing_transaction_user ON TABLE billing_transaction FIELDS user, created_at;

    // ---- 兑换码(库存 sha256(code) 与掩码后缀;amount 面值:微元,红线;
    //      核销事务原子性是红线,见 billing_store.rs) ----
    DEFINE TABLE redemption_code SCHEMAFULL;
    DEFINE FIELD code_hash ON TABLE redemption_code TYPE string;
    DEFINE FIELD code_suffix ON TABLE redemption_code TYPE string DEFAULT '';
    DEFINE FIELD amount ON TABLE redemption_code TYPE int;
    DEFINE FIELD batch ON TABLE redemption_code TYPE string DEFAULT '';
    DEFINE FIELD status ON TABLE redemption_code TYPE string DEFAULT 'unused';
    DEFINE FIELD used_by ON TABLE redemption_code TYPE option<record<user>>;
    DEFINE FIELD used_at ON TABLE redemption_code TYPE option<datetime>;
    DEFINE FIELD expires_at ON TABLE redemption_code TYPE option<datetime>;
    DEFINE FIELD note ON TABLE redemption_code TYPE string DEFAULT '';
    DEFINE FIELD created_at ON TABLE redemption_code TYPE datetime DEFAULT time::now();
    DEFINE INDEX unique_redemption_code_hash ON TABLE redemption_code FIELDS code_hash UNIQUE;
    DEFINE INDEX idx_redemption_code_batch ON TABLE redemption_code FIELDS batch;

    // ---- 模型级健康(id = [provider, model]) ----
    DEFINE TABLE model_health SCHEMAFULL;
    DEFINE FIELD provider ON TABLE model_health TYPE record<provider>;
    DEFINE FIELD status ON TABLE model_health TYPE string DEFAULT 'unknown';
    DEFINE FIELD latency_ms ON TABLE model_health TYPE float DEFAULT 0.0;
    DEFINE FIELD error ON TABLE model_health TYPE string DEFAULT '';
    DEFINE FIELD checked_at ON TABLE model_health TYPE option<datetime>;
    DEFINE INDEX idx_model_health_provider ON TABLE model_health FIELDS provider;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_endpoints() {
        // 嵌入式(默认形式)
        let ep = Endpoint::parse("surrealkv://./aikun.surreal").unwrap();
        assert_eq!(ep.kind, EndpointKind::SurrealKv);
        assert_eq!(ep.address, "surrealkv://./aikun.surreal");
        assert_eq!((ep.ns.as_str(), ep.db.as_str()), ("aikun", "aikun"));
        assert!(ep.auth.is_none());

        // 远程 + ns/db/user/pass
        let ep = Endpoint::parse("wss://db.example.com?ns=prod&db=gw&user=root&pass=pw").unwrap();
        assert_eq!(ep.kind, EndpointKind::Wss);
        assert_eq!(ep.address, "wss://db.example.com");
        assert_eq!((ep.ns.as_str(), ep.db.as_str()), ("prod", "gw"));
        assert_eq!(ep.auth, Some(("root".to_string(), "pw".to_string())));

        // 内存库别名
        assert_eq!(Endpoint::parse("memory").unwrap().kind, EndpointKind::Mem);
        assert_eq!(Endpoint::parse("mem://").unwrap().kind, EndpointKind::Mem);
        assert_eq!(
            Endpoint::parse("http://127.0.0.1:8000").unwrap().kind,
            EndpointKind::Http
        );

        // 非法输入
        assert!(Endpoint::parse("").is_err());
        assert!(Endpoint::parse("surrealkv://").is_err(), "空目录路径");
        assert!(
            Endpoint::parse("sqlite://aikun.db").is_err(),
            "旧 scheme 拒绝"
        );
        assert!(
            Endpoint::parse("ws://h?user=root").is_err(),
            "user/pass 必须成对"
        );
        assert!(
            Endpoint::parse("ws://h?shard=1").is_err(),
            "未知 query 参数拒绝"
        );
    }

    /// 骨架级连通性验证:Any 引擎 + mem:// + ensure_schema + 版本闸门。
    /// 证明钉版的 4.0.1 在本 workspace 内可编译可连接(设计文档 §一的
    /// 阻断性验证)。
    ///
    /// 注:SurrealDB 4.0 嵌入式引擎的查询执行在**调用任务栈上内联递归**,
    /// 官方建议 ~10MiB 栈(engine::local 文档);默认 2MiB 测试线程会
    /// 溢出,故用大栈线程跑。生产侧等价要求(tokio 线程栈配置)见设计
    /// 文档 §六"集成批必办"。
    #[test]
    fn mem_connect_schema_and_gate() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("test runtime");
                rt.block_on(async {
                    let ep = Endpoint::parse("mem://").unwrap();
                    let handle = SurrealHandle::connect(&ep).await.expect("connect mem://");
                    handle.ensure_schema().await.expect("ensure schema");

                    // 版本闸门:初始化后读到 SUPPORTED;伪造更高版本后
                    // ensure_schema 拒绝
                    let v = handle.schema_version().await.unwrap();
                    assert_eq!(v, Some(SUPPORTED_SCHEMA_VERSION));

                    handle
                        .db()
                        .query("UPDATE store_meta:schema SET version = $v")
                        .bind(("v", SUPPORTED_SCHEMA_VERSION + 1))
                        .await
                        .unwrap();
                    let err = handle.ensure_schema().await.expect_err("gate must reject");
                    assert!(err.to_string().contains("禁止降级运行"), "got: {err}");
                });
            })
            .expect("spawn big-stack thread")
            .join()
            .expect("join test thread");
    }
}
