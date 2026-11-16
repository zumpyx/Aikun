//! SQLite 连接池(迁自二进制 `aikun/src/db/mod.rs`,plan 步骤 8):
//! 单写连接 + 轮转只读连接组,PRAGMA 只出现在这里与 `schema.rs`
//! (SQLite impl 内部细节,不进能力 trait 签名)。

use std::sync::Mutex;

use rusqlite::Connection;

use crate::crypto::KeyCipher;
use crate::schema::{
    SCHEMA_VERSION, initialize_schema, migrate_encrypt_provider_keys, migrate_provider_key_hashes,
    rebuild_usage_counters, seed_default_admin,
};

/// 只读连接数:读路径(认证、选路、统计)远多于写,4 个轮转足够
/// 覆盖管理后台与 /v1 热路径的并发度。
const READ_POOL_SIZE: usize = 4;

/// 池实例序号(进程内单调递增):缓存一致性比对的一维——不同池(如
/// 单测里的多个 :memory: 库)即便表令牌字符串相同也必须判为不匹配,
/// 否则会把 A 池的快照错当 B 池的缓存命中。
static POOL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub struct DbPool {
    /// 写连接:所有 INSERT/UPDATE/DELETE 走这里,串行化写。
    pub conn: Mutex<Connection>,
    /// 只读连接池,经 `read()` 轮转取用。
    readers: Vec<Mutex<Connection>>,
    rr: std::sync::atomic::AtomicUsize,
    /// providers.api_key 静态加密,密钥见 KeyCipher::from_secret。
    pub cipher: KeyCipher,
    /// 实例序号(见 [`POOL_SEQ`]);进程内唯一,缓存比对用。
    pub id: u64,
}

impl DbPool {
    /// 打开(或创建)数据库并完成全部启动期工作:版本闸门、PRAGMA、
    /// schema 初始化与迁移、渠道密钥加密/哈希回填、默认管理员与默认
    /// 价格播种、当日日用量计数器重建。
    ///
    /// 迁移顺序与吸收前一致:加密迁移必须先于 key_hash 回填(回填要先
    /// 解密 enc:v1: 密文再算哈希)。
    pub fn open(database_url: &str, cipher: KeyCipher) -> Result<Self, rusqlite::Error> {
        let conn = create_connection(database_url)?;
        migrate_encrypt_provider_keys(&conn, &cipher)?;
        // 必须在加密迁移之后:回填要解密 enc:v1: 密文再算哈希。
        migrate_provider_key_hashes(&conn, &cipher)?;
        // 日用量计数器从明细重建:升级(当天已有记账)与重启(计数器
        // 是加速器,明细是唯一真相)都归零对齐,refactor-plan 9.3。
        if let Err(e) = rebuild_usage_counters(&conn, chrono::Utc::now().date_naive()) {
            tracing::warn!("Failed to rebuild daily usage counters: {}", e);
        }
        // :memory: 数据库每个连接是独立的空库,只读连接没有意义——
        // 留空,read() 回退到写连接。
        let is_memory = database_url.contains(":memory:");
        let mut readers = Vec::with_capacity(READ_POOL_SIZE);
        if !is_memory {
            for _ in 0..READ_POOL_SIZE {
                match open_reader(database_url) {
                    Ok(conn) => readers.push(Mutex::new(conn)),
                    // 单个 reader 失败不应拖垮整个进程:跳过即可。
                    Err(e) => {
                        tracing::warn!("Failed to open read-only DB connection, skipping: {}", e);
                    }
                }
            }
            // 全部失败时 readers 为空,read() 回退写连接
            // (读写退化为互斥,但服务可用)。
        }
        Ok(Self {
            conn: Mutex::new(conn),
            readers,
            rr: std::sync::atomic::AtomicUsize::new(0),
            cipher,
            id: POOL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        })
    }

    /// 取一个只读连接(轮转)。只用于纯 SELECT;任何写必须走 `conn`。
    /// :memory: 配置下回退写连接(读连接会是独立的空库)。
    pub fn read(&self) -> &Mutex<Connection> {
        if self.readers.is_empty() {
            return &self.conn;
        }
        let i = self.rr.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        &self.readers[i % self.readers.len()]
    }
}

