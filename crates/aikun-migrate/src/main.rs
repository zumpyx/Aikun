//! # aikun-migrate:SQLite → SurrealDB 存量数据迁移工具(阶段 S)
//!
//! 读旧 SQLite `aikun.db` 全表 → 按设计文档(docs/surreal-migration.md
//! §五)的新旧字段映射写入 SurrealDB。**幂等可重跑**(按 record id
//! UPSERT 全字段覆盖);完成后打印各表行数对照,全部一致才报成功。
//!
//! ## 用法
//!
//! ```text
//! AIKUN_DATABASE_URL=sqlite:///path/aikun.db \
//! AIKUN_SURREAL_URL=surrealkv://./aikun.surreal \
//! aikun-migrate
//! ```
//!
//! 也可用 `--sqlite <url>` / `--surreal <url>` 覆盖(env 优先级同主程序:
//! env > CLI > 默认)。源库以只读方式打开,迁移绝不写源库。
//!
//! ## 闸门(user_version 的替代语义,设计文档 §四/§五)
//!
//! * 源库 `PRAGMA user_version` 必须等于 `EXPECTED_SOURCE_VERSION`(旧
//!   schema 的最终版本):更低 = 先用旧版二进制把库升满再迁;更高 =
//!   拒绝(禁止降级读取)。
//! * 目标库先 `ensure_schema`(版本闸门 + 幂等 DDL + 首启写
//!   `store_meta:schema.version`)——这就是旧 user_version 闸门的替代。
//!
//! ## 红线(设计文档头)
//!
//! 金额(balance/cost/amount/balance_after/面值)**微元整数原样直搬**,
//! 不乘不除;渠道 api_key **密文原样直搬**(同一 KeyCipher 派生,免重
//! 加密);数据搬运不改任何计费/兑换语义。

use rusqlite::Connection;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::types::{Array, RecordId, RecordIdKey, SurrealValue as _, Value as Sval};

use aikun_store::surreal::SurrealHandle;

/// 旧 SQLite schema 的最终版本(旧 schema.rs 的 SCHEMA_VERSION=3;源库
/// 必须已被旧二进制升到该版,否则列集与映射不符)。
const EXPECTED_SOURCE_VERSION: i64 = 3;

/// 单次提交的最大语句数(嵌入式引擎在调用任务栈上递归执行,分批限制
/// 单查询的语句量与栈深,docs/surreal-migration.md §一栈配置)。
const CHUNK_STMTS: usize = 200;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aikun_migrate=info".into()),
        )
        .init();

    // 嵌入式引擎查询在调用任务栈上内联递归:大栈运行时(≥10MiB,
    // 设计文档 §一栈配置,与 store 单测同口径)。
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(16 * 1024 * 1024)
        .build()
        .expect("build tokio runtime");
    if let Err(e) = runtime.block_on(run()) {
        eprintln!("迁移失败: {e}");
        std::process::exit(1);
    }
}

/// 简单 CLI:--sqlite <url> / --surreal <url>(可 --k=v);env 覆盖 CLI
/// (与主程序一致:env > CLI > 默认)。
fn parse_args() -> Result<(String, String), String> {
    let mut sqlite: Option<String> = None;
    let mut surreal: Option<String> = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let (key, inline_val) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        match key.as_str() {
            "--sqlite" | "--surreal" => {
                let v = inline_val
                    .or_else(|| it.next())
                    .ok_or_else(|| format!("参数 {key} 缺少值"))?;
                if key == "--sqlite" {
                    sqlite = Some(v);
                } else {
                    surreal = Some(v);
                }
            }
            "--help" | "-h" => {
                eprintln!("aikun-migrate [--sqlite <url>] [--surreal <url>]");
                eprintln!("env:AIKUN_DATABASE_URL(源)/ AIKUN_SURREAL_URL(目标)> CLI > 默认");
                std::process::exit(0);
            }
            other => return Err(format!("未知参数 {other}(可用:--sqlite/--surreal)")),
        }
    }
    let sqlite = std::env::var("AIKUN_DATABASE_URL")
        .ok()
        .or(sqlite)
        .unwrap_or_else(|| "sqlite://aikun.db?mode=rw".to_string());
    let surreal = std::env::var("AIKUN_SURREAL_URL")
        .ok()
        .or(surreal)
        .unwrap_or_else(|| "surrealkv://./aikun.surreal".to_string());
    Ok((sqlite, surreal))
}

