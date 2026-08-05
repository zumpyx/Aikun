use rusqlite::{Connection, params};
use std::sync::Mutex;

use crate::config::AppConfig;
use crate::crypto::KeyCipher;

pub fn create_connection(config: &AppConfig) -> Result<Connection, rusqlite::Error> {
    // Parse sqlite:// URL, stripping query parameters like ?mode=rwc.
    // A bare file path (no sqlite:// prefix) is accepted as-is.
    let without_scheme = config
        .database_url
        .strip_prefix("sqlite://")
        .unwrap_or(&config.database_url);
    let db_path = without_scheme.split('?').next().unwrap_or("aikun.db");
    let db_path = if db_path.is_empty() { "aikun.db" } else { db_path };
    let conn = Connection::open(db_path)?;
    // synchronous=NORMAL:WAL 模式下提交不强制 fsync,断电最多丢失最后一个检查点
    // 之后的若干事务(不会损坏库),换取写入吞吐一个数量级提升 —— 日志/指标类写入
    // 可接受该取舍;若每次提交都 fsync,单连接写入会被磁盘 fsync 锁死在百级 TPS。
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")?;
    initialize_schema(&conn)?;
    seed_default_admin(&conn)?;
    // 价格表为空时导入内置默认价格(LiteLLM 刊例价快照),便于开箱即计费
    match crate::billing::seed_default_prices(&conn) {
        Ok(n) if n > 0 => tracing::info!("已导入 {} 条内置默认模型价格", n),
        Ok(_) => {}
        Err(e) => tracing::error!("导入内置默认模型价格失败: {}", e),
    }
    Ok(conn)
}

/// 只读连接:WAL 允许并发读者与单写者并行,读路径不再与写互斥。
/// WAL 是文件级设置无需重设,busy_timeout 是连接级必须带上。
fn open_reader(config: &AppConfig) -> Result<Connection, rusqlite::Error> {
    let without_scheme = config
        .database_url
        .strip_prefix("sqlite://")
        .unwrap_or(&config.database_url);
    let db_path = without_scheme.split('?').next().unwrap_or("aikun.db");
    let db_path = if db_path.is_empty() { "aikun.db" } else { db_path };
    let conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")?;
    Ok(conn)
}

/// 只读连接数:读路径(认证、选路、统计)远多于写,4 个轮转足够
/// 覆盖管理后台与 /v1 热路径的并发度。
const READ_POOL_SIZE: usize = 4;

pub struct DbPool {
    /// 写连接:所有 INSERT/UPDATE/DELETE 走这里,串行化写。
    pub conn: Mutex<Connection>,
    /// 只读连接池,经 `read()` 轮转取用。
    readers: Vec<Mutex<Connection>>,
    rr: std::sync::atomic::AtomicUsize,
    /// providers.api_key 静态加密,密钥见 KeyCipher::from_config。
    pub cipher: KeyCipher,
}