pub fn create_connection(database_url: &str) -> Result<Connection, rusqlite::Error> {
    // Parse sqlite:// URL, stripping query parameters like ?mode=rwc.
    // A bare file path (no sqlite:// prefix) is accepted as-is.
    let without_scheme = database_url
        .strip_prefix("sqlite://")
        .unwrap_or(database_url);
    let db_path = without_scheme.split('?').next().unwrap_or("aikun.db");
    let db_path = if db_path.is_empty() {
        "aikun.db"
    } else {
        db_path
    };
    let conn = Connection::open(db_path)?;
    // 版本闸门:库的 schema 版本比本程序新(降级运行)时拒绝启动。
    let db_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if db_version > SCHEMA_VERSION {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISMATCH),
            Some(format!(
                "数据库 schema 版本 v{} 比本程序支持的 v{} 新,请升级 aikun(禁止降级运行)",
                db_version, SCHEMA_VERSION
            )),
        ));
    }
    // synchronous=NORMAL:WAL 模式下提交不强制 fsync,断电最多丢失最后一个检查点
    // 之后的若干事务(不会损坏库),换取写入吞吐一个数量级提升 —— 日志/指标类写入
    // 可接受该取舍;若每次提交都 fsync,单连接写入会被磁盘 fsync 锁死在百级 TPS。
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")?;
    initialize_schema(&conn)?;
    seed_default_admin(&conn)?;
    // 价格表为空时导入内置默认价格(LiteLLM 刊例价快照),便于开箱即计费。
    // 播种逻辑与本调用同在仓储层(db 层不再反向调用 billing,plan 步骤 8)。
    match crate::pricing::seed_default_prices(&conn) {
        Ok(n) if n > 0 => tracing::info!("已导入 {} 条内置默认模型价格", n),
        Ok(_) => {}
        Err(e) => tracing::error!("导入内置默认模型价格失败: {}", e),
    }
    Ok(conn)
}

/// 只读连接:WAL 允许并发读者与单写者并行,读路径不再与写互斥。
/// WAL 是文件级设置无需重设,busy_timeout 是连接级必须带上。
fn open_reader(database_url: &str) -> Result<Connection, rusqlite::Error> {
    let without_scheme = database_url
        .strip_prefix("sqlite://")
        .unwrap_or(database_url);
    let db_path = without_scheme.split('?').next().unwrap_or("aikun.db");
    let db_path = if db_path.is_empty() {
        "aikun.db"
    } else {
        db_path
    };
    let conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")?;
    Ok(conn)
}