async fn run() -> Result<(), String> {
    let (sqlite_url, surreal_url) = parse_args()?;
    println!("源(SQLite,只读): {sqlite_url}");
    println!("目标(SurrealDB)  : {surreal_url}");

    let sqlite = open_sqlite_readonly(&sqlite_url)?;
    gate_source_version(&sqlite)?;

    let endpoint = aikun_store::surreal::Endpoint::parse(&surreal_url)
        .map_err(|e| format!("surreal-url 非法: {e}"))?;
    let handle = SurrealHandle::connect(&endpoint)
        .await
        .map_err(|e| format!("SurrealDB 连接失败: {e}"))?;
    handle
        .ensure_schema()
        .await
        .map_err(|e| format!("目标库 schema 初始化失败: {e}"))?;
    println!("目标库 schema 就绪(store_meta:schema.version 闸门已过)");

    let db = handle.db();
    // 逐表搬运(顺序无依赖:record 链接不做 EXISTS 预检,dangling 读侧
    // 降级,设计文档 §四)。计数在搬运后对照。
    let mut report: Vec<(&'static str, i64, i64)> = Vec::new();
    macro_rules! migrate {
        ($old:literal, $new:literal, $f:expr) => {{
            let src = count_sqlite(&sqlite, $old)?;
            $f(&sqlite, db).await?;
            let dst = count_sureal(db, $new).await?;
            report.push(($new, src, dst));
            println!("{} 源 {src:>9} 行 → 目标 {dst:>9} 行", $new);
        }};
    }
    migrate!("users", "user", migrate_users);
    migrate!("api_keys", "api_key", migrate_api_keys);
    migrate!("providers", "provider", migrate_providers);
    migrate!("request_logs", "request_log", migrate_request_logs);
    migrate!("model_prices", "model_price", migrate_model_prices);
    // api_key_usage_daily 特例:v0.5 重构中期(计数器表落地)之前的 v3 库
    // 没有这张表——user_version 同为 3 但表集不同(仓库根 aikun.db 实测)。
    // 缺表按 0 行迁移而非中断:计数器本就可由 request_logs 明细重建
    // (新 schema 注释"明细是唯一真相"),中断反而留下写了一半的库。
    {
        let src = count_sqlite_opt(&sqlite, "api_key_usage_daily")?;
        migrate_api_key_usage_daily(&sqlite, db).await?;
        let dst = count_sureal(db, "api_key_usage_daily").await?;
        report.push(("api_key_usage_daily", src, dst));
        println!("api_key_usage_daily 源 {src:>9} 行 → 目标 {dst:>9} 行");
    }
    migrate!("usage_daily", "usage_daily", migrate_usage_daily);
    migrate!(
        "channel_affinity",
        "channel_affinity",
        migrate_channel_affinity
    );
    migrate!(
        "billing_transactions",
        "billing_transaction",
        migrate_billing_transactions
    );
    migrate!(
        "redemption_codes",
        "redemption_code",
        migrate_redemption_codes
    );
    migrate!("model_health", "model_health", migrate_model_health);

    println!("\n==== 各表行数对照 ====");
    println!(
        "{:<24} {:>12} {:>12} {:>6}",
        "表", "SQLite", "SurrealDB", "一致"
    );
    let mut all_match = true;
    for (table, src, dst) in &report {
        let ok = src == dst;
        all_match &= ok;
        println!(
            "{table:<24} {src:>12} {dst:>12} {}",
            if ok { "✓" } else { "✗" }
        );
    }
    if all_match {
        println!("\n迁移完成:各表行数一致;幂等可重跑(重跑按 record id 覆盖)。");
        Ok(())
    } else {
        Err("行数不一致:请检查上方差异表后重跑(幂等)".to_string())
    }
}

// ---------- SQLite 读取辅助 ----------

/// 只读打开源库(`?mode=rw` 等 query 参数剥掉,统一只读——迁移绝不写源库)。
fn open_sqlite_readonly(url: &str) -> Result<Connection, String> {
    let without_scheme = url.strip_prefix("sqlite://").unwrap_or(url);
    let path = without_scheme.split('?').next().unwrap_or("aikun.db");
    let path = if path.is_empty() { "aikun.db" } else { path };
    Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("打开源库 {path} 失败(应先由旧版二进制建库): {e}"))
}

/// 版本闸门:源库必须恰在 EXPECTED_SOURCE_VERSION(旧 schema 最终版)。
fn gate_source_version(conn: &Connection) -> Result<(), String> {
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| format!("读 user_version 失败: {e}"))?;
    match v.cmp(&EXPECTED_SOURCE_VERSION) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Less => Err(format!(
            "源库 user_version={v} 低于 {EXPECTED_SOURCE_VERSION}:请先用旧版 aikun 二进制启动一次\
             把库升满,再执行迁移"
        )),
        std::cmp::Ordering::Greater => Err(format!(
            "源库 user_version={v} 高于本工具支持的 {EXPECTED_SOURCE_VERSION}(禁止降级读取)"
        )),
    }
}

