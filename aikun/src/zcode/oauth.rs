//! ZCode OAuth 在线登录端口实现(reqwest;装配层注入 admin 服务)。
//!
// 流程移植自 Zcode2Api internal/gateway/login.go:init(随机 poll_token
// 作 Bearer,body {"provider":"zai"|"bigmodel"})→ 浏览器 authorize → poll 轮询。
// captcha:origin 已 skip(2026-10-08 起 chat 请求不再要求),此处不带。

pub struct ZcodeOAuthImpl {
    pub base: String,
}

#[async_trait::async_trait]
impl aikun_admin::admin_providers::ZcodeOAuth for ZcodeOAuthImpl {
    async fn init(
        &self,
        env: &str,
    ) -> Result<(aikun_admin::admin_providers::ZcodeOAuthInit, String), String> {
        // env → 账号体系 provider(2026-11-19 实测:bigmodel 走智谱授权页,
        // zai 走 z.ai 授权页;同一 init 端点,无效值上游回 code 1000)。
        let provider = if env == "cn" { "bigmodel" } else { "zai" };
        let poll_token: String = {
            use rand::RngCore;
            let mut buf = [0u8; 32];
            rand::rng().fill_bytes(&mut buf);
            buf.iter().map(|b| format!("{b:02x}")).collect()
        };
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;
        let resp = client
            .post(format!("{}/oauth/cli/init", self.base))
            .bearer_auth(&poll_token)
            .json(&serde_json::json!({"provider": provider}))
            .send()
            .await
            .map_err(|e| format!("oauth init 请求失败: {}", e.without_url()))?;
        if !resp.status().is_success() {
            return Err(format!("oauth init 上游返回 {}", resp.status()));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("oauth init 响应解析失败: {e}"))?;
        let data = &body["data"];
        let flow_id = data["flow_id"].as_str().unwrap_or_default().to_string();
        let authorize_url = data["authorize_url"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if flow_id.is_empty() || authorize_url.is_empty() {
            return Err("oauth init 响应缺少 flow_id / authorize_url".to_string());
        }
        Ok((
            aikun_admin::admin_providers::ZcodeOAuthInit {
                flow_id,
                authorize_url,
            },
            poll_token,
        ))
    }

    async fn poll(
        &self,
        flow_id: &str,
        poll_token: &str,
    ) -> aikun_admin::admin_providers::ZcodePollStatus {
        use aikun_admin::admin_providers::ZcodePollStatus;
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
        {
            Ok(c) => c,
            Err(_) => return ZcodePollStatus::Pending,
        };
        // poll_token 作 Bearer(与 init 同凭证链);网络错误按 Pending。
        let resp = match client
            .get(format!("{}/oauth/cli/poll/{flow_id}", self.base))
            .bearer_auth(poll_token)
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => return ZcodePollStatus::Pending,
        };
        if !resp.status().is_success() {
            return ZcodePollStatus::Pending;
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => return ZcodePollStatus::Pending,
        };
        let data = &body["data"];
        match data["status"].as_str().unwrap_or("pending") {
            "ready" => ZcodePollStatus::Ready {
                token: data["token"].as_str().unwrap_or_default().to_string(),
                // 国内(智谱)账号多为手机号登录,user.email 常是空串而非
                // null(2026-11-19 实测用户踩坑:name 空 → 400):email →
                // phone → name/nickname 依次取第一个非空,兜底 zcode-user。
                email: ["email", "phone", "name", "nickname"]
                    .iter()
                    .find_map(|k| data["user"][k].as_str().filter(|v| !v.trim().is_empty()))
                    .unwrap_or("zcode-user")
                    .to_string(),
                // 家族业务 token(monitor 域订阅/5h·周额度查询用;国内
                // bigmodel / 国际 zai 各自的 access_token,命名两种口径都收)。
                business_token: ["bigmodel", "zai"].iter().find_map(|family| {
                    let f = &data[family];
                    ["access_token", "accessToken"].iter().find_map(|k| {
                        f[*k]
                            .as_str()
                            .filter(|v| !v.trim().is_empty())
                            .map(str::to_string)
                    })
                }),
            },
            "expired" => ZcodePollStatus::Expired,
            "failed" => {
                ZcodePollStatus::Failed(data["error"].as_str().unwrap_or("授权失败").to_string())
            }
            _ => ZcodePollStatus::Pending,
        }
    }
}
