//! 渠道拓扑快照缓存(roadmap 阶段 3,决策 4:拓扑走缓存、事实走 DB)。
//!
//! * **快照内容**:全部启用渠道的 [`Provider`] 行(**api_key 保持库内
//!   密文**,选中后由 selector 解一次)+ 预解析的 models / model_mapping
//!   (建快照时一次,选路路径零重复 JSON parse、零逐行解密)。
//! * **失效检测**(每次取用,一条聚合点查):
//!   1. `aikun_admin::admin_providers::topology_epoch`——管理端写路径
//!      成功后递增的进程内世代,覆盖**不改 updated_at 的写**(健康状态
//!      UPDATE)与同秒内连续 PATCH(updated_at 秒级粒度撞车);
//!   2. `aikun_store::CacheTokens::channel_topology_token`——存储层表
//!      令牌(行数/最大 rowid/最大 updated_at 聚合),覆盖绕过管理端
//!      API 的直写(测试夹具、运维脚本)。
//!
//!   两者任一变化 ⇒ 回源全量重建。
//! * **事实仍落库**(roadmap:计费、EMA 指标、亲和):每请求的 EMA 写、
//!   自动禁用照旧走 DB;快照用**同一公式**镜像 EMA 并同步禁用态
//!   (`mirror_request_result` / `mirror_failure` / `deactivate`),
//!   保证选路评分的实时性与重构前一致(逐值对拍见 selector 测试)。
//!   重建时以 DB 为准,镜像漂移自愈。
//!
//! 已知残余缺口(有意接受,记 plan notes):绕过管理端 API、且既不改变
//! 行数/rowid、也不触及 updated_at 的**同秒内第二次**直写 UPDATE 可能
//! 不触发重建(首笔会被 token 捕获;管理端路径无此洞——有世代计数)。