fn count_sqlite(conn: &Connection, table: &str) -> Result<i64, String> {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .map_err(|e| format!("统计 {table} 行数失败: {e}"))
}

/// 表是否存在(sqlite_master 直查;仅 api_key_usage_daily 的缺表容忍用)。
fn table_exists(conn: &Connection, table: &str) -> Result<bool, String> {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .map_err(|e| format!("查 sqlite_master 失败: {e}"))
}

/// 同 [`count_sqlite`],但表缺失按 0 行(而不是报错)。仅用于
/// api_key_usage_daily:部分 v3 库没有这张表(见 run 内注释),语义上
/// 就是"无计数器数据";其余表缺失说明源库不对,仍应报错。
fn count_sqlite_opt(conn: &Connection, table: &str) -> Result<i64, String> {
    if !table_exists(conn, table)? {
        tracing::warn!("源库缺 {table} 表(旧 v3 库形态):按 0 行迁移");
        return Ok(0);
    }
    count_sqlite(conn, table)
}

async fn count_sureal(db: &Surreal<Any>, table: &str) -> Result<i64, String> {
    let mut res = db
        .query(format!("SELECT count() AS c FROM {table} GROUP ALL"))
        .await
        .map_err(|e| format!("统计 {table} 行数失败: {e}"))?
        .check()
        .map_err(|e| format!("统计 {table} 行数失败: {e}"))?;
    let rows: Vec<serde_json::Value> = res.take(0).map_err(|e| e.to_string())?;
    Ok(rows
        .first()
        .and_then(|r| r.get("c"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0))
}

// ---------- 字段变换辅助(设计文档 §五) ----------

type DT = chrono::DateTime<chrono::Utc>;

/// 时间戳双格式解析:RFC3339(`2026-01-01T00:00:00Z`/`+00:00`)与
/// SQLite `datetime('now')`(`2026-01-01 00:00:00`,按 UTC);空串/NULL
/// → None。迁移后统一为原生 datetime。
fn parse_dt(raw: Option<String>) -> Option<DT> {
    let s = raw?.trim().to_string();
    if s.is_empty() {
        return None;
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(&s) {
        return Some(t.with_timezone(&chrono::Utc));
    }
    chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|n| n.and_utc())
}

/// 必填时间:解析失败报错(旧行时间戳必须可解析,静默丢弃会破坏对账)。
fn parse_dt_req(raw: Option<String>, table: &str, id: &str) -> Result<DT, String> {
    parse_dt(raw).ok_or_else(|| format!("{table} 行 {id} 的时间戳缺失或不可解析"))
}

/// JSON 文本 → array<string>(''/坏 JSON → [],设计文档 §五)。
fn parse_json_array(raw: Option<String>) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(&raw.unwrap_or_default()).unwrap_or_default()
}

/// JSON 文本 → object(''/坏 JSON → {})。
fn parse_json_object(raw: Option<String>) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(&raw.unwrap_or_default())
        .unwrap_or_else(|_| serde_json::json!({}))
}

fn rid(table: &str, key: &str) -> RecordId {
    RecordId::new(table, RecordIdKey::from(key))
}

type Bind = (String, Sval);

fn s_str(v: String) -> Sval {
    Sval::String(v)
}
fn s_opt_str(v: Option<String>) -> Sval {
    v.map(Sval::String).unwrap_or(Sval::None)
}
fn s_i64(v: i64) -> Sval {
    v.into_value()
}
fn s_f64(v: f64) -> Sval {
    v.into_value()
}
fn s_bool(v: i64) -> Sval {
    Sval::from_bool(v != 0)
}
fn s_opt_f64(v: Option<f64>) -> Sval {
    v.map(s_f64).unwrap_or(Sval::None)
}
fn s_dt(v: DT) -> Sval {
    v.into_value()
}
fn s_opt_dt(v: Option<DT>) -> Sval {
    v.map(s_dt).unwrap_or(Sval::None)
}
fn s_arr(v: Vec<String>) -> Sval {
    let items: Vec<Sval> = v.into_iter().map(Sval::String).collect();
    Sval::Array(Array::from(items))
}
fn s_obj(v: serde_json::Value) -> Sval {
    v.into_value()
}

