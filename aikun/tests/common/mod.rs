//! E2E 测试基建:axum mock 上游 + 拉起真实 aikun 二进制 + 管理面 API 检查通道。
//! 不改生产代码结构:测试通过 CARGO_BIN_EXE 启动完整应用。
//!
//! 阶段 S 检查通道(docs/surreal-migration.md §七):嵌入式 SurrealKV 数据
//! 文件被服务进程独占,测试进程不能直连 seed/断言——全部改经管理面 API:
//! spawn 时注入已知 admin 密码(AIKUN_RESET_ADMIN_PASSWORD,启动即重置)
//! → 登录 /api/auth/login 拿 JWT → seed(建用户/渠道/价格)与断言(日志/
//! 余额/渠道状态)全走管理面端点。库内形态检查(密文落库、哈希存储、微元
//! 整数)由 aikun-store 的 surreal 单测承担(设计文档 §七第 2 层)。
#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// 检查通道的已知 admin 密码(spawn 注入 AIKUN_RESET_ADMIN_PASSWORD;
/// restart 后启动期同样重置,故 restart 会丢弃缓存的 admin token)。
pub const E2E_ADMIN_PASSWORD: &str = "e2e-admin-password-2026";
const ADMIN_USERNAME: &str = "admin";

/// 取一个空闲端口(绑定后立刻释放,竞态窗口极小,可接受)。
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

// ---------- Mock 上游 ----------

pub enum Scripted {
    Json(u16, Value),
    /// body 原样返回,content-type: text/event-stream。
    Sse(String),
    /// 延迟 delay 后按 Json 返回:模拟慢上游,用于并发/超时类断言。
    DelayedJson(std::time::Duration, u16, Value),
    /// 延迟 delay 后按 Sse 返回(流式慢上游)。
    DelayedSse(std::time::Duration, String),
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

    pub fn push_delayed_json(&self, delay: std::time::Duration, status: u16, body: Value) {
        self.state
            .script
            .lock()
            .unwrap()
            .push_back(Scripted::DelayedJson(delay, status, body));
    }

    pub fn push_delayed_sse(&self, delay: std::time::Duration, body: &str) {
        self.state
            .script
            .lock()
            .unwrap()
            .push_back(Scripted::DelayedSse(delay, body.to_string()));
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
        Some(Scripted::DelayedJson(delay, status, v)) => {
            tokio::time::sleep(delay).await;
            (
                axum::http::StatusCode::from_u16(status).unwrap(),
                axum::Json(v),
            )
                .into_response()
        }
        Some(Scripted::DelayedSse(delay, body)) => {
            tokio::time::sleep(delay).await;
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                body,
            )
                .into_response()
        }
        None => axum::Json(openai_completion("OK")).into_response(),
    }
}

/// 默认 mock 补全:usage 5 输入(含 2 缓存)+ 3 输出。
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
        "usage": {
            "prompt_tokens": 5,
            "completion_tokens": 3,
            "total_tokens": 8,
            // 5 个输入 token 中 2 个命中缓存:记账按 (3 未缓存, 3 输出, 2 缓存) 拆分
            "prompt_tokens_details": {"cached_tokens": 2}
        }
    })
}

// ---------- 被测应用 ----------

pub struct TestApp {
    pub base: String,
    port: u16,
    child: Child,
    dir: tempfile::TempDir,
    extra_env: Vec<(String, String)>,
    /// 缓存的 admin JWT;restart 后启动期会重置 admin 密码(token_version+1),
    /// 缓存随之作废。
    admin_token: tokio::sync::Mutex<Option<String>>,
}

