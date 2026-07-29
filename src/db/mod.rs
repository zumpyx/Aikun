use rusqlite::{Connection, params};
use std::sync::Mutex;

use crate::config::AppConfig;

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
    Ok(conn)
}

pub struct DbPool {
    pub conn: Mutex<Connection>,
}

impl DbPool {
    pub fn new(config: &AppConfig) -> Result<Self, rusqlite::Error> {
        let conn = create_connection(config)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
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
            created_at      TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS providers (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            provider_type   TEXT NOT NULL DEFAULT 'openai'
                CHECK(provider_type IN ('openai', 'anthropic', 'azure', 'custom')),
            base_url        TEXT NOT NULL,
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
            latency_ms      INTEGER NOT NULL DEFAULT 0,
            status_code     INTEGER NOT NULL DEFAULT 0,
            success         INTEGER NOT NULL DEFAULT 1,
            error_message   TEXT,
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

        CREATE INDEX IF NOT EXISTS idx_request_logs_user_id ON request_logs(user_id);
        CREATE INDEX IF NOT EXISTS idx_request_logs_created_at ON request_logs(created_at);
        CREATE INDEX IF NOT EXISTS idx_request_logs_provider_id ON request_logs(provider_id);
        CREATE INDEX IF NOT EXISTS idx_providers_health ON providers(health_status);
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
        conn.execute(
            &format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, ddl),
            [],
        )?;
        tracing::info!("Migrated {} table: added {} column", table, column);
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
    ensure_column(conn, "api_keys", "expires_at", "TEXT")?;
    ensure_column(conn, "api_keys", "models", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "api_keys", "key_suffix", "TEXT NOT NULL DEFAULT ''")?;
    ensure_column(conn, "users", "token_version", "INTEGER NOT NULL DEFAULT 0")?;
    // request_logs 的后加列无条件补齐:FK 已修但缺列的中间态数据库
    // 不会触发下面的表重建,缺列会导致日志插入静默失败。
    ensure_column(conn, "request_logs", "request_type", "TEXT NOT NULL DEFAULT 'chat'")?;
    migrate_api_key_hashes(conn)?;
    // The UNIQUE constraint on api_keys.key already covers this lookup index.
    conn.execute_batch("DROP INDEX IF EXISTS idx_api_keys_key")?;
    migrate_request_logs_fk(conn)?;
    Ok(())
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
    tracing::info!("Migrating request_logs: rebuilding with ON DELETE SET NULL foreign keys");
    conn.execute_batch(
        "PRAGMA foreign_keys=OFF;
         BEGIN IMMEDIATE;
         CREATE TABLE request_logs_mig (
            id              TEXT PRIMARY KEY,
            user_id         TEXT REFERENCES users(id) ON DELETE SET NULL,
            api_key_id      TEXT REFERENCES api_keys(id) ON DELETE SET NULL,
            provider_id     TEXT REFERENCES providers(id) ON DELETE SET NULL,
            model           TEXT NOT NULL,
            request_type    TEXT NOT NULL DEFAULT 'chat',
            prompt_tokens   INTEGER NOT NULL DEFAULT 0,
            completion_tokens INTEGER NOT NULL DEFAULT 0,
            total_tokens    INTEGER NOT NULL DEFAULT 0,
            latency_ms      INTEGER NOT NULL DEFAULT 0,
            status_code     INTEGER NOT NULL DEFAULT 0,
            success         INTEGER NOT NULL DEFAULT 1,
            error_message   TEXT,
            created_at      TEXT NOT NULL DEFAULT (datetime('now'))
         );
         INSERT INTO request_logs_mig
            SELECT id, user_id, api_key_id, provider_id, model, request_type,
                   prompt_tokens, completion_tokens, total_tokens, latency_ms,
                   status_code, success, error_message, created_at
            FROM request_logs;
         DROP TABLE request_logs;
         ALTER TABLE request_logs_mig RENAME TO request_logs;
         CREATE INDEX IF NOT EXISTS idx_request_logs_user_id ON request_logs(user_id);
         CREATE INDEX IF NOT EXISTS idx_request_logs_created_at ON request_logs(created_at);
         CREATE INDEX IF NOT EXISTS idx_request_logs_provider_id ON request_logs(provider_id);
         COMMIT;
         PRAGMA foreign_keys=ON;",
    )?;
    tracing::info!("request_logs migration complete");
    Ok(())
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
            params![id, "admin", password_hash, "Administrator", "admin"],
        )?;
        tracing::warn!("============================================================");
        tracing::warn!("Default admin created — username: admin  password: {}", password);
        tracing::warn!("This password is shown only once. Log in and change it now.");
        tracing::warn!("============================================================");
    }
    Ok(())
}