/// 分批执行 `UPSERT <record id> SET …`(幂等全字段覆盖):每 CHUNK_STMTS
/// 行一个 BEGIN…COMMIT;行内绑定参数重命名为全局唯一,避免跨行撞名。
async fn upsert_rows(
    db: &Surreal<Any>,
    table: &str,
    rows: Vec<(String, Vec<Bind>)>,
) -> Result<(), String> {
    for chunk in rows.chunks(CHUNK_STMTS) {
        let mut sql = String::from("BEGIN TRANSACTION;");
        let mut binds: Vec<Bind> = Vec::with_capacity(chunk.len() * 8);
        for (i, (stmt, bs)) in chunk.iter().enumerate() {
            let mut renamed = stmt.clone();
            for (k, v) in bs {
                let new_key = format!("r{i}_{k}");
                renamed = renamed.replace(&format!("${k}"), &format!("${new_key}"));
                binds.push((new_key, v.clone()));
            }
            sql.push('\n');
            sql.push_str(&renamed);
            sql.push(';');
        }
        sql.push('\n');
        sql.push_str("COMMIT TRANSACTION;");
        let mut q = db.query(sql);
        for (k, v) in &binds {
            q = q.bind((k.as_str(), v.clone()));
        }
        q.await
            .map_err(|e| format!("写入 {table} 失败: {e}"))?
            .check()
            .map_err(|e| format!("写入 {table} 失败: {e}"))?;
    }
    Ok(())
}

/// 读一行的一组列,统一把 rusqlite 错误转成 String(table 上下文)。
macro_rules! col {
    ($row:expr, $idx:literal) => {
        $row.get($idx).map_err(|e| e.to_string())?
    };
}

// ---------- 各表迁移(映射 = docs/surreal-migration.md §五) ----------

async fn migrate_users(sqlite: &Connection, db: &Surreal<Any>) -> Result<(), String> {
    let mut stmt = sqlite
        .prepare(
            "SELECT id, username, password_hash, display_name, role, is_active,
                    token_version, balance, created_at, updated_at FROM users",
        )
        .map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| format!("读 users 失败: {e}"))?;
    let mut out: Vec<(String, Vec<Bind>)> = Vec::new();
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let id: String = col!(r, 0);
        // balance:微元整数直搬(红线:不经浮点)。
        out.push((
            "UPSERT type::record('user', $k) SET username = $username, \
             password_hash = $password_hash, display_name = $display_name, \
             role = $role, is_active = $is_active, token_version = $token_version, \
             balance = $balance, created_at = $created_at, updated_at = $updated_at"
                .to_string(),
            vec![
                ("k".into(), s_str(id.clone())),
                ("username".into(), s_str(col!(r, 1))),
                ("password_hash".into(), s_str(col!(r, 2))),
                ("display_name".into(), s_str(col!(r, 3))),
                ("role".into(), s_str(col!(r, 4))),
                ("is_active".into(), s_bool(col!(r, 5))),
                ("token_version".into(), s_i64(col!(r, 6))),
                ("balance".into(), s_i64(col!(r, 7))),
                (
                    "created_at".into(),
                    s_dt(parse_dt_req(r.get(8).ok(), "users", &id)?),
                ),
                (
                    "updated_at".into(),
                    s_dt(parse_dt_req(r.get(9).ok(), "users", &id)?),
                ),
            ],
        ));
    }
    upsert_rows(db, "user", out).await
}

async fn migrate_api_keys(sqlite: &Connection, db: &Surreal<Any>) -> Result<(), String> {
    let mut stmt = sqlite
        .prepare(
            "SELECT id, user_id, key, key_suffix, name, is_active, last_used_at, expires_at,
                    models, rate_limit_rpm, quota_daily_tokens, max_concurrent, created_at
             FROM api_keys",
        )
        .map_err(|e| e.to_string())?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("读 api_keys 失败: {e}"))?;
    let mut out: Vec<(String, Vec<Bind>)> = Vec::new();
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let id: String = col!(r, 0);
        let user_id: String = col!(r, 1);
        out.push((
            "UPSERT type::record('api_key', $k) SET user = type::record('user', $user), \
             key = $key, key_suffix = $key_suffix, name = $name, is_active = $is_active, \
             last_used_at = $last_used_at, expires_at = $expires_at, models = $models, \
             rate_limit_rpm = $rate_limit_rpm, quota_daily_tokens = $quota_daily_tokens, \
             max_concurrent = $max_concurrent, created_at = $created_at"
                .to_string(),
            vec![
                ("k".into(), s_str(id.clone())),
                ("user".into(), s_str(user_id)),
                ("key".into(), s_str(col!(r, 2))),
                ("key_suffix".into(), s_str(col!(r, 3))),
                ("name".into(), s_str(col!(r, 4))),
                ("is_active".into(), s_bool(col!(r, 5))),
                ("last_used_at".into(), s_opt_dt(parse_dt(r.get(6).ok()))),
                ("expires_at".into(), s_opt_dt(parse_dt(r.get(7).ok()))),
                // models:JSON 文本 → array<string>;''/坏 JSON → []
                ("models".into(), s_arr(parse_json_array(r.get(8).ok()))),
                ("rate_limit_rpm".into(), s_i64(col!(r, 9))),
                ("quota_daily_tokens".into(), s_i64(col!(r, 10))),
                ("max_concurrent".into(), s_i64(col!(r, 11))),
                (
                    "created_at".into(),
                    s_dt(parse_dt_req(r.get(12).ok(), "api_keys", &id)?),
                ),
            ],
        ));
    }
    upsert_rows(db, "api_key", out).await
}