fn spawn_child(dir: &std::path::Path, port: u16, extra_env: &[(String, String)]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_aikun"))
        .envs(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir(dir)
        .env("AIKUN_HOST", format!("127.0.0.1:{}", port))
        .env("AIKUN_JWT_SECRET", "e2e-test-secret-not-for-production")
        // 阶段 S:被测二进制读嵌入式 SurrealKV(临时目录,随 TempDir 清理)
        .env(
            "AIKUN_SURREAL_URL",
            format!("surrealkv://{}/test.surreal", dir.display()),
        )
        // 检查通道:已知密码重置 admin,登录 /api/auth/login 拿 JWT
        .env("AIKUN_RESET_ADMIN_PASSWORD", E2E_ADMIN_PASSWORD)
        .env("AIKUN_HEALTH_CHECK_INTERVAL", "3600")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn aikun binary")
}

/// 探测服务就绪;子进程提前退出(端口被抢注 → bind panic 秒退)立即返回 false,
/// 供调用方换端口重拉。60s 上限覆盖冷启动竞争。
async fn probe_ready(child: &mut Child, base: &str) -> bool {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            return false;
        }
        if let Ok(r) = client.get(format!("{}/api/health", base)).send().await
            && r.status().is_success()
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 测试用户夹具:经管理面 API 创建 + 登录 + 建 API key(明文仅此一次)。
pub struct TestUser {
    pub id: String,
    pub username: String,
    pub password: String,
    /// 登录签发的 JWT(User/Admin tag 端点用)。
    pub token: String,
    /// /v1 代理面的 Bearer 明文。
    pub api_key: String,
    pub api_key_id: String,
}

/// API key 三项限额(0 = 不限;建 key 时一次性写入)。
#[derive(Default, Clone, Copy)]
pub struct KeyLimits {
    pub rpm: i64,
    pub quota_daily_tokens: i64,
    pub max_concurrent: i64,
}

/// 渠道种子(经 POST /api/providers;health_status 起始恒 unknown,
/// 选路仅排除 unhealthy,unknown 可用)。
pub struct ProviderSeed<'a> {
    pub name: &'a str,
    /// "openai" | "anthropic" | "responses"(responses 自动伴随 openai)。
    pub protocol: &'a str,
    pub base_url: &'a str,
    pub models: &'a [&'a str],
    pub priority: i64,
    pub weight: f64,
    pub api_key: &'a str,
    pub model_mapping: Option<Value>,
}

impl Default for ProviderSeed<'_> {
    fn default() -> Self {
        Self {
            name: "p",
            protocol: "openai",
            base_url: "http://127.0.0.1:1",
            models: &["gpt-4"],
            priority: 10,
            weight: 10.0,
            // 空 key 不参与查重(多渠道种子可并存);需要断言上游凭证的
            // 用例显式传 api_key(管理面创建即加密,转发时解密回明文)。
            api_key: "",
            model_mapping: None,
        }
    }
}

impl TestApp {
    /// 拉起真实 aikun 二进制:独立临时目录作为 cwd(避免读到仓库的
    /// .env)与嵌入式 SurrealKV 数据目录、随机端口、超长健康检查间隔
    /// (不干扰断言);就绪后经管理面 API 清空默认价格保证用例隔离。
    pub async fn spawn() -> Self {
        Self::spawn_with_env(&[]).await
    }

    /// 同 spawn,额外注入环境变量(如 AIKUN_CACHE_MODE)。
    pub async fn spawn_with_env(extra: &[(&str, &str)]) -> Self {
        let extra_env: Vec<(String, String)> = extra
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let dir = tempfile::tempdir().unwrap();
        // 端口竞态根治:free_port() 释放端口后、子进程 bind 前的窗口里,端口可能
        // 被并行测试抢注,子进程 bind 失败秒退。表现为偶发 readiness 超时
        // (全量套件并行时可复现)。对策:探测到子进程早退或超时,换端口
        // 重拉,至多 3 次——正常路径零额外开销。
        let mut port = free_port();
        let mut child = spawn_child(dir.path(), port, &extra_env);
        let mut ready = false;
        for attempt in 0..3 {
            if probe_ready(&mut child, &format!("http://127.0.0.1:{}", port)).await {
                ready = true;
                break;
            }
            assert!(
                attempt < 2,
                "aikun did not become ready after 3 spawn attempts (port race or startup failure)"
            );
            let _ = child.kill();
            let _ = child.wait();
            port = free_port();
            child = spawn_child(dir.path(), port, &extra_env);
        }
        assert!(ready);
        let mut app = Self {
            base: format!("http://127.0.0.1:{}", port),
            port,
            child,
            dir,
            extra_env,
            admin_token: tokio::sync::Mutex::new(None),
        };
        app.wait_ready().await;
        // 全新库启动时会 seed 内置默认价格;各用例自行维护价格数据,
        // 这里经管理面 API 清空以保证隔离(旧直连 DELETE 的等价通道)。
        app.clear_prices().await;
        app
    }

    /// 杀掉进程并在同一目录/数据目录/端口上重新拉起——用于验证重启语义
    /// (如默认价格重播种、渠道密钥跨重启可用)。
    pub async fn restart(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.child = spawn_child(self.dir.path(), self.port, &self.extra_env);
        // 启动期 AIKUN_RESET_ADMIN_PASSWORD 再次重置(token_version+1),
        // 旧 admin JWT 全部失效:丢弃缓存,下次按需重新登录。
        *self.admin_token.lock().await = None;
        self.wait_ready().await;
    }

    async fn wait_ready(&mut self) {
        assert!(
            probe_ready(&mut self.child, &self.base).await,
            "aikun did not become ready within 60s"
        );
    }