/// 重置 admin 账户的密码哈希,并 token_version + 1(旧 JWT 会话全部失效)。
/// 目标用户:优先用户名 admin;不存在时取最早创建的 role='admin' 用户。
/// 返回 false 表示库中没有任何 admin 用户(不报错,视为无需处理)。
/// 供启动期的 AIKUN_RESET_ADMIN_PASSWORD 运维逃生口调用。
pub fn reset_admin_password(
    conn: &Connection,
    password_hash: &str,
) -> Result<bool, rusqlite::Error> {
    use rusqlite::OptionalExtension;
    let target: Option<String> = match conn
        .query_row("SELECT id FROM users WHERE username = 'admin'", [], |row| {
            row.get(0)
        })
        .optional()?
    {
        Some(id) => Some(id),
        None => conn
            .query_row(
                "SELECT id FROM users WHERE role = 'admin' ORDER BY created_at LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?,
    };
    let Some(id) = target else {
        return Ok(false);
    };
    let n = conn.execute(
        "UPDATE users SET password_hash = ?1, token_version = token_version + 1,
                updated_at = datetime('now')
         WHERE id = ?2",
        rusqlite::params![password_hash, id],
    )?;
    Ok(n > 0)
}

// ---------- CacheTokens(SQLite 实现) ----------
//
// 指纹口径:(行数, 最大 rowid, 最大 updated_at, unhealthy 计数)。捕获
// INSERT/DELETE、任何触及 updated_at 的 UPDATE(管理端 CRUD 与自动禁用
// 都会写 updated_at),以及不改 updated_at 的健康态跃迁(unhealthy 计数
// ——选路资格过滤的输入);EMA 类热路径写(latency_ms/error_rate)不进
// 指纹,由进程内快照同步维护,避免每请求触发全量重载。残余盲区(有意
// 接受):绕过管理端 API 且不动上述列的直写(如 weight/models 原地改
// 值)——生产写路径全部落在指纹或世代计数器内,详见 channel/registry.rs。

impl crate::CacheTokens for DbPool {
    fn channel_topology_token(&self) -> Result<crate::CacheToken, crate::StoreError> {
        let conn = self
            .read()
            .lock()
            .map_err(|_| crate::StoreError::Unavailable)?;
        let stamp: String = conn
            .query_row(
                "SELECT format('%d|%d|%s|%d', COUNT(*), COALESCE(MAX(rowid), 0),
                        COALESCE(MAX(updated_at), ''),
                        COALESCE(SUM(health_status = 'unhealthy'), 0)) FROM providers",
                [],
                |row| row.get(0),
            )
            .map_err(|e| crate::StoreError::Failed(e.to_string()))?;
        Ok(crate::CacheToken::new(stamp))
    }

    fn price_table_token(&self) -> Result<crate::CacheToken, crate::StoreError> {
        let conn = self
            .read()
            .lock()
            .map_err(|_| crate::StoreError::Unavailable)?;
        let stamp: String = conn
            .query_row(
                "SELECT format('%d|%d|%s', COUNT(*), COALESCE(MAX(rowid), 0),
                        COALESCE(MAX(updated_at), '')) FROM model_prices",
                [],
                |row| row.get(0),
            )
            .map_err(|e| crate::StoreError::Failed(e.to_string()))?;
        Ok(crate::CacheToken::new(stamp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// :memory: 库:建表 + 种子 admin 已由 DbPool::open 完成。
    fn memory_pool() -> DbPool {
        DbPool::open(":memory:", KeyCipher::from_secret("test")).expect("in-memory db")
    }

    /// 版本闸门:初始化后 user_version 等于当前 schema 版本;来自更高版本
    /// 的数据库(降级运行)应被拒绝打开。
    #[test]
    fn user_version_gate_rejects_newer_database() {
        let dir = std::env::temp_dir().join(format!("aikun-store-vg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}", dir.join("test.db").display());

        let conn = create_connection(&url).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        // 历史遗留标记(v0.1.0/v0.2.0 写过 1/2)不得触发闸门
        conn.execute_batch("PRAGMA user_version = 2").unwrap();
        drop(conn);
        let conn = create_connection(&url).unwrap();
        // 模拟来自未来版本的库
        conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
            .unwrap();
        drop(conn);

        assert!(create_connection(&url).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 令牌:行内容/行数变化 ⇒ 令牌变化;同内容稳定。
    #[test]
    fn cache_token_tracks_table_changes() {
        let pool = memory_pool();
        let t0 = crate::CacheTokens::channel_topology_token(&pool).unwrap();
        assert_eq!(
            t0,
            crate::CacheTokens::channel_topology_token(&pool).unwrap()
        );
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO providers (id, name, provider_type, openai_base_url, api_key, models)
                 VALUES ('p1', 't', 'openai', 'http://x', 'k', '[]')",
                [],
            )
            .unwrap();
        let t1 = crate::CacheTokens::channel_topology_token(&pool).unwrap();
        assert_ne!(t0, t1);
        // 触及 updated_at 的 UPDATE 也变化(管理端 PATCH 语义;显式给不同
        // 的时间戳——datetime('now') 与 INSERT 默认值可能落在同一秒)
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE providers SET weight = 2, updated_at = '2030-01-01 00:00:00' WHERE id = 'p1'",
                [],
            )
            .unwrap();
        let t2 = crate::CacheTokens::channel_topology_token(&pool).unwrap();
        assert_ne!(t1, t2);
        // unhealthy 跃迁(不动 updated_at)也变化:选路资格过滤的输入
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE providers SET health_status = 'unhealthy' WHERE id = 'p1'",
                [],
            )
            .unwrap();
        assert_ne!(
            t2,
            crate::CacheTokens::channel_topology_token(&pool).unwrap()
        );
        // 价格表令牌独立变化
        let p0 = crate::CacheTokens::price_table_token(&pool).unwrap();
        pool.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO model_prices (id, model, prompt_price, completion_price)
                 VALUES ('mp1', 'm', 1, 2)",
                [],
            )
            .unwrap();
        assert_ne!(p0, crate::CacheTokens::price_table_token(&pool).unwrap());
    }

    #[test]
    fn reset_admin_password_changes_hash_and_bumps_token_version() {
        let pool = memory_pool();
        let conn = pool.conn.lock().unwrap();
        let (old_hash, old_version): (String, i64) = conn
            .query_row(
                "SELECT password_hash, token_version FROM users WHERE username = 'admin'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert!(reset_admin_password(&conn, "new-hash-1").unwrap());
        let (hash, version): (String, i64) = conn
            .query_row(
                "SELECT password_hash, token_version FROM users WHERE username = 'admin'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(hash, "new-hash-1");
        assert_ne!(hash, old_hash);
        assert_eq!(version, old_version + 1);

        // 再次重置:token_version 继续递增(每重置一次旧会话就失效一次)。
        assert!(reset_admin_password(&conn, "new-hash-2").unwrap());
        let version: i64 = conn
            .query_row(
                "SELECT token_version FROM users WHERE username = 'admin'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, old_version + 2);
    }

    #[test]
    fn reset_admin_password_falls_back_to_first_admin_role_user() {
        let pool = memory_pool();
        let conn = pool.conn.lock().unwrap();
        // 用户名不是 admin 的 admin 角色用户也应被命中。
        conn.execute(
            "UPDATE users SET username = 'root' WHERE username = 'admin'",
            [],
        )
        .unwrap();

        assert!(reset_admin_password(&conn, "root-hash").unwrap());
        let hash: String = conn
            .query_row(
                "SELECT password_hash FROM users WHERE username = 'root'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hash, "root-hash");
    }

    #[test]
    fn reset_admin_password_without_admin_user_is_noop() {
        let pool = memory_pool();
        let conn = pool.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO users (id, username, password_hash, role) VALUES ('u1', 'bob', 'h', 'user')",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM users WHERE role = 'admin'", [])
            .unwrap();

        // 没有任何 admin 用户:返回 false,普通用户不受影响。
        assert!(!reset_admin_password(&conn, "x").unwrap());
        let hash: String = conn
            .query_row(
                "SELECT password_hash FROM users WHERE id = 'u1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hash, "h");
    }
}