async fn migrate_providers(sqlite: &Connection, db: &Surreal<Any>) -> Result<(), String> {
    let mut stmt = sqlite
        .prepare(
            "SELECT id, name, provider_type, openai_base_url, anthropic_base_url, api_key,
                    key_hash, models, model_mapping, protocols, default_protocol, priority,
                    weight, is_active, health_status, latency_ms, error_rate,
                    last_health_check, max_retries, timeout_secs, proxy_url, note,
                    website_url, channel_kind, consecutive_failures, disabled_reason,
                    created_at, updated_at
             FROM providers",
        )
        .map_err(|e| e.to_string())?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("读 providers 失败: {e}"))?;
    let mut out: Vec<(String, Vec<Bind>)> = Vec::new();
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let id: String = col!(r, 0);
        out.push((
            "UPSERT type::record('provider', $k) SET name = $name, \
             provider_type = $provider_type, openai_base_url = $openai_base_url, \
             anthropic_base_url = $anthropic_base_url, api_key = $api_key, \
             key_hash = $key_hash, models = $models, model_mapping = $model_mapping, \
             protocols = $protocols, default_protocol = $default_protocol, \
             priority = $priority, weight = $weight, is_active = $is_active, \
             health_status = $health_status, latency_ms = $latency_ms, \
             error_rate = $error_rate, last_health_check = $last_health_check, \
             consecutive_failures = $consecutive_failures, disabled_reason = $disabled_reason, \
             max_retries = $max_retries, timeout_secs = $timeout_secs, proxy_url = $proxy_url, \
             note = $note, website_url = $website_url, channel_kind = $channel_kind, \
             created_at = $created_at, updated_at = $updated_at"
                .to_string(),
            vec![
                ("k".into(), s_str(id.clone())),
                ("name".into(), s_str(col!(r, 1))),
                // provider_type:openapi 契约必留字段,直搬
                ("provider_type".into(), s_str(col!(r, 2))),
                ("openai_base_url".into(), s_str(col!(r, 3))),
                ("anthropic_base_url".into(), s_str(col!(r, 4))),
                // api_key:enc:v1: 密文原样直搬(同一 KeyCipher,免重加密;红线)
                ("api_key".into(), s_str(col!(r, 5))),
                ("key_hash".into(), s_str(col!(r, 6))),
                ("models".into(), s_arr(parse_json_array(r.get(7).ok()))),
                (
                    "model_mapping".into(),
                    s_obj(parse_json_object(r.get(8).ok())),
                ),
                ("protocols".into(), s_arr(parse_json_array(r.get(9).ok()))),
                ("default_protocol".into(), s_str(col!(r, 10))),
                ("priority".into(), s_i64(col!(r, 11))),
                ("weight".into(), s_f64(col!(r, 12))),
                ("is_active".into(), s_bool(col!(r, 13))),
                ("health_status".into(), s_str(col!(r, 14))),
                ("latency_ms".into(), s_f64(col!(r, 15))),
                ("error_rate".into(), s_f64(col!(r, 16))),
                (
                    "last_health_check".into(),
                    s_opt_dt(parse_dt(r.get(17).ok())),
                ),
                ("max_retries".into(), s_i64(col!(r, 18))),
                ("timeout_secs".into(), s_i64(col!(r, 19))),
                ("proxy_url".into(), s_str(col!(r, 20))),
                ("note".into(), s_str(col!(r, 21))),
                ("website_url".into(), s_str(col!(r, 22))),
                ("channel_kind".into(), s_str(col!(r, 23))),
                ("consecutive_failures".into(), s_i64(col!(r, 24))),
                ("disabled_reason".into(), s_str(col!(r, 25))),
                (
                    "created_at".into(),
                    s_dt(parse_dt_req(r.get(26).ok(), "providers", &id)?),
                ),
                (
                    "updated_at".into(),
                    s_dt(parse_dt_req(r.get(27).ok(), "providers", &id)?),
                ),
            ],
        ));
    }
    upsert_rows(db, "provider", out).await
}