use std::sync::{Arc, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use aikun_channels::Provider;
use tracing::warn;

use aikun_store::CacheToken;

/// 快照内的一条渠道:Provider 原始行(api_key 为库内形态)+ 预解析的
/// 模型清单与映射 key(解析口径与 `provider_supports_model` 一致:
/// 解析失败 ⇒ 空 ⇒ 不支持任何模型)。
pub(crate) struct ChannelEntry {
    pub provider: Provider,
    /// models 列(JSON array)解析结果。
    pub models: Vec<String>,
    /// model_mapping 的**非空** key(空前缀不匹配任何模型,同旧口径)。
    pub mapping_keys: Vec<String>,
}

impl ChannelEntry {
    fn new(provider: Provider) -> Self {
        let models = serde_json::from_str::<Vec<String>>(&provider.models).unwrap_or_default();
        let mapping_keys = serde_json::from_str::<std::collections::HashMap<String, String>>(
            &provider.model_mapping,
        )
        .map(|m| {
            m.keys()
                .filter(|k| !k.is_empty())
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
        Self {
            provider,
            models,
            mapping_keys,
        }
    }

    /// 与旧 `provider_supports_model` 同口径:精确模型名,或任一非空
    /// 映射 key 的前缀命中。
    pub(crate) fn supports_model(&self, model: &str) -> bool {
        self.models.iter().any(|m| m == model)
            || self
                .mapping_keys
                .iter()
                .any(|k| model.starts_with(k.as_str()))
    }
}

/// 一份拓扑快照:条目集可被 EMA 镜像 / 禁用态同步原地修改,快照本体经
/// `Arc` 共享给并发的选路请求。
pub struct TopologySnapshot {
    entries: RwLock<Vec<ChannelEntry>>,
}

impl TopologySnapshot {
    fn from_providers(providers: Vec<Provider>) -> Self {
        Self {
            entries: RwLock::new(providers.into_iter().map(ChannelEntry::new).collect()),
        }
    }

    fn empty() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
        }
    }

    pub(crate) fn entries(&self) -> RwLockReadGuard<'_, Vec<ChannelEntry>> {
        self.entries.read().unwrap_or_else(|e| e.into_inner())
    }

    fn entries_mut(&self) -> RwLockWriteGuard<'_, Vec<ChannelEntry>> {
        self.entries.write().unwrap_or_else(|e| e.into_inner())
    }

    /// 单渠道尝试超时上界(pipe 编排用;与旧
    /// `SELECT MAX(timeout_secs) FROM providers WHERE is_active=1` 同值)。
    pub fn max_timeout_secs(&self) -> i64 {
        self.entries()
            .iter()
            .map(|e| i64::from(e.provider.timeout_secs))
            .max()
            .unwrap_or(0)
    }

    /// 启用且非 unhealthy 渠道可服务的全部模型名(去重排序;含
    /// model_mapping 的请求侧 key)。与旧 `list_available_models` 同口径。
    pub fn available_models(&self) -> Vec<String> {
        let mut models: Vec<String> = Vec::new();
        for e in self.entries().iter() {
            if e.provider.health_status == "unhealthy" {
                continue;
            }
            for m in e.models.iter().chain(e.mapping_keys.iter()) {
                if !models.contains(m) {
                    models.push(m.clone());
                }
            }
        }
        models.sort();
        models
    }

    // ---- 事实镜像(与 selector 的 SQL 同公式,口径见各调用点) ----

    /// 请求结果 EMA 镜像(`record_request_result` 的 UPDATE 同公式):
    /// `latency = latency==0 ? l : latency*0.9 + l*0.1`;
    /// `error_rate = success ? *0.95 : *0.95+0.05`。
    pub(crate) fn mirror_request_result(&self, provider_id: &str, latency_ms: f64, success: bool) {
        self.with_provider_mut(provider_id, |p| {
            if p.latency_ms == 0.0 {
                p.latency_ms = latency_ms;
            } else {
                p.latency_ms = p.latency_ms * 0.9 + latency_ms * 0.1;
            }
            p.error_rate = if success {
                p.error_rate * 0.95
            } else {
                p.error_rate * 0.95 + 0.05
            };
        });
    }

    /// 失败尝试的 EMA 镜像(`record_failure` 的 UPDATE 同公式):
    /// `is_upstream_failure` 推高 error_rate(`*0.95 + 0.05`),否则按
    /// 成功方向衰减(`*0.95`)。
    pub(crate) fn mirror_failure(
        &self,
        provider_id: &str,
        latency_ms: f64,
        is_upstream_failure: bool,
    ) {
        self.with_provider_mut(provider_id, |p| {
            if p.latency_ms == 0.0 {
                p.latency_ms = latency_ms;
            } else {
                p.latency_ms = p.latency_ms * 0.9 + latency_ms * 0.1;
            }
            p.error_rate = if is_upstream_failure {
                p.error_rate * 0.95 + 0.05
            } else {
                p.error_rate * 0.95
            };
        });
    }

    /// 自动禁用 / 管理端禁用:从快照移除(is_active=0 等价;DB 写照旧,
    /// 该写会更新 updated_at,下次令牌比对也会触发重建——双保险)。
    pub(crate) fn deactivate(&self, provider_id: &str) {
        self.entries_mut().retain(|e| e.provider.id != provider_id);
    }

    /// 后台健康检查回写健康态(不进 EMA;与 health.rs 的 UPDATE 同列)。
    pub(crate) fn set_health(&self, provider_id: &str, status: &str) {
        self.with_provider_mut(provider_id, |p| p.health_status = status.to_string());
    }

    /// 在写锁内定位并修改单条渠道(条目 Vec 独占持有,闭包内可变借用)。
    fn with_provider_mut(&self, provider_id: &str, f: impl FnOnce(&mut Provider)) {
        let mut entries = self.entries_mut();
        if let Some(e) = entries.iter_mut().find(|e| e.provider.id == provider_id) {
            f(&mut e.provider);
        }
    }
}

struct RegistryState {
    /// 快照属主(池实例序号):不同库不得共享缓存命中。
    owner: u64,
    epoch: u64,
    token: Option<CacheToken>,
    snap: Arc<TopologySnapshot>,
}

/// 进程内渠道拓扑注册表(单例;装配不经过 main.rs,首次取用时惰性就位)。
pub struct ChannelRegistry {
    state: RwLock<RegistryState>,
}

static REGISTRY: OnceLock<ChannelRegistry> = OnceLock::new();

/// 进程唯一注册表。
pub fn shared() -> &'static ChannelRegistry {
    REGISTRY.get_or_init(|| ChannelRegistry {
        state: RwLock::new(RegistryState {
            owner: 0,
            epoch: 0,
            token: None,
            snap: Arc::new(TopologySnapshot::empty()),
        }),
    })
}