impl DbPool {
    pub fn new(config: &AppConfig) -> Result<Self, rusqlite::Error> {
        let cipher = KeyCipher::from_config(config);
        let conn = create_connection(config)?;
        migrate_encrypt_provider_keys(&conn, &cipher)?;
        // :memory: 数据库每个连接是独立的空库,只读连接没有意义——
        // 留空,read() 回退到写连接。
        let is_memory = config.database_url.contains(":memory:");
        let mut readers = Vec::with_capacity(READ_POOL_SIZE);
        if !is_memory {
            for _ in 0..READ_POOL_SIZE {
                match open_reader(config) {
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

/// 把存量明文 providers.api_key 重加密为 enc:v1: 密文;已加密的行跳过,
/// 幂等,每次启动都运行(正常为 0 行,代价一次扫描)。
fn migrate_encrypt_provider_keys(
    conn: &Connection,
    cipher: &KeyCipher,
) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, api_key FROM providers WHERE api_key != '' AND api_key NOT LIKE '{}%'",
        crate::crypto::ENC_PREFIX
    ))?;
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    let n = rows.len();
    for (id, key) in rows {
        conn.execute(
            "UPDATE providers SET api_key = ?1 WHERE id = ?2",
            params![cipher.encrypt(&key), id],
        )?;
    }
    if n > 0 {
        tracing::info!("Migrated {} plaintext provider api_keys to AES-256-GCM ciphertext", n);
    }
    Ok(())
}

fn initialize_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS users (
            id              TEXT PRIMARY KEY,
            username        TEXT NOT NULL UNIQUE,
            password_hash   TEXT NOT NULL,
            display_name    TEXT NOT NULL DEFAULT '',
            role            TEXT NOT NULL DEFAULT 'user'
                CHECK(role IN ('admin', 'user')),
            is_active       INTEGER NOT NULL DEFAULT 1,
            token_version   INTEGER NOT NULL DEFAULT 0,
            -- 余额:整数微元(1 元 = 1e6 微元),口径见 src/billing.rs 模块头。
            balance         INTEGER NOT NULL DEFAULT 0,
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS api_keys (
            id              TEXT PRIMARY KEY,
            user_id         TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            key             TEXT NOT NULL UNIQUE,
            key_suffix      TEXT NOT NULL DEFAULT '',
            name            TEXT NOT NULL DEFAULT '',
            is_active       INTEGER NOT NULL DEFAULT 1,
            last_used_at    TEXT,
            expires_at      TEXT,
            models          TEXT NOT NULL DEFAULT '',
            rate_limit_rpm  INTEGER NOT NULL DEFAULT 0,
            quota_daily_tokens INTEGER NOT NULL DEFAULT 0,
            -- 并发在途请求上限,0 = 不限制(内存计数,见 AppState.api_key_inflight)
            max_concurrent  INTEGER NOT NULL DEFAULT 0,
            created_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS providers (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            provider_type   TEXT NOT NULL DEFAULT 'openai'
                CHECK(provider_type IN ('openai', 'anthropic', 'azure', 'custom')),
            openai_base_url     TEXT NOT NULL,
            anthropic_base_url  TEXT NOT NULL DEFAULT '',
            api_key         TEXT NOT NULL,
            models          TEXT NOT NULL,
            priority        INTEGER NOT NULL DEFAULT 0,
            weight          REAL NOT NULL DEFAULT 1.0,
            is_active       INTEGER NOT NULL DEFAULT 1,
            health_status   TEXT NOT NULL DEFAULT 'unknown'
                CHECK(health_status IN ('healthy', 'degraded', 'unhealthy', 'unknown')),
            latency_ms      REAL NOT NULL DEFAULT 0,
            error_rate      REAL NOT NULL DEFAULT 0,
            last_health_check TEXT,
            max_retries     INTEGER NOT NULL DEFAULT 3,
            timeout_secs    INTEGER NOT NULL DEFAULT 120,
            proxy_url       TEXT NOT NULL DEFAULT '',
            model_mapping   TEXT NOT NULL DEFAULT '',
            consecutive_failures INTEGER NOT NULL DEFAULT 0,
            disabled_reason TEXT NOT NULL DEFAULT '',
            protocols       TEXT NOT NULL DEFAULT '',
            default_protocol TEXT NOT NULL DEFAULT '',
            note            TEXT NOT NULL DEFAULT '',
            website_url     TEXT NOT NULL DEFAULT '',
            created_at      TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS request_logs (
            id              TEXT PRIMARY KEY,
            user_id         TEXT REFERENCES users(id) ON DELETE SET NULL,
            api_key_id      TEXT REFERENCES api_keys(id) ON DELETE SET NULL,
            provider_id     TEXT REFERENCES providers(id) ON DELETE SET NULL,
            model           TEXT NOT NULL,
            request_type    TEXT NOT NULL DEFAULT 'chat',
            prompt_tokens   INTEGER NOT NULL DEFAULT 0,
            completion_tokens INTEGER NOT NULL DEFAULT 0,
            total_tokens    INTEGER NOT NULL DEFAULT 0,
            cached_tokens   INTEGER NOT NULL DEFAULT 0,
            latency_ms      INTEGER NOT NULL DEFAULT 0,
            status_code     INTEGER NOT NULL DEFAULT 0,
            success         INTEGER NOT NULL DEFAULT 1,
            error_message   TEXT,
            -- 单次请求费用:整数微元。
            cost            INTEGER NOT NULL DEFAULT 0,
            created_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS model_health (
            provider_id     TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
            model           TEXT NOT NULL,
            status          TEXT NOT NULL DEFAULT 'unknown'
                CHECK(status IN ('healthy', 'unhealthy', 'unknown')),
            latency_ms      REAL NOT NULL DEFAULT 0,
            error           TEXT NOT NULL DEFAULT '',
            checked_at      TEXT,
            PRIMARY KEY (provider_id, model)
        );

        -- 模型价格:每 1M tokens 的价格(元)。model 支持 * 前缀通配
        -- (如 gpt-*),匹配规则见 src/billing.rs。cached_price 可空:
        -- NULL 时缓存 token 按 prompt_price 计费。
        CREATE TABLE IF NOT EXISTS model_prices (
            id                  TEXT PRIMARY KEY,
            model               TEXT NOT NULL UNIQUE,
            prompt_price        REAL NOT NULL DEFAULT 0,
            completion_price    REAL NOT NULL DEFAULT 0,
            cached_price        REAL,
            created_at          TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at          TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- 管理员手工调账流水(充值/扣减);请求扣费不逐条写这里,
        -- 计费明细以 request_logs.cost 为准。
        CREATE TABLE IF NOT EXISTS billing_transactions (
            id              TEXT PRIMARY KEY,
            user_id         TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            -- 金额与账后余额:整数微元。
            amount          INTEGER NOT NULL,
            balance_after   INTEGER NOT NULL,
            kind            TEXT NOT NULL
                CHECK(kind IN ('recharge', 'adjust')),
            note            TEXT NOT NULL DEFAULT '',
            created_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- 按 (用户, 日) 的消费汇总:purge 删除 request_logs 明细前先聚合到
        -- 这里,保留期过后余额对账(Σ充值 − Σ消费)仍有永久依据。
        CREATE TABLE IF NOT EXISTS usage_daily (            user_id     TEXT NOT NULL,
            date        TEXT NOT NULL,
            requests    INTEGER NOT NULL DEFAULT 0,
            tokens      INTEGER NOT NULL DEFAULT 0,
            -- 日消费合计:整数微元。
            cost        INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (user_id, date)
        );

        -- 兑换码:库中只存 sha256(code) 与掩码后缀,明文仅生成时返回一次。
        -- amount 为面值(整数微元);expires_at 为空 = 永不过期(UTC)。
        CREATE TABLE IF NOT EXISTS redemption_codes (
            id              TEXT PRIMARY KEY,
            code_hash       TEXT NOT NULL UNIQUE,
            code_suffix     TEXT NOT NULL DEFAULT '',
            amount          INTEGER NOT NULL,
            batch           TEXT NOT NULL DEFAULT '',
            status          TEXT NOT NULL DEFAULT 'unused'
                CHECK(status IN ('unused', 'used', 'disabled')),
            used_by         TEXT REFERENCES users(id) ON DELETE SET NULL,
            used_at         TEXT,
            expires_at      TEXT,
            note            TEXT NOT NULL DEFAULT '',
            created_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_request_logs_user_id ON request_logs(user_id);
        CREATE INDEX IF NOT EXISTS idx_request_logs_created_at ON request_logs(created_at);
        CREATE INDEX IF NOT EXISTS idx_request_logs_provider_id ON request_logs(provider_id);
        -- enforce_key_limits 的每请求日额度子查询按 (api_key_id, 当天) 过滤。
        CREATE INDEX IF NOT EXISTS idx_request_logs_api_key_created ON request_logs(api_key_id, created_at);
        CREATE INDEX IF NOT EXISTS idx_providers_health ON providers(health_status);
        CREATE INDEX IF NOT EXISTS idx_redemption_codes_batch ON redemption_codes(batch);
        ",
    )?;

    // Lightweight migrations: add columns introduced after the initial schema.
    run_migrations(conn)?;
    Ok(())
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    ddl: &str,
) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .collect();
    if !columns.iter().any(|c| c == column) {
        // 检查-执行非原子:多实例共享同一 SQLite 文件并发启动时,另一实例
        // 可能抢先加了列。ALTER 失败后复查,列已存在即视为成功。
        if let Err(e) = conn.execute(
            &format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, ddl),
            [],
        ) {
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
            let now_exists = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .filter_map(|r| r.ok())
                .any(|c: String| c == column);
            if !now_exists {
                return Err(e);
            }
        } else {
            tracing::info!("Migrated {} table: added {} column", table, column);
        }
    }
    Ok(())
}

/// providers.base_url → openai_base_url(列重命名,幂等)。
/// 仅当旧列存在且新列不存在时执行。
fn rename_base_url_column(conn: &Connection) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare("PRAGMA table_info(providers)")?;
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .collect();
    if columns.iter().any(|c| c == "base_url") && !columns.iter().any(|c| c == "openai_base_url") {
        conn.execute(
            "ALTER TABLE providers RENAME COLUMN base_url TO openai_base_url",
            [],
        )?;
        tracing::info!("Migrated providers table: renamed base_url to openai_base_url");
    }
    Ok(())
}

fn run_migrations(conn: &Connection) -> Result<(), rusqlite::Error> {
    ensure_column(conn, "providers", "proxy_url", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "providers", "model_mapping", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "providers", "consecutive_failures", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "providers", "disabled_reason", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "providers", "protocols", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "providers", "default_protocol", "TEXT NOT NULL DEFAULT ''")?;
    // 渠道备注:同一供应商多个帐号时用于区分。
    ensure_column(conn, "providers", "note", "TEXT NOT NULL DEFAULT ''")?;
    // 渠道官网地址,仅用于后台展示。
    ensure_column(conn, "providers", "website_url", "TEXT NOT NULL DEFAULT ''")?;
    // API key 限流/额度:每分钟请求数与每日 token 额度,0 表示不限制。
    ensure_column(conn, "api_keys", "rate_limit_rpm", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "api_keys", "quota_daily_tokens", "INTEGER NOT NULL DEFAULT 0")?;
    // API key 并发限制:同时在途请求数上限,0 表示不限制。
    ensure_column(conn, "api_keys", "max_concurrent", "INTEGER NOT NULL DEFAULT 0")?;
    // 计费:用户余额(整数微元,允许为负——扣费不拦截,拦截在请求入口预检)。
    // 存量 REAL 库由 migrate_currency_micro 重建转换,此处只兜底"列缺失"。
    ensure_column(conn, "users", "balance", "INTEGER NOT NULL DEFAULT 0")?;
    // Backfill protocol fields from the legacy provider_type, gated by
    // user_version so it runs exactly once instead of on every startup.
    let user_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version == 0 {
        conn.execute(
            "UPDATE providers SET protocols = '[\"anthropic\"]', default_protocol = 'anthropic'
             WHERE protocols = '' AND provider_type = 'anthropic'",
            [],
        )?;
        conn.execute(
            "UPDATE providers SET protocols = '[\"openai\"]', default_protocol = 'openai'
             WHERE protocols = ''",
            [],
        )?;
        conn.execute_batch("PRAGMA user_version = 1")?;
    }
    // v2: base_url 拆分为按协议的两个地址。旧值回填给 anthropic_base_url,
    // 拆分后行为与之前一致(两种协议仍打同一个上游地址)。
    if user_version < 2 {
        rename_base_url_column(conn)?;
        ensure_column(conn, "providers", "anthropic_base_url", "TEXT NOT NULL DEFAULT ''")?;
        conn.execute(
            "UPDATE providers SET anthropic_base_url = openai_base_url WHERE anthropic_base_url = ''",
            [],
        )?;
        conn.execute_batch("PRAGMA user_version = 2")?;
    }
    ensure_column(conn, "api_keys", "expires_at", "TEXT")?;
    ensure_column(conn, "api_keys", "models", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "api_keys", "key_suffix", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "users", "token_version", "INTEGER NOT NULL DEFAULT 0")?;
    // 默认管理员显示名改名(Administrator 过长,侧栏显示被截断);幂等,
    // 只影响从未改过显示名的默认 admin 行。
    conn.execute(
        "UPDATE users SET display_name = 'Admin' WHERE username = 'admin' AND display_name = 'Administrator'",
        [],
    )?;
    // request_logs 的后加列无条件补齐:FK 已修但缺列的中间态数据库
    // 不会触发下面的表重建,缺列会导致日志插入静默失败。
    ensure_column(conn, "request_logs", "request_type", "TEXT NOT NULL DEFAULT 'chat'")?;
    migrate_api_key_hashes(conn)?;
    // The UNIQUE constraint on api_keys.key already covers this lookup index.
    conn.execute_batch("DROP INDEX IF EXISTS idx_api_keys_key")?;
    // 货币整数化必须先于 request_logs 的 FK 重建:若 FK 重建先把 REAL 费用
    // 值拷进 INTEGER 列,声明类型已变,货币迁移会漏转( REAL 值残留在
    // INTEGER 列里,读取即类型错误)。货币迁移重建的 request_logs 自带
    // ON DELETE SET NULL,随后 FK 检查自然通过、不再重复重建。
    migrate_currency_micro(conn)?;
    migrate_request_logs_fk(conn)?;
    // 表重建会 DROP 旧表(索引随之丢失),重建后统一补建全部 request_logs
    // 索引;新库路径下 IF NOT EXISTS 为 no-op。
    ensure_request_logs_indexes(conn)?;
    // cost 已并入 CREATE TABLE 与重建 DDL;此调用兜底"重建早已完成、
    // cost 尚未加"的中间态旧库。
    ensure_column(conn, "request_logs", "cost", "INTEGER NOT NULL DEFAULT 0")?;
    // 缓存 token 单独计价:request_logs 记缓存用量,model_prices 记缓存价
    // (可空,NULL 按输入价计)。
    ensure_column(conn, "request_logs", "cached_tokens", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "model_prices", "cached_price", "REAL")?;
    Ok(())
}

/// request_logs 全部索引的统一补建:与 initialize_schema 中的索引 DDL
/// 保持一致,任何一条缺失都在这里补齐。
fn ensure_request_logs_indexes(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_request_logs_user_id ON request_logs(user_id);
         CREATE INDEX IF NOT EXISTS idx_request_logs_created_at ON request_logs(created_at);
         CREATE INDEX IF NOT EXISTS idx_request_logs_provider_id ON request_logs(provider_id);
         CREATE INDEX IF NOT EXISTS idx_request_logs_api_key_created ON request_logs(api_key_id, created_at);",
    )
}

/// Hash any remaining plaintext API keys in place (SHA-256 hex) and record
/// the last 4 chars in key_suffix. Unmigrated keys carry the "sk-" prefix,
/// which keeps this migration idempotent.
fn migrate_api_key_hashes(conn: &Connection) -> Result<(), rusqlite::Error> {
    use sha2::{Digest, Sha256};
    let rows: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT id, key FROM api_keys WHERE key LIKE 'sk-%'")?;
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect()
    };
    if rows.is_empty() {
        return Ok(());
    }
    for (id, key) in &rows {
        let digest = format!("{:x}", Sha256::digest(key.as_bytes()));
        let suffix: String = key
            .chars()
            .skip(key.chars().count().saturating_sub(4))
            .collect();
        conn.execute(
            "UPDATE api_keys SET key = ?1, key_suffix = ?2 WHERE id = ?3",
            params![digest, suffix, id],
        )?;
    }
    tracing::info!(
        "Migrated {} plaintext api_keys to SHA-256 digests",
        rows.len()
    );
    Ok(())
}

/// PRAGMA table_info 枚举列名(按表定义顺序)。
fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(cols)
}

/// 货币整数化迁移:users.balance / request_logs.cost /
/// billing_transactions.amount,balance_after / usage_daily.cost 从
/// REAL(元)重建为 INTEGER(微元,×1e6)。按列的声明类型逐列判定
/// (浮点类型才转换),幂等;全部转换在同一事务,失败整体回滚。
/// SQLite 不能就地改列类型,只能重建表;DDL 与 initialize_schema 保持一致。
fn migrate_currency_micro(conn: &Connection) -> Result<(), rusqlite::Error> {
    let declared_type = |table: &str, column: &str| -> Result<Option<String>, rusqlite::Error> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
        let ty = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })?
            .filter_map(|r| r.ok())
            .find(|(name, _)| name == column)
            .map(|(_, ty)| ty);
        Ok(ty)
    };
    // (表, 全部金额列):声明类型仍为浮点类型的列才需要转换
    const MONEY_COLS: &[(&str, &[&str])] = &[
        ("users", &["balance"]),
        ("request_logs", &["cost"]),
        ("billing_transactions", &["amount", "balance_after"]),
        ("usage_daily", &["cost"]),
    ];
    // (表, 待转换金额列):逐列判定声明类型,任何一列仍是浮点类型
    // (REAL/DOUBLE/FLOAT,大小写不敏感)就重建整表,且只转换这些列——
    // 手工修库等造成的部分迁移不会被静默跳过,已转 INTEGER 的列也
    // 不会二次 ×1e6。
    let mut rebuild: Vec<(&str, Vec<&str>)> = Vec::new();
    for (table, cols) in MONEY_COLS {
        let mut float_cols = Vec::new();
        for col in *cols {
            let is_float = declared_type(table, col)?
                .map(|ty| {
                    let ty = ty.to_ascii_uppercase();
                    matches!(ty.as_str(), "REAL" | "DOUBLE" | "DOUBLE PRECISION" | "FLOAT")
                })
                .unwrap_or(false);
            if is_float {
                float_cols.push(*col);
            }
        }
        if !float_cols.is_empty() {
            rebuild.push((table, float_cols));
        }
    }
    if rebuild.is_empty() {
        return Ok(());
    }
    tracing::info!(
        "Migrating currency columns to integer micro-yuan: {}",
        rebuild.iter().map(|(t, _)| *t).collect::<Vec<_>>().join(", ")
    );
    conn.execute_batch("PRAGMA foreign_keys=OFF; BEGIN IMMEDIATE;")?;
    let result = (|| -> Result<(), rusqlite::Error> {
        for (table, money_cols) in &rebuild {
            let mig = format!("{}_curmig", table);
            let ddl = match *table {
                "users" => format!(
                    "CREATE TABLE {} (
                        id              TEXT PRIMARY KEY,
                        username        TEXT NOT NULL UNIQUE,
                        password_hash   TEXT NOT NULL,
                        display_name    TEXT NOT NULL DEFAULT '',
                        role            TEXT NOT NULL DEFAULT 'user'
                            CHECK(role IN ('admin', 'user')),
                        is_active       INTEGER NOT NULL DEFAULT 1,
                        token_version   INTEGER NOT NULL DEFAULT 0,
                        balance         INTEGER NOT NULL DEFAULT 0,
                        created_at      TEXT NOT NULL DEFAULT (datetime('now')),
                        updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
                    );", mig),
                "request_logs" => format!(
                    "CREATE TABLE {} (
                        id              TEXT PRIMARY KEY,
                        user_id         TEXT REFERENCES users(id) ON DELETE SET NULL,
                        api_key_id      TEXT REFERENCES api_keys(id) ON DELETE SET NULL,
                        provider_id     TEXT REFERENCES providers(id) ON DELETE SET NULL,
                        model           TEXT NOT NULL,
                        request_type    TEXT NOT NULL DEFAULT 'chat',
                        prompt_tokens   INTEGER NOT NULL DEFAULT 0,
                        completion_tokens INTEGER NOT NULL DEFAULT 0,
                        total_tokens    INTEGER NOT NULL DEFAULT 0,
                        cached_tokens   INTEGER NOT NULL DEFAULT 0,
                        latency_ms      INTEGER NOT NULL DEFAULT 0,
                        status_code     INTEGER NOT NULL DEFAULT 0,
                        success         INTEGER NOT NULL DEFAULT 1,
                        error_message   TEXT,
                        cost            INTEGER NOT NULL DEFAULT 0,
                        created_at      TEXT NOT NULL DEFAULT (datetime('now'))
                    );", mig),
                "billing_transactions" => format!(
                    "CREATE TABLE {} (
                        id              TEXT PRIMARY KEY,
                        user_id         TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                        amount          INTEGER NOT NULL,
                        balance_after   INTEGER NOT NULL,
                        kind            TEXT NOT NULL
                            CHECK(kind IN ('recharge', 'adjust')),
                        note            TEXT NOT NULL DEFAULT '',
                        created_at      TEXT NOT NULL DEFAULT (datetime('now'))
                    );", mig),
                "usage_daily" => format!(
                    "CREATE TABLE {} (
                        user_id     TEXT NOT NULL,
                        date        TEXT NOT NULL,
                        requests    INTEGER NOT NULL DEFAULT 0,
                        tokens      INTEGER NOT NULL DEFAULT 0,
                        cost        INTEGER NOT NULL DEFAULT 0,
                        PRIMARY KEY (user_id, date)
                    );", mig),
                // 重建队列只来自 MONEY_COLS,出现其他表是编程错误;
                // 绝不能静默套用错误的 DDL(交集复制会丢列)。
                other => unreachable!("unexpected table in currency migration: {}", other),
            };
            conn.execute_batch(&ddl)?;
            // 列清单动态枚举(同 migrate_request_logs_fk):新旧表共有的列
            // 全部复制,金额列在 SELECT 侧 ×1e6 转整数微元。
            let new_cols = table_columns(conn, &mig)?;
            let old_cols = table_columns(conn, table)?;
            let select: Vec<String> = new_cols
                .iter()
                .filter(|c| old_cols.contains(c))
                .map(|c| {
                    if money_cols.contains(&c.as_str()) {
                        format!("CAST(ROUND({} * 1000000) AS INTEGER) AS {}", c, c)
                    } else {
                        c.clone()
                    }
                })
                .collect();
            let cols = new_cols
                .iter()
                .filter(|c| old_cols.contains(c))
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            conn.execute_batch(&format!(
                "INSERT INTO {mig} ({cols}) SELECT {sel} FROM {table};
                 DROP TABLE {table};
                 ALTER TABLE {mig} RENAME TO {table};",
                sel = select.join(", ")
            ))?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT; PRAGMA foreign_keys=ON;")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK; PRAGMA foreign_keys=ON;");
            return Err(e);
        }
    }
    // request_logs 重建带走其索引,由 run_migrations 后续的
    // ensure_request_logs_indexes 统一补建。
    tracing::info!("Currency migration complete");
    Ok(())
}