async fn migrate_request_logs(sqlite: &Connection, db: &Surreal<Any>) -> Result<(), String> {
    // 本表通常最大:流式读取 + 分批写,内存只持一个 chunk 的行。
    let mut stmt = sqlite
        .prepare(
            "SELECT id, user_id, api_key_id, provider_id, model, request_type, prompt_tokens,
                    completion_tokens, total_tokens, cached_tokens, latency_ms, status_code,
                    success, error_message, cost, created_at
             FROM request_logs",
        )
        .map_err(|e| e.to_string())?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("读 request_logs 失败: {e}"))?;
    let mut batch: Vec<(String, Vec<Bind>)> = Vec::with_capacity(CHUNK_STMTS);
    let mut total: i64 = 0;
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let id: String = col!(r, 0);
        // 关联列:NULL → NONE(record 链接,dangling 读侧降级,设计文档 §四)。
        let user: Option<String> = r.get(1).map_err(|e| e.to_string())?;
        let api_key: Option<String> = r.get(2).map_err(|e| e.to_string())?;
        let provider: Option<String> = r.get(3).map_err(|e| e.to_string())?;
        let link = |t: &str, id: &Option<String>| -> Sval {
            id.as_deref()
                .map(|k| Sval::RecordId(rid(t, k)))
                .unwrap_or(Sval::None)
        };
        batch.push((
            "UPSERT type::record('request_log', $k) SET user = $user, api_key = $api_key, \
             provider = $provider, model = $model, request_type = $request_type, \
             prompt_tokens = $prompt_tokens, completion_tokens = $completion_tokens, \
             total_tokens = $total_tokens, cached_tokens = $cached_tokens, \
             latency_ms = $latency_ms, status_code = $status_code, success = $success, \
             error_message = $error_message, cost = $cost, created_at = $created_at"
                .to_string(),
            vec![
                ("k".into(), s_str(id.clone())),
                ("user".into(), link("user", &user)),
                ("api_key".into(), link("api_key", &api_key)),
                ("provider".into(), link("provider", &provider)),
                ("model".into(), s_str(col!(r, 4))),
                ("request_type".into(), s_str(col!(r, 5))),
                ("prompt_tokens".into(), s_i64(col!(r, 6))),
                ("completion_tokens".into(), s_i64(col!(r, 7))),
                ("total_tokens".into(), s_i64(col!(r, 8))),
                ("cached_tokens".into(), s_i64(col!(r, 9))),
                ("latency_ms".into(), s_i64(col!(r, 10))),
                ("status_code".into(), s_i64(col!(r, 11))),
                ("success".into(), s_bool(col!(r, 12))),
                ("error_message".into(), s_opt_str(r.get(13).ok())),
                // cost:微元整数直搬(红线)
                ("cost".into(), s_i64(col!(r, 14))),
                // 双格式解析是本表关键(两种格式混存量最大)
                (
                    "created_at".into(),
                    s_dt(parse_dt_req(r.get(15).ok(), "request_logs", &id)?),
                ),
            ],
        ));
        total += 1;
        if batch.len() >= CHUNK_STMTS {
            upsert_rows(db, "request_log", std::mem::take(&mut batch)).await?;
            tracing::info!("request_log 已迁移 {total} 行…");
        }
    }
    upsert_rows(db, "request_log", batch).await?;
    Ok(())
}