    pub fn client(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    // ---------- 检查通道:管理面 API ----------

    /// 登录任意用户拿 JWT(断言 200)。
    pub async fn login(&self, username: &str, password: &str) -> String {
        let resp = self
            .client()
            .post(format!("{}/api/auth/login", self.base))
            .json(&json!({"username": username, "password": password}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "login({username}) 必须成功: {:?}",
            resp.text().await
        );
        resp.json::<Value>().await.unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// admin JWT(缓存;restart 后自动重登)。
    pub async fn admin_token(&self) -> String {
        let mut cached = self.admin_token.lock().await;
        if let Some(t) = cached.as_ref() {
            return t.clone();
        }
        let t = self.login(ADMIN_USERNAME, E2E_ADMIN_PASSWORD).await;
        *cached = Some(t.clone());
        t
    }

    /// 管理面 GET → JSON(断言 200)。
    pub async fn admin_get(&self, path: &str) -> Value {
        let resp = self
            .client()
            .get(format!("{}{}", self.base, path))
            .bearer_auth(self.admin_token().await)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "GET {path}: {:?}", resp.text().await);
        resp.json().await.unwrap()
    }

    async fn clear_prices(&self) {
        let token = self.admin_token().await;
        loop {
            let page: Value = self
                .client()
                .get(format!("{}/api/prices?limit=200&offset=0", self.base))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let items = page["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                return;
            }
            for id in items
                .iter()
                .map(|p| p["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
            {
                let resp = self
                    .client()
                    .delete(format!("{}/api/prices/{}", self.base, id))
                    .bearer_auth(&token)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(resp.status(), 204);
            }
        }
    }

    // ---------- 种子夹具(全部经管理面 API)----------

    /// 创建用户(admin API),返回 id。
    pub async fn create_user(&self, username: &str, password: &str, role: &str) -> String {
        let resp = self
            .client()
            .post(format!("{}/api/users", self.base))
            .bearer_auth(self.admin_token().await)
            .json(&json!({
                "username": username,
                "password": password,
                "display_name": username,
                "role": role,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            201,
            "创建用户 {username} 失败: {:?}",
            resp.text().await
        );
        resp.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// 创建用户 + 登录 + 建 API key(限额可选)。
    pub async fn create_user_with_key(
        &self,
        username: &str,
        role: &str,
        limits: KeyLimits,
    ) -> TestUser {
        let password = format!("{username}-password-1");
        let id = self.create_user(username, &password, role).await;
        let token = self.login(username, &password).await;
        let resp = self
            .client()
            .post(format!("{}/api/api-keys", self.base))
            .bearer_auth(&token)
            .json(&json!({
                "name": username,
                "rate_limit_rpm": limits.rpm,
                "quota_daily_tokens": limits.quota_daily_tokens,
                "max_concurrent": limits.max_concurrent,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            201,
            "为 {username} 建 API key 失败: {:?}",
            resp.text().await
        );
        let body: Value = resp.json().await.unwrap();
        TestUser {
            api_key_id: body["id"].as_str().unwrap().to_string(),
            api_key: body["key"].as_str().unwrap().to_string(),
            id,
            username: username.to_string(),
            password,
            token,
        }
    }

    /// 默认测试用户:username=e2e、role=admin、无限额、余额 0。
    /// (旧 SQLite 直连夹具 seed_api_key 的 API 等价;调账流水计数
    /// 的用例用本形态保持纯净,余额按需 adjust。)
    pub async fn seed_user(&self) -> TestUser {
        self.create_user_with_key("e2e", "admin", KeyLimits::default())
            .await
    }

    /// 同 seed_user,但预充 1 元(/v1 余额预检要求 balance > 0;
    /// 与旧 SQLite 种子行带 1_000_000 微元对齐)。会多一条充值流水。
    pub async fn seed_user_funded(&self) -> TestUser {
        let user = self.seed_user().await;
        assert_eq!(self.adjust_balance(&user.id, 1.0).await, 1.0);
        user
    }

    /// 同 seed_user,但设置 rate_limit_rpm / quota_daily_tokens(0 = 不限)。
    pub async fn seed_user_with_limits(&self, rpm: i64, daily_quota: i64) -> TestUser {
        self.create_user_with_key(
            "e2e",
            "admin",
            KeyLimits {
                rpm,
                quota_daily_tokens: daily_quota,
                max_concurrent: 0,
            },
        )
        .await
    }

    /// 调账(元;正=充值 recharge,负=扣减 adjust),返回新余额(元)。
    pub async fn adjust_balance(&self, user_id: &str, amount_yuan: f64) -> f64 {
        let resp = self
            .client()
            .post(format!(
                "{}/api/users/{user_id}/balance-transactions",
                self.base
            ))
            .bearer_auth(self.admin_token().await)
            .json(&json!({"amount": amount_yuan}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            201,
            "调账 {user_id} +{amount_yuan} 失败: {:?}",
            resp.text().await
        );
        resp.json::<Value>().await.unwrap()["balance"]
            .as_f64()
            .unwrap()
    }

    /// 用户当前余额(元;GET /api/users/{id})。
    pub async fn user_balance(&self, user_id: &str) -> f64 {
        self.admin_get(&format!("/api/users/{user_id}")).await["balance"]
            .as_f64()
            .unwrap()
    }

    /// 建渠道(POST /api/providers),返回 id。
    pub async fn seed_provider(&self, p: &ProviderSeed<'_>) -> String {
        let protocols: Vec<&str> = match p.protocol {
            "anthropic" => vec!["anthropic"],
            // responses 不能单独成渠道(跨字段约束),伴随 openai;
            // /v1/responses 请求仍优先走原生 responses(Upstream protocol negotiation)
            "responses" => vec!["responses", "openai"],
            _ => vec!["openai"],
        };
        let mut body = json!({
            "name": p.name,
            "api_key": p.api_key,
            "models": p.models,
            "protocols": protocols,
            "priority": p.priority,
            "weight": p.weight,
        });
        if p.protocol == "anthropic" {
            body["anthropic_base_url"] = json!(p.base_url);
        } else {
            body["openai_base_url"] = json!(p.base_url);
        }
        if let Some(m) = &p.model_mapping {
            body["model_mapping"] = m.clone();
        }
        let resp = self
            .client()
            .post(format!("{}/api/providers", self.base))
            .bearer_auth(self.admin_token().await)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            201,
            "建渠道 {} 失败: {:?}",
            p.name,
            resp.text().await
        );
        resp.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// 建价格行(元/1M tokens),返回 id。
    pub async fn seed_price(
        &self,
        model: &str,
        prompt_price: f64,
        completion_price: f64,
        cached_price: Option<f64>,
    ) -> String {
        let mut body = json!({
            "model": model,
            "prompt_price": prompt_price,
            "completion_price": completion_price,
        });
        if let Some(c) = cached_price {
            body["cached_price"] = json!(c);
        }
        let resp = self
            .client()
            .post(format!("{}/api/prices", self.base))
            .bearer_auth(self.admin_token().await)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            201,
            "建价格 {model} 失败: {:?}",
            resp.text().await
        );
        resp.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    // ---------- 断言夹具(管理面 API 回读)----------

    /// 某用户的日志列表(admin 视角,可跨用户),按时间倒序。
    pub async fn logs(&self, user_id: &str) -> Vec<Value> {
        self.admin_get(&format!("/api/logs?user_id={user_id}&limit=200"))
            .await["items"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// 某用户的日志统计(total_tokens / total_cost 元 / success_rate)。
    pub async fn log_stats(&self, user_id: &str) -> Value {
        self.admin_get(&format!("/api/logs/stats?user_id={user_id}"))
            .await
    }

    /// 渠道当前状态(health_status / is_active / error_rate / …)。
    pub async fn provider(&self, id: &str) -> Value {
        self.admin_get(&format!("/api/providers/{id}")).await
    }

    /// 轮询管理面 GET 直到谓词命中,返回该次响应体。记账失败行为是
    /// 异步 fire-and-forget:断言前轮询等待条件成立(谓词是同步闭包,
    /// 捕获只发生在闭包调用内,不跨 await 逃逸)。
    pub async fn wait_admin(
        &self,
        path: &str,
        timeout: Duration,
        mut pred: impl FnMut(&Value) -> bool,
    ) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let v = self.admin_get(path).await;
            if pred(&v) {
                return v;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for condition on: {path}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------- JWT 直签(用户行存在且 token_version=0 时可用) ----------

/// 用测试密钥签发任意身份的 JWT(管理面创建的用户 token_version 起始为 0;
/// 改密/重置后会失效,由各用例自行规避)。
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

/// 记账失败行为异步 fire-and-forget:断言前轮询等待条件成立。
/// 轮询统一走 [`TestApp::wait_admin`](管理面 GET + 同步谓词);该自由函数
/// 是等用户首条成功日志的常用形态,返回命中的那一行。
pub async fn wait_success_log(app: &TestApp, user_id: &str, timeout: Duration) -> Value {
    let page = app
        .wait_admin(
            &format!("/api/logs?user_id={user_id}&limit=200"),
            timeout,
            |page| {
                page["items"].as_array().is_some_and(|items| {
                    items
                        .iter()
                        .any(|l| l["success"] == serde_json::json!(true))
                })
            },
        )
        .await;
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["success"] == serde_json::json!(true))
        .cloned()
        .unwrap()
}