/// Existing databases created before the FK fix have request_logs foreign keys
/// without ON DELETE SET NULL, which makes deleting a provider/api_key/user
/// that has any logs fail with a constraint error. SQLite cannot alter FK
/// constraints in place, so rebuild the table when the old shape is detected.
fn migrate_request_logs_fk(conn: &Connection) -> Result<(), rusqlite::Error> {
    let needs_rebuild = {
        let mut stmt = conn.prepare("PRAGMA foreign_key_list(request_logs)")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(3)?, row.get::<_, String>(6)?)) // (from, on_delete)
            })?
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>();
        // Table missing (nothing to migrate) → skip; otherwise rebuild unless
        // all three FKs already have ON DELETE SET NULL.
        let all_set_null = ["user_id", "api_key_id", "provider_id"]
            .iter()
            .all(|col| {
                rows.iter()
                    .any(|(from, on_delete)| from == col && on_delete == "SET NULL")
            });
        !rows.is_empty() && !all_set_null
    };
    if !needs_rebuild {
        return Ok(());
    }

    // request_type 等后加列已由 run_migrations 无条件补齐,此处直接重建。
    // 重建表 DDL 与 initialize_schema 的 CREATE TABLE 保持一致。
    tracing::info!("Migrating request_logs: rebuilding with ON DELETE SET NULL foreign keys");
    conn.execute_batch("PRAGMA foreign_keys=OFF; BEGIN IMMEDIATE;")?;
    let rebuild = (|| -> Result<(), rusqlite::Error> {
        conn.execute_batch(
            "CREATE TABLE request_logs_mig (
                id              TEXT PRIMARY KEY,
                user_id         TEXT REFERENCES users(id) ON DELETE SET NULL,
                api_key_id      TEXT REFERENCES api_keys(id) ON DELETE SET NULL,
                provider_id     TEXT REFERENCES providers(id) ON DELETE SET NULL,
                model           TEXT NOT NULL,
                request_type    TEXT NOT NULL DEFAULT 'chat',
                prompt_tokens   INTEGER NOT NULL DEFAULT 0,
                completion_tokens INTEGER NOT NULL DEFAULT 0,
                total_tokens    INTEGER NOT NULL DEFAULT 0,
                cached_tokens   INTEGER NOT NULL DEFAULT 0,
                latency_ms      INTEGER NOT NULL DEFAULT 0,
                status_code     INTEGER NOT NULL DEFAULT 0,
                success         INTEGER NOT NULL DEFAULT 1,
                error_message   TEXT,
                cost            INTEGER NOT NULL DEFAULT 0,
                created_at      TEXT NOT NULL DEFAULT (datetime('now'))
             );",
        )?;
        // 列清单动态枚举:新旧表共有的列全部复制,未来加列忘了同步这里的
        // INSERT 也不会静默丢数据。
        let new_cols = table_columns(conn, "request_logs_mig")?;
        let old_cols = table_columns(conn, "request_logs")?;
        let common: Vec<&str> = new_cols
            .iter()
            .filter(|c| old_cols.contains(c))
            .map(String::as_str)
            .collect();
        let cols = common.join(", ");
        conn.execute_batch(&format!(
            "INSERT INTO request_logs_mig ({cols}) SELECT {cols} FROM request_logs;
             DROP TABLE request_logs;
             ALTER TABLE request_logs_mig RENAME TO request_logs;"
        ))?;
        Ok(())
    })();
    match rebuild {
        Ok(()) => conn.execute_batch("COMMIT; PRAGMA foreign_keys=ON;")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK; PRAGMA foreign_keys=ON;");
            return Err(e);
        }
    }
    // 索引不在此补建:DROP TABLE 会带走旧表索引,由 run_migrations 在本
    // 函数之后调 ensure_request_logs_indexes 统一补齐。
    tracing::info!("request_logs migration complete");
    Ok(())
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
        params![password_hash, id],
    )?;
    Ok(n > 0)
}

