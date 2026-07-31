//! E2E 测试基建:axum mock 上游 + 拉起真实 aikun 二进制 + DB 夹具。
//! 不改生产代码结构:测试通过 CARGO_BIN_EXE 启动完整应用(含迁移),
//! 用 rusqlite 直连临时 SQLite 做夹具注入和异步记账断言。
#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const TEST_KEY: &str = "sk-e2e-test-key";

/// 取一个空闲端口(绑定后立刻释放,竞态窗口极小,可接受)。
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// 与 src/auth 的 hash_api_key 同算法:hex(sha256(key))。
pub fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(s.as_bytes()))
}

// ---------- Mock 上游 ----------

pub enum Scripted {
    Json(u16, Value),
    /// body 原样返回,content-type: text/event-stream。
    Sse(String),
}

#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    /// Authorization 头(无则 None),用于断言转发给上游的凭证。
    pub authorization: Option<String>,
    pub body: String,
}

struct MockState {
    script: Mutex<VecDeque<Scripted>>,
    requests: Mutex<Vec<Recorded>>,
}

/// 脚本化 mock 上游:按队列依次返回预设响应,脚本耗尽后回默认的
/// OpenAI 补全(200,也覆盖健康检查的 GET /models)。所有请求被记录,
/// 用于断言网关发给上游的内容。
pub struct MockUpstream {
    pub base: String,
    state: Arc<MockState>,
}

impl MockUpstream {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(MockState {
            script: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
        });
        let app = axum::Router::new()
            .fallback(axum::routing::any(mock_handler))
            .with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base: format!("http://127.0.0.1:{}", port),
            state,
        }
    }

    pub fn push_json(&self, status: u16, body: Value) {
        self.state
            .script
            .lock()
            .unwrap()
            .push_back(Scripted::Json(status, body));
    }

    pub fn push_sse(&self, body: &str) {
        self.state
            .script
            .lock()
            .unwrap()
            .push_back(Scripted::Sse(body.to_string()));
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.requests.lock().unwrap().clone()
    }
}

async fn mock_handler(
    axum::extract::State(st): axum::extract::State<Arc<MockState>>,
    req: axum::extract::Request,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    // 健康探针(GET /models)不消费脚本:启动首轮健康检查与夹具注入
    // 存在调度竞态,探针一律回默认 200,保证脚本只被真实代理请求消耗。
    let is_health_probe = path.ends_with("/models");
    let authorization = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let bytes = axum::body::to_bytes(req.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap_or_default();
    st.requests.lock().unwrap().push(Recorded {
        method,
        path,
        authorization,
        body: String::from_utf8_lossy(&bytes).to_string(),
    });
    let scripted = if is_health_probe {
        None
    } else {
        st.script.lock().unwrap().pop_front()
    };
    match scripted {
        Some(Scripted::Json(status, v)) => (
            axum::http::StatusCode::from_u16(status).unwrap(),
            axum::Json(v),
        )
            .into_response(),
        Some(Scripted::Sse(body)) => (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            body,
        )
            .into_response(),
        None => axum::Json(openai_completion("OK")).into_response(),
    }
}

pub fn openai_completion(text: &str) -> Value {
    json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
    })
}

// ---------- 被测应用 ----------

pub struct TestApp {
    pub base: String,
    pub db_path: PathBuf,
    port: u16,
    child: Child,
    dir: tempfile::TempDir,
}

fn spawn_child(dir: &std::path::Path, db_path: &std::path::Path, port: u16) -> Child {
    Command::new(env!("CARGO_BIN_EXE_aikun"))
        .current_dir(dir)
        .env("AIKUN_HOST", format!("127.0.0.1:{}", port))
        .env("AIKUN_JWT_SECRET", "e2e-test-secret-not-for-production")
        .env(
            "AIKUN_DATABASE_URL",
            format!("sqlite://{}?mode=rwc", db_path.display()),
        )
        .env("AIKUN_HEALTH_CHECK_INTERVAL", "3600")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn aikun binary")
}

