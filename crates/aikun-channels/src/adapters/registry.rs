//! 渠道类型注册表:channel_kind → 适配器实例。
//!
//! 内置适配器在 [`ChannelRegistry::builtin`] 里集中注册——这是新增渠道类型
//! 时唯一需要改动的注册点。

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use serde::Serialize;

use crate::ChannelAdapter;
use crate::provider::Provider;

pub struct ChannelRegistry {
    adapters: BTreeMap<&'static str, Arc<dyn ChannelAdapter>>,
}

/// 管理端下拉列表用的适配器描述。
#[derive(Debug, Serialize)]
pub struct ChannelKindInfo {
    pub kind: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub experimental: bool,
}

impl ChannelRegistry {
    pub fn empty() -> Self {
        Self {
            adapters: BTreeMap::new(),
        }
    }

    /// 注册一个适配器;同名 kind 后注册者覆盖先注册者。
    pub fn register(&mut self, adapter: Arc<dyn ChannelAdapter>) {
        self.adapters.insert(adapter.kind(), adapter);
    }

    /// 全部内置适配器。**新增渠道类型时在这里加一行。**
    pub fn builtin() -> Self {
        let mut reg = Self::empty();
        reg.register(Arc::new(super::generic::GenericAdapter));
        reg.register(Arc::new(super::workbuddy::WorkBuddyAdapter));
        reg.register(Arc::new(super::qoder::QoderAdapter));
        reg.register(Arc::new(super::zcode::ZcodeAdapter));
        reg.register(Arc::new(super::zai_plan::ZaiPlanAdapter));
        reg.register(Arc::new(super::bigmodel_plan::BigModelPlanAdapter));
        reg
    }

    pub fn get(&self, kind: &str) -> Option<Arc<dyn ChannelAdapter>> {
        self.adapters.get(kind).cloned()
    }

    /// channel_kind 是否合法:空串(= generic)或已注册的类型。
    pub fn is_valid_kind(&self, kind: &str) -> bool {
        kind.is_empty() || self.adapters.contains_key(kind)
    }

    /// 按 kind 取适配器;空串或未注册(例如库是更新版本的二进制写的)
    /// 一律回退 generic,保证存量渠道照常可用。
    pub fn resolve(&self, kind: &str) -> Arc<dyn ChannelAdapter> {
        if let Some(a) = self.get(kind) {
            return a;
        }
        if !kind.is_empty() {
            tracing::warn!("未注册的渠道类型 '{}',按 generic 处理", kind);
        }
        self.get(super::generic::KIND)
            .expect("generic adapter must always be registered")
    }

    pub fn list(&self) -> Vec<ChannelKindInfo> {
        // generic 排第一,其余按 kind 字母序。
        let mut out: Vec<ChannelKindInfo> = self
            .adapters
            .values()
            .map(|a| ChannelKindInfo {
                kind: a.kind(),
                label: a.label(),
                description: a.description(),
                experimental: a.experimental(),
            })
            .collect();
        out.sort_by_key(|i| (i.kind != super::generic::KIND, i.kind));
        out
    }
}

static REGISTRY: OnceLock<ChannelRegistry> = OnceLock::new();

/// 进程级注册表(首次访问时构建内置适配器)。
pub fn registry() -> &'static ChannelRegistry {
    REGISTRY.get_or_init(ChannelRegistry::builtin)
}

/// 取渠道对应的适配器。
pub fn adapter_for(provider: &Provider) -> Arc<dyn ChannelAdapter> {
    registry().resolve(provider.channel_kind.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_has_generic_first_and_placeholders() {
        let reg = ChannelRegistry::builtin();
        let kinds: Vec<_> = reg.list().into_iter().map(|i| i.kind).collect();
        assert_eq!(kinds[0], "generic");
        assert!(kinds.contains(&"workbuddy"));
        assert!(kinds.contains(&"qoder"));
        assert!(kinds.contains(&"zcode"));
    }

    #[test]
    fn empty_and_unknown_kind_fall_back_to_generic() {
        let reg = ChannelRegistry::builtin();
        assert_eq!(reg.resolve("").kind(), "generic");
        assert_eq!(reg.resolve("from-the-future").kind(), "generic");
        assert_eq!(reg.resolve("qoder").kind(), "qoder");
    }

    #[test]
    fn kind_validation() {
        let reg = ChannelRegistry::builtin();
        assert!(reg.is_valid_kind(""));
        assert!(reg.is_valid_kind("generic"));
        assert!(reg.is_valid_kind("workbuddy"));
        assert!(reg.is_valid_kind("zcode"));
        assert!(!reg.is_valid_kind("nope"));
    }

    /// 自定义适配器可以覆写 URL、鉴权与请求体,其余走默认实现。
    struct DemoAdapter;
    impl ChannelAdapter for DemoAdapter {
        fn kind(&self) -> &'static str {
            "demo"
        }
        fn label(&self) -> &'static str {
            "Demo"
        }
        fn endpoint_path(&self, _protocol: aikun_protocol::Protocol) -> &'static str {
            "/api/chat"
        }
        fn prepare_request(
            &self,
            _p: &Provider,
            _protocol: aikun_protocol::Protocol,
            body: &mut serde_json::Value,
        ) {
            body["vendor_flag"] = serde_json::json!(true);
        }
    }

    #[test]
    fn custom_adapter_overrides_hooks() {
        let mut reg = ChannelRegistry::builtin();
        reg.register(Arc::new(DemoAdapter));
        let a = reg.resolve("demo");
        let mut p = crate::provider::tests::bare_provider();
        p.openai_base_url = "https://vendor.example/v1".into();
        assert_eq!(
            a.chat_url(&p, aikun_protocol::Protocol::OpenAi),
            "https://vendor.example/v1/api/chat"
        );
        let mut body = serde_json::json!({"model": "m"});
        a.prepare_request(&p, aikun_protocol::Protocol::OpenAi, &mut body);
        assert_eq!(body["vendor_flag"], true);
        // 未覆写的钩子沿用默认实现
        assert_eq!(
            a.models_url("https://vendor.example", aikun_protocol::Protocol::OpenAi),
            "https://vendor.example/v1/models"
        );
    }
}