fn seed_default_admin(conn: &Connection) -> Result<(), rusqlite::Error> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM users WHERE role = 'admin'",
        [],
        |row| row.get(0),
    )?;

    if count == 0 {
        let id = uuid::Uuid::new_v4().to_string();
        // Generate a random password instead of a well-known default; it is
        // printed exactly once, here, at first startup.
        let password = crate::auth::generate_random_password();
        let password_hash = match crate::auth::hash_password(&password) {
            Ok(h) => h,
            Err(e) => {
                // Never seed an admin with an unusable placeholder hash.
                tracing::error!("Failed to hash default admin password, skipping seed: {}", e);
                return Ok(());
            }
        };
        conn.execute(
            "INSERT INTO users (id, username, password_hash, display_name, role)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, "admin", password_hash, "Admin", "admin"],
        )?;
        tracing::warn!("============================================================");
        tracing::warn!("Default admin created — username: admin  password: {}", password);
        tracing::warn!("This password is shown only once. Log in and change it now.");
        tracing::warn!("============================================================");
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    /// :memory: 库:建表 + 种子 admin 已由 DbPool::new 完成。
    fn memory_pool() -> DbPool {
        let config = AppConfig {
            database_url: "sqlite://:memory:".to_string(),
            ..Default::default()
        };
        DbPool::new(&config).expect("in-memory db")
    }

    #[test]
    fn currency_migration_converts_real_yuan_to_integer_micro() {
        // 旧库形态:四张表金额列均为 REAL(元)
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users (
                id TEXT PRIMARY KEY, username TEXT NOT NULL UNIQUE,
                password_hash TEXT NOT NULL, balance REAL NOT NULL DEFAULT 0);
             CREATE TABLE request_logs (
                id TEXT PRIMARY KEY, user_id TEXT, model TEXT NOT NULL DEFAULT '',
                cost REAL NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')));
             CREATE TABLE billing_transactions (
                id TEXT PRIMARY KEY, user_id TEXT NOT NULL,
                amount REAL NOT NULL, balance_after REAL NOT NULL,
                kind TEXT NOT NULL, note TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL DEFAULT (datetime('now')));
             CREATE TABLE usage_daily (
                user_id TEXT NOT NULL, date TEXT NOT NULL,
                requests INTEGER NOT NULL DEFAULT 0, tokens INTEGER NOT NULL DEFAULT 0,
                cost REAL NOT NULL DEFAULT 0, PRIMARY KEY (user_id, date));
             INSERT INTO users (id, username, password_hash, balance) VALUES
                ('u1', 'a', 'h', 100.5), ('u2', 'b', 'h', -0.25);
             INSERT INTO request_logs (id, user_id, cost) VALUES
                ('l1', 'u1', 0.00014), ('l2', 'u1', 0.0);
             INSERT INTO billing_transactions (id, user_id, amount, balance_after, kind)
                VALUES ('t1', 'u1', 100.0, 100.5, 'recharge');
             INSERT INTO usage_daily VALUES ('u1', '2024-01-01', 3, 300, 0.03);",
        )
        .unwrap();

        migrate_currency_micro(&conn).unwrap();

        let q = |sql: &str| -> i64 {
            conn.query_row(sql, [], |r| r.get(0)).unwrap()
        };
        assert_eq!(q("SELECT balance FROM users WHERE id = 'u1'"), 100_500_000);
        assert_eq!(q("SELECT balance FROM users WHERE id = 'u2'"), -250_000);
        assert_eq!(q("SELECT cost FROM request_logs WHERE id = 'l1'"), 140);
        assert_eq!(q("SELECT cost FROM request_logs WHERE id = 'l2'"), 0);
        assert_eq!(q("SELECT amount FROM billing_transactions WHERE id = 't1'"), 100_000_000);
        assert_eq!(q("SELECT balance_after FROM billing_transactions WHERE id = 't1'"), 100_500_000);
        assert_eq!(q("SELECT cost FROM usage_daily WHERE user_id = 'u1'"), 30_000);

        // 声明类型已变为 INTEGER(否则读 i64 会命中残留的 REAL 值)
        let ty = |table: &str| -> String {
            conn.query_row(
                &format!("SELECT type FROM pragma_table_info('{}') WHERE name IN ('balance','cost','amount') LIMIT 1", table),
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        for t in ["users", "request_logs", "billing_transactions", "usage_daily"] {
            assert_eq!(ty(t), "INTEGER", "{} not rebuilt", t);
        }

        // 幂等:再跑一次值不变
        migrate_currency_micro(&conn).unwrap();
        assert_eq!(q("SELECT balance FROM users WHERE id = 'u1'"), 100_500_000);
        assert_eq!(q("SELECT cost FROM request_logs WHERE id = 'l1'"), 140);
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
        conn.execute("UPDATE users SET username = 'root' WHERE username = 'admin'", [])
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
        conn.execute("DELETE FROM users WHERE role = 'admin'", []).unwrap();

        // 没有任何 admin 用户:返回 false,普通用户不受影响。
        assert!(!reset_admin_password(&conn, "x").unwrap());
        let hash: String = conn
            .query_row("SELECT password_hash FROM users WHERE id = 'u1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(hash, "h");
    }
}