async fn migrate_model_prices(sqlite: &Connection, db: &Surreal<Any>) -> Result<(), String> {
    let mut stmt = sqlite
        .prepare(
            "SELECT id, model, prompt_price, completion_price, cached_price, created_at, updated_at
             FROM model_prices",
        )
        .map_err(|e| e.to_string())?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("读 model_prices 失败: {e}"))?;
    let mut out: Vec<(String, Vec<Bind>)> = Vec::new();
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let id: String = col!(r, 0);
        // 价格是 float 费率(每 1M tokens),非金额,保持浮点口径。
        out.push((
            "UPSERT type::record('model_price', $k) SET model = $model, \
             prompt_price = $prompt_price, completion_price = $completion_price, \
             cached_price = $cached_price, created_at = $created_at, updated_at = $updated_at"
                .to_string(),
            vec![
                ("k".into(), s_str(id.clone())),
                ("model".into(), s_str(col!(r, 1))),
                ("prompt_price".into(), s_f64(col!(r, 2))),
                ("completion_price".into(), s_f64(col!(r, 3))),
                ("cached_price".into(), s_opt_f64(r.get(4).ok())),
                (
                    "created_at".into(),
                    s_dt(parse_dt_req(r.get(5).ok(), "model_prices", &id)?),
                ),
                (
                    "updated_at".into(),
                    s_dt(parse_dt_req(r.get(6).ok(), "model_prices", &id)?),
                ),
            ],
        ));
    }
    upsert_rows(db, "model_price", out).await
}

async fn migrate_api_key_usage_daily(sqlite: &Connection, db: &Surreal<Any>) -> Result<(), String> {
    // 缺表(旧 v3 库)→ 0 行,直接跳过;与 count_sqlite_opt 同口径。
    if !table_exists(sqlite, "api_key_usage_daily")? {
        tracing::warn!("api_key_usage_daily 表不存在:跳过(计数器可由 request_logs 明细重建)");
        return Ok(());
    }
    let mut stmt = sqlite
        .prepare("SELECT api_key_id, date, tokens_spent FROM api_key_usage_daily")
        .map_err(|e| e.to_string())?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("读 api_key_usage_daily 失败: {e}"))?;
    let mut out: Vec<(String, Vec<Bind>)> = Vec::new();
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let key: String = col!(r, 0);
        let date: String = col!(r, 1);
        // id = [api_key:<id>, '<date>'];api_key/date 冗余存(查询面)。
        out.push((
            "UPSERT type::record('api_key_usage_daily', [$link, $date]) SET \
             api_key = $link, date = $date, tokens_spent = $tokens_spent"
                .to_string(),
            vec![
                ("link".into(), Sval::RecordId(rid("api_key", &key))),
                ("date".into(), s_str(date)),
                ("tokens_spent".into(), s_i64(col!(r, 2))),
            ],
        ));
    }
    upsert_rows(db, "api_key_usage_daily", out).await
}

async fn migrate_usage_daily(sqlite: &Connection, db: &Surreal<Any>) -> Result<(), String> {
    let mut stmt = sqlite
        .prepare("SELECT user_id, date, requests, tokens, cost FROM usage_daily")
        .map_err(|e| e.to_string())?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("读 usage_daily 失败: {e}"))?;
    let mut out: Vec<(String, Vec<Bind>)> = Vec::new();
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let user: String = col!(r, 0);
        let date: String = col!(r, 1);
        // cost:微元整数直搬(红线)。
        out.push((
            "UPSERT type::record('usage_daily', [$link, $date]) SET user = $link, \
             date = $date, requests = $requests, tokens = $tokens, cost = $cost"
                .to_string(),
            vec![
                ("link".into(), Sval::RecordId(rid("user", &user))),
                ("date".into(), s_str(date)),
                ("requests".into(), s_i64(col!(r, 2))),
                ("tokens".into(), s_i64(col!(r, 3))),
                ("cost".into(), s_i64(col!(r, 4))),
            ],
        ));
    }
    upsert_rows(db, "usage_daily", out).await
}

async fn migrate_channel_affinity(sqlite: &Connection, db: &Surreal<Any>) -> Result<(), String> {
    let mut stmt = sqlite
        .prepare("SELECT api_key_id, model, provider_id FROM channel_affinity")
        .map_err(|e| e.to_string())?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("读 channel_affinity 失败: {e}"))?;
    let mut out: Vec<(String, Vec<Bind>)> = Vec::new();
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let key: String = col!(r, 0);
        let model: String = col!(r, 1);
        let provider: String = col!(r, 2);
        // id = [api_key:<id>, '<model>']。
        out.push((
            "UPSERT type::record('channel_affinity', [$link, $model]) SET \
             provider = $provider"
                .to_string(),
            vec![
                ("link".into(), Sval::RecordId(rid("api_key", &key))),
                ("model".into(), s_str(model)),
                (
                    "provider".into(),
                    Sval::RecordId(rid("provider", &provider)),
                ),
            ],
        ));
    }
    upsert_rows(db, "channel_affinity", out).await
}