impl ChannelRegistry {
    /// 取当前快照:世代/表令牌任一变化先回源重建。重建失败时返回旧快照
    /// (可能为空——等价于旧实现"DB 读失败 ⇒ 选路 None ⇒ 503")并留痕,
    /// 不缓存失败的令牌(下一请求重试)。
    ///
    /// 阶段 S:令牌点查与全量拉取都是 Surreal 查询(async 化翻转,
    /// docs/surreal-migration.md §六),不再需要 blocking 线程上下文。
    pub async fn current(&self, store: &crate::store::AppStore) -> Arc<TopologySnapshot> {
        let epoch = aikun_admin::admin_providers::topology_epoch();
        let token = match store.channels.topology_token().await {
            Ok(t) => Some(t),
            Err(e) => {
                warn!("channel topology token probe failed: {e}");
                None
            }
        };
        if let Some(snap) = self.matching(store.id, epoch, token.as_ref()) {
            return snap;
        }
        let loaded = store.channels.active_channels().await;
        let mut st = self.lock_write();
        // 双检查:竞态下另一任务可能已完成同一 (store, epoch, token) 的重建。
        if let Some(snap) = state_matches(&st, store.id, epoch, token.as_ref()) {
            return snap;
        }
        match loaded {
            Ok(providers) => {
                st.owner = store.id;
                st.epoch = epoch;
                st.token = token;
                st.snap = Arc::new(TopologySnapshot::from_providers(providers));
            }
            Err(e) => {
                warn!("channel topology snapshot rebuild failed: {e}");
                // 不更新缓存世代/令牌:下一请求重试重建。
            }
        }
        Arc::clone(&st.snap)
    }

    /// 已缓存快照(不做失效检测):EMA 镜像 / 禁用态同步用——对陈旧快照
    /// 的镜像无害(重建以 DB 为准),省掉热路径写侧的额外点查。
    pub(crate) fn cached(&self) -> Arc<TopologySnapshot> {
        Arc::clone(&self.lock_read().snap)
    }

    fn matching(
        &self,
        owner: u64,
        epoch: u64,
        token: Option<&CacheToken>,
    ) -> Option<Arc<TopologySnapshot>> {
        state_matches(&self.lock_read(), owner, epoch, token)
    }

    fn lock_read(&self) -> RwLockReadGuard<'_, RegistryState> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_write(&self) -> RwLockWriteGuard<'_, RegistryState> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }
}

