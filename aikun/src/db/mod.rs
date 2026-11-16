//! 仓储层的二进制薄壳(plan 步骤 8):池/schema/迁移/种子已吸收进
//! `crates/aikun-store`;本文件只保留 `crate::db::*` 旧路径与
//! `DbPool::new(&AppConfig)` 兼容签名(装配点在 main.rs,不改调用方)。
//!
//! 能力 trait(`aikun_store::{ChannelTopology, PriceBook, KeyUsage,
//! CacheTokens}`)直接实现在 aikun-store 的 SQLite 池上,本 crate 经
//! Deref 透传使用。

use std::ops::Deref;

use aikun_store::pool::DbPool as StorePool;

use crate::config::AppConfig;

/// 进程唯一存储句柄:包装 [`aikun_store`] 的 SQLite 池,保持
/// `DbPool::new(&AppConfig)` 与 `pool.conn` / `pool.read()` /
/// `pool.cipher` 字段访问兼容(字段访问经 Deref 透传)。
pub struct DbPool(StorePool);

impl DbPool {
    pub fn new(config: &AppConfig) -> Result<Self, rusqlite::Error> {
        let cipher = crate::crypto::cipher_from_config(config);
        Ok(Self(StorePool::open(&config.database_url, cipher)?))
    }
}

impl Deref for DbPool {
    type Target = StorePool;

    fn deref(&self) -> &StorePool {
        &self.0
    }
}

// 运维逃生口(main.rs 的 AIKUN_RESET_ADMIN_PASSWORD)。
pub use aikun_store::pool::reset_admin_password;