async fn migrate_billing_transactions(
    sqlite: &Connection,
    db: &Surreal<Any>,
) -> Result<(), String> {
    let mut stmt = sqlite
        .prepare(
            "SELECT id, user_id, amount, balance_after, kind, note, created_at
             FROM billing_transactions",
        )
        .map_err(|e| e.to_string())?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("读 billing_transactions 失败: {e}"))?;
    let mut out: Vec<(String, Vec<Bind>)> = Vec::new();
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let id: String = col!(r, 0);
        let user: String = col!(r, 1);
        // amount/balance_after:微元整数直搬(红线)。
        out.push((
            "UPSERT type::record('billing_transaction', $k) SET user = $user, \
             amount = $amount, balance_after = $balance_after, kind = $kind, \
             note = $note, created_at = $created_at"
                .to_string(),
            vec![
                ("k".into(), s_str(id.clone())),
                ("user".into(), Sval::RecordId(rid("user", &user))),
                ("amount".into(), s_i64(col!(r, 2))),
                ("balance_after".into(), s_i64(col!(r, 3))),
                ("kind".into(), s_str(col!(r, 4))),
                ("note".into(), s_str(col!(r, 5))),
                (
                    "created_at".into(),
                    s_dt(parse_dt_req(r.get(6).ok(), "billing_transactions", &id)?),
                ),
            ],
        ));
    }
    upsert_rows(db, "billing_transaction", out).await
}

async fn migrate_redemption_codes(sqlite: &Connection, db: &Surreal<Any>) -> Result<(), String> {
    let mut stmt = sqlite
        .prepare(
            "SELECT id, code_hash, code_suffix, amount, batch, status, used_by, used_at,
                    expires_at, note, created_at
             FROM redemption_codes",
        )
        .map_err(|e| e.to_string())?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("读 redemption_codes 失败: {e}"))?;
    let mut out: Vec<(String, Vec<Bind>)> = Vec::new();
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let id: String = col!(r, 0);
        let used_by: Option<String> = r.get(6).map_err(|e| e.to_string())?;
        let used_by_val = used_by
            .as_deref()
            .map(|u| Sval::RecordId(rid("user", u)))
            .unwrap_or(Sval::None);
        // amount:面值微元整数直搬(红线)。
        out.push((
            "UPSERT type::record('redemption_code', $k) SET code_hash = $code_hash, \
             code_suffix = $code_suffix, amount = $amount, batch = $batch, \
             status = $status, used_by = $used_by, used_at = $used_at, \
             expires_at = $expires_at, note = $note, created_at = $created_at"
                .to_string(),
            vec![
                ("k".into(), s_str(id.clone())),
                ("code_hash".into(), s_str(col!(r, 1))),
                ("code_suffix".into(), s_str(col!(r, 2))),
                ("amount".into(), s_i64(col!(r, 3))),
                ("batch".into(), s_str(col!(r, 4))),
                ("status".into(), s_str(col!(r, 5))),
                ("used_by".into(), used_by_val),
                ("used_at".into(), s_opt_dt(parse_dt(r.get(7).ok()))),
                ("expires_at".into(), s_opt_dt(parse_dt(r.get(8).ok()))),
                ("note".into(), s_str(col!(r, 9))),
                (
                    "created_at".into(),
                    s_dt(parse_dt_req(r.get(10).ok(), "redemption_codes", &id)?),
                ),
            ],
        ));
    }
    upsert_rows(db, "redemption_code", out).await
}

async fn migrate_model_health(sqlite: &Connection, db: &Surreal<Any>) -> Result<(), String> {
    let mut stmt = sqlite
        .prepare(
            "SELECT provider_id, model, status, latency_ms, error, checked_at FROM model_health",
        )
        .map_err(|e| e.to_string())?;
    let mut rows = stmt
        .query([])
        .map_err(|e| format!("读 model_health 失败: {e}"))?;
    let mut out: Vec<(String, Vec<Bind>)> = Vec::new();
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        let provider: String = col!(r, 0);
        let model: String = col!(r, 1);
        // id = [provider:<id>, '<model>'];provider 冗余存(列表查询)。
        out.push((
            "UPSERT type::record('model_health', [$link, $model]) SET provider = $link, \
             status = $status, latency_ms = $latency_ms, error = $error, checked_at = $checked_at"
                .to_string(),
            vec![
                ("link".into(), Sval::RecordId(rid("provider", &provider))),
                ("model".into(), s_str(model)),
                ("status".into(), s_str(col!(r, 2))),
                ("latency_ms".into(), s_f64(col!(r, 3))),
                ("error".into(), s_str(col!(r, 4))),
                ("checked_at".into(), s_opt_dt(parse_dt(r.get(5).ok()))),
            ],
        ));
    }
    upsert_rows(db, "model_health", out).await
}