fn state_matches(
    st: &RegistryState,
    owner: u64,
    epoch: u64,
    token: Option<&CacheToken>,
) -> Option<Arc<TopologySnapshot>> {
    // owner(存储实例序号)参与比对:不同库(单测的多实例)即使
    // 表令牌字符串相同也不是同一份快照。
    (st.owner == owner && st.epoch == epoch && st.token.as_ref() == token && token.is_some())
        .then(|| Arc::clone(&st.snap))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 快照条目的模型支持口径:与旧 provider_supports_model 逐值一致。
    #[test]
    fn entry_supports_model_matches_legacy_semantics() {
        let p = |models: &str, mapping: &str| {
            ChannelEntry::new(Provider {
                id: "p".into(),
                name: "p".into(),
                provider_type: "openai".into(),
                openai_base_url: "http://x".into(),
                anthropic_base_url: String::new(),
                api_key: "k".into(),
                key_decrypt_failed: false,
                models: models.into(),
                priority: 0,
                weight: 1.0,
                is_active: true,
                health_status: "healthy".into(),
                latency_ms: 0.0,
                error_rate: 0.0,
                last_health_check: None,
                max_retries: 3,
                timeout_secs: 120,
                created_at: String::new(),
                updated_at: String::new(),
                proxy_url: String::new(),
                model_mapping: mapping.into(),
                consecutive_failures: 0,
                disabled_reason: String::new(),
                protocols: "[\"openai\"]".into(),
                default_protocol: "openai".into(),
                note: String::new(),
                website_url: String::new(),
                channel_kind: String::new(),
            })
        };

        let e = p(r#"["gpt-4","claude-3"]"#, "");
        assert!(e.supports_model("gpt-4"));
        assert!(!e.supports_model("gpt-4o"));
        // 映射 key 前缀命中
        let e = p("[]", r#"{"claude-": "gpt-4"}"#);
        assert!(e.supports_model("claude-3-opus"));
        assert!(!e.supports_model("gpt-4"));
        // 空映射 key 不匹配任何模型
        let e = p("[]", r#"{"": "gpt-4"}"#);
        assert!(!e.supports_model("anything"));
        // 坏 JSON ⇒ 不支持
        let e = p("not json", "also not json");
        assert!(!e.supports_model("gpt-4"));
    }

    /// EMA 镜像公式与 SQL 逐值对拍(公式拷贝自 selector 的 UPDATE 语句)。
    #[test]
    fn mirror_ema_matches_sql_formula() {
        let snap = TopologySnapshot::from_providers(vec![Provider {
            id: "p".into(),
            name: "p".into(),
            provider_type: "openai".into(),
            openai_base_url: "http://x".into(),
            anthropic_base_url: String::new(),
            api_key: "k".into(),
            key_decrypt_failed: false,
            models: "[]".into(),
            priority: 0,
            weight: 1.0,
            is_active: true,
            health_status: "healthy".into(),
            latency_ms: 0.0,
            error_rate: 0.0,
            last_health_check: None,
            max_retries: 3,
            timeout_secs: 120,
            created_at: String::new(),
            updated_at: String::new(),
            proxy_url: String::new(),
            model_mapping: String::new(),
            consecutive_failures: 0,
            disabled_reason: String::new(),
            protocols: "[\"openai\"]".into(),
            default_protocol: "openai".into(),
            note: String::new(),
            website_url: String::new(),
            channel_kind: String::new(),
        }]);

        // latency=0 ⇒ 直接取新值(SQL CASE 首支);成功衰减 error_rate
        snap.mirror_request_result("p", 100.0, true);
        assert_eq!(snap.entries()[0].provider.latency_ms, 100.0);
        assert_eq!(snap.entries()[0].provider.error_rate, 0.0);

        // 第二次:100*0.9 + 200*0.1 = 110
        snap.mirror_request_result("p", 200.0, true);
        assert!((snap.entries()[0].provider.latency_ms - 110.0).abs() < 1e-9);

        // 失败(上游 5xx):error_rate = 0*0.95+0.05
        snap.mirror_failure("p", 0.0, true);
        assert!((snap.entries()[0].provider.error_rate - 0.05).abs() < 1e-9);
        // 客户端 4xx:按成功方向衰减 0.05*0.95 = 0.0475
        snap.mirror_failure("p", 0.0, false);
        assert!((snap.entries()[0].provider.error_rate - 0.0475).abs() < 1e-9);

        // 未知渠道 no-op
        snap.mirror_request_result("other", 1.0, true);
        snap.deactivate("other");
        assert_eq!(snap.entries().len(), 1);

        // 禁用移除;健康态更新在位
        snap.set_health("p", "degraded");
        assert_eq!(snap.entries()[0].provider.health_status, "degraded");
        snap.deactivate("p");
        assert!(snap.entries().is_empty());
    }

    #[test]
    fn max_timeout_and_available_models_from_snapshot() {
        let mk = |id: &str, models: &str, health: &str, timeout: i32| {
            ChannelEntry::new(Provider {
                id: id.into(),
                name: id.into(),
                provider_type: "openai".into(),
                openai_base_url: "http://x".into(),
                anthropic_base_url: String::new(),
                api_key: "k".into(),
                key_decrypt_failed: false,
                models: models.into(),
                priority: 0,
                weight: 1.0,
                is_active: true,
                health_status: health.into(),
                latency_ms: 0.0,
                error_rate: 0.0,
                last_health_check: None,
                max_retries: 3,
                timeout_secs: timeout,
                created_at: String::new(),
                updated_at: String::new(),
                proxy_url: String::new(),
                model_mapping: r#"{"emb-": "up"}"#.into(),
                consecutive_failures: 0,
                disabled_reason: String::new(),
                protocols: "[\"openai\"]".into(),
                default_protocol: "openai".into(),
                note: String::new(),
                website_url: String::new(),
                channel_kind: String::new(),
            })
        };
        let snap = TopologySnapshot {
            entries: RwLock::new(vec![
                mk("a", "[\"gpt-4\"]", "healthy", 60),
                mk("b", "[\"gpt-4\",\"claude-3\"]", "unhealthy", 300),
            ]),
        };
        assert_eq!(snap.max_timeout_secs(), 300);
        // unhealthy 渠道不计入;映射 key 并入
        assert_eq!(snap.available_models(), vec!["emb-", "gpt-4"]);
    }
}