impl TestApp {
    /// 拉起真实 aikun 二进制:独立临时目录作为 cwd(避免读到仓库的
    /// .env)、独立 SQLite、随机端口、超长健康检查间隔(不干扰断言)。
    pub async fn spawn() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let port = free_port();
        let child = spawn_child(dir.path(), &db_path, port);
        let app = Self {
            base: format!("http://127.0.0.1:{}", port),
            db_path: db_path.clone(),
            port,
            child,
            dir,
        };
        app.wait_ready().await;
        app
    }

    /// 杀掉进程并在同一目录/数据库/端口上重新拉起——用于验证启动期
    /// 迁移(如存量明文渠道 key 的重加密)。
    pub async fn restart(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.child = spawn_child(self.dir.path(), &self.db_path, self.port);
        self.wait_ready().await;
    }

    async fn wait_ready(&self) {
        let client = reqwest::Client::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(r) = client
                .get(format!("{}/api/health", self.base))
                .send()
                .await
            {
                if r.status().is_success() {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "aikun did not become ready within 15s"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn db(&self) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(&self.db_path).unwrap();
        conn.execute_batch("PRAGMA busy_timeout=5000;").unwrap();
        conn
    }

    pub fn client(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    /// 给请求加上测试 API key 的 Bearer 头。
    pub fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("Authorization", format!("Bearer {}", TEST_KEY))
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------- DB 夹具 ----------

/// 插入一个用户和一把 API key(TEST_KEY 的 sha256)。
pub fn seed_api_key(db: &rusqlite::Connection) {
    seed_api_key_with_limits(db, 0, 0);
}

/// 同 seed_api_key,但设置 rate_limit_rpm / quota_daily_tokens(0 = 不限)。
pub fn seed_api_key_with_limits(db: &rusqlite::Connection, rpm: i64, daily_quota: i64) {
    db.execute(
        "INSERT INTO users (id, username, password_hash, display_name, role)
         VALUES ('u-e2e', 'e2e', 'unused', 'E2E', 'admin')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO api_keys (id, user_id, key, name, rate_limit_rpm, quota_daily_tokens)
         VALUES ('k-e2e', 'u-e2e', ?1, 'e2e', ?2, ?3)",
        rusqlite::params![sha256_hex(TEST_KEY), rpm, daily_quota],
    )
    .unwrap();
}

pub struct ProviderSeed<'a> {
    pub id: &'a str,
    pub name: &'a str,
    /// "openai" | "anthropic",同时写入 provider_type/protocols/default_protocol。
    pub protocol: &'a str,
    /// mock 的 base,两路地址写同一个。
    pub base_url: &'a str,
    /// JSON array,如 "[\"gpt-4\"]"。
    pub models: &'a str,
    pub priority: i32,
    pub weight: f64,
    pub health_status: &'a str,
    pub error_rate: f64,
}

pub fn seed_provider(db: &rusqlite::Connection, p: &ProviderSeed) {
    let protocols = format!("[\"{}\"]", p.protocol);
    db.execute(
        "INSERT INTO providers (id, name, provider_type, openai_base_url, anthropic_base_url,
                                api_key, models, priority, weight, health_status, error_rate,
                                protocols, default_protocol)
         VALUES (?1, ?2, ?3, ?4, ?4, 'upstream-key', ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
        rusqlite::params![
            p.id,
            p.name,
            p.protocol,
            p.base_url,
            p.models,
            p.priority,
            p.weight,
            p.health_status,
            p.error_rate,
            protocols,
        ],
    )
    .unwrap();
}

/// 用测试密钥签发任意身份的 JWT(token_version=0;用户需在 users 表中
/// 存在且 is_active=1,否则 authenticate_jwt 会拒绝)。
pub fn sign_jwt(sub: &str, username: &str, role: &str) -> String {
    #[derive(serde::Serialize)]
    struct TestClaims {
        sub: String,
        username: String,
        role: String,
        exp: usize,
        iat: usize,
        token_version: i64,
    }
    // app 进程的 AIKUN_JWT_SECRET 固定为 e2e-test-secret-not-for-production
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as usize;
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &TestClaims {
            sub: sub.to_string(),
            username: username.to_string(),
            role: role.to_string(),
            exp: now + 3600,
            iat: now,
            token_version: 0,
        },
        &jsonwebtoken::EncodingKey::from_secret(b"e2e-test-secret-not-for-production"),
    )
    .unwrap()
}

/// 用测试密钥直接签发 admin JWT(u-e2e 角色为 admin、token_version=0)。
pub fn admin_jwt() -> String {
    sign_jwt("u-e2e", "e2e", "admin")
}

/// 记账/禁用都是异步 fire-and-forget:断言前轮询等待条件成立。
pub async fn wait_until(desc: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !f() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for: {}",
            desc
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
