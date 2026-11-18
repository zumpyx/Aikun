//! ZCode 免费额度领取端口实现(reqwest;装配层注入 admin 服务)。
//!
//! 端点跟**渠道自身的 base** 走(国内渠道打 zcode.bigmodel.cn,国际打
//! zcode.z.ai——与凭证环境一致);序列见 ZcodeClaimPort 文档与
//! aikun_channels zcode 常量。
pub struct ZcodeClaimImpl;

impl ZcodeClaimImpl {
    /// 端点前缀归一:剥尾斜杠与 /api/v1 后缀(渠道 base 是纯 origin,
    /// AIKUN_ZCODE_OAUTH_BASE 默认带 /api/v1——两种形态都归一到 origin)。
    fn origin_of(base: &str) -> String {
        let mut b = base.trim_end_matches('/').to_string();
        if let Some(stripped) = b.strip_suffix("/api/v1") {
            b = stripped.to_string();
        }
        b
    }

    fn url_for(base: &str, path: &str) -> String {
        format!("{}{path}", Self::origin_of(base))
    }

    /// billing 家族通用请求:billing_request_headers + JSON 体,20s 超时。
    /// 返回 (status, 解析后的 body);网络失败 Err。
    async fn request_json(
        &self,
        method: reqwest::Method,
        base: &str,
        path: &str,
        jwt: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(u16, serde_json::Value), String> {
        use aikun_channels::adapters::zcode::{billing_request_headers, device_mid_for};
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;
        let mid = device_mid_for(jwt);
        let rid = uuid::Uuid::new_v4().to_string();
        let url = Self::url_for(base, path);
        let mut req = client.request(method, &url);
        for (k, v) in billing_request_headers(jwt, &mid, &rid) {
            req = req.header(k, v.as_str());
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("请求失败: {}", e.without_url()))?;
        let status = resp.status().as_u16();
        let parsed: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        Ok((status, parsed))
    }
}

/// 激活事件 16 字段体(炼化自 Zcode2Api BuildActivationEvent)。
pub(crate) fn zcode_activation_event(element: &str, user_id: &str, mid: &str) -> serde_json::Value {
    use aikun_channels::adapters::zcode::{BILLING_CLIENT_VERSION, BILLING_OS_VERSION};
    serde_json::json!({
        "event_id": uuid::Uuid::new_v4().to_string(),
        "client_timezone": "Asia/Shanghai",
        "client_language": "en-US",
        "element_name": element,
        "event_region": "app",
        "event_type": "view",
        "event_text": "",
        "event_extra_detail": {},
        "user_id": user_id,
        "screen_resolution": "1920x1080",
        "app_version": BILLING_CLIENT_VERSION,
        "device_os_category": "macos",
        "device_os_version": BILLING_OS_VERSION,
        "device_mid": mid,
        "mac_id": "",
        "marketing_params": "{}",
    })
}

#[async_trait::async_trait]
impl aikun_admin::admin_providers::ZcodeClaimPort for ZcodeClaimImpl {
    async fn report_activation(&self, base: &str, jwt: &str) -> Result<(), String> {
        use aikun_channels::adapters::zcode::{EVENT_REPORT_PATH, device_mid_for, jwt_sub};
        // JWT 无 sub 时上游无法归因,Zcode2Api 直接跳过事件——同口径,
        // 不算问题(领取本体不受影响)。
        let Some(user_id) = jwt_sub(jwt) else {
            return Ok(());
        };
        let mid = device_mid_for(jwt);
        for element in ["app_launch", "app_daily_active"] {
            let body = zcode_activation_event(element, &user_id, &mid);
            let (status, resp) = self
                .request_json(
                    reqwest::Method::POST,
                    base,
                    EVENT_REPORT_PATH,
                    jwt,
                    Some(&body),
                )
                .await?;
            if status >= 400 {
                return Err(format!("event {element} 上游返回 {status}"));
            }
            if resp["code"].as_i64().unwrap_or(0) != 0 {
                return Err(format!(
                    "event {element} code {} {}",
                    resp["code"].as_i64().unwrap_or(-1),
                    resp["msg"].as_str().unwrap_or_default()
                ));
            }
        }
        Ok(())
    }

    async fn offered_plans(
        &self,
        base: &str,
        jwt: &str,
    ) -> Result<Vec<aikun_admin::admin_providers::ZcodeOfferedPlan>, String> {
        use aikun_admin::admin_providers::ZcodeOfferedPlan;
        use aikun_channels::adapters::zcode::{PREVIEW_PATH, TOUCH_PATH};
        let mut offers: Vec<ZcodeOfferedPlan> = Vec::new();
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut preview_err: Option<String> = None;

        // billing/preview:plans 按 priority 降序(稳定),高优先级先领。
        match self
            .request_json(reqwest::Method::GET, base, PREVIEW_PATH, jwt, None)
            .await
        {
            Ok((200, body)) if body["code"].as_i64() == Some(0) => {
                let mut plans: Vec<(i64, String, String)> = body["data"]["plans"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|p| {
                                let id = p["plan_id"].as_str()?.trim().to_string();
                                if id.is_empty() {
                                    return None;
                                }
                                Some((
                                    p["priority"].as_i64().unwrap_or(0),
                                    id,
                                    p["name"].as_str().unwrap_or_default().to_string(),
                                ))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                plans.sort_by_key(|(priority, _, _)| std::cmp::Reverse(*priority));
                for (_, id, name) in plans {
                    seen.insert(id.to_lowercase(), offers.len());
                    offers.push(ZcodeOfferedPlan {
                        plan_id: id,
                        name,
                        campaign_id: String::new(),
                    });
                }
            }
            Ok((status, _)) => {
                preview_err = Some(format!("preview HTTP {status}"));
            }
            Err(e) => preview_err = Some(format!("preview: {e}")),
        }

        // marketing/touch:横幅按钮携带的 plan_id + campaign_id;preview 已
        // 有的补 campaign,新出现的追加在后(touch-only)。seq=0 = 游标起点。
        match self
            .request_json(
                reqwest::Method::GET,
                base,
                &format!("{TOUCH_PATH}?seq=0"),
                jwt,
                None,
            )
            .await
        {
            Ok((200, body)) if body["code"].as_i64() == Some(0) => {
                for d in body["data"]["deliveries"].as_array().unwrap_or(&vec![]) {
                    let campaign = d["campaign_id"].as_str().unwrap_or("").trim().to_string();
                    for b in d["banner"]["buttons"].as_array().unwrap_or(&vec![]) {
                        let pid = b["action"]["args"]["plan_id"]
                            .as_str()
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if pid.is_empty() {
                            continue;
                        }
                        match seen.get(&pid.to_lowercase()) {
                            Some(i) => {
                                if offers[*i].campaign_id.is_empty() {
                                    offers[*i].campaign_id = campaign.clone();
                                }
                            }
                            None => {
                                seen.insert(pid.to_lowercase(), offers.len());
                                offers.push(ZcodeOfferedPlan {
                                    plan_id: pid,
                                    name: String::new(),
                                    campaign_id: campaign.clone(),
                                });
                            }
                        }
                    }
                }
            }
            // touch 拉不到且 preview 也没有 → 无从领取,整体报错。
            Ok((status, _)) if offers.is_empty() => {
                return Err(preview_err.unwrap_or_else(|| format!("touch HTTP {status}")));
            }
            Err(e) if offers.is_empty() => {
                return Err(preview_err.unwrap_or_else(|| format!("touch: {e}")));
            }
            // preview 已有清单,touch 失败只影响 campaign 标记,不阻断。
            Ok((status, _)) => {
                tracing::debug!("zcode marketing/touch HTTP {status},跳过投放合并");
            }
            Err(e) => {
                tracing::debug!("zcode marketing/touch 失败,跳过投放合并: {e}");
            }
        }
        Ok(offers)
    }

    async fn owned_plans(&self, base: &str, jwt: &str) -> Result<Vec<String>, String> {
        use aikun_channels::adapters::zcode::{BALANCE_PATH, BILLING_CLIENT_VERSION};
        let path = format!("{BALANCE_PATH}?app_version={BILLING_CLIENT_VERSION}");
        let (status, body) = self
            .request_json(reqwest::Method::GET, base, &path, jwt, None)
            .await?;
        if status != 200 {
            return Err(format!("balance HTTP {status}"));
        }
        if body["code"].as_i64() != Some(0) {
            return Err(format!(
                "balance code {}",
                body["code"].as_i64().unwrap_or(-1)
            ));
        }
        Ok(body["data"]["plans"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| {
                        let id = p["plan_id"].as_str()?.trim().to_lowercase();
                        if id.is_empty() { None } else { Some(id) }
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn claim_plan(&self, base: &str, jwt: &str, plan_id: &str) -> Result<bool, String> {
        use aikun_channels::adapters::zcode::CLAIM_PATH;
        let body = serde_json::json!({"plan_id": plan_id});
        let (status, resp) = self
            .request_json(reqwest::Method::POST, base, CLAIM_PATH, jwt, Some(&body))
            .await?;
        // 401/403 带错误体 code(与 Zcode2Api 同口径);其余非 200 直接报。
        if status != 200 && status != 401 && status != 403 {
            return Err(format!("claim HTTP {status}"));
        }
        match resp["code"].as_i64() {
            Some(0) => Ok(true),
            Some(1003) => Ok(false),
            Some(code) => Err(format!(
                "claim code {code} {}",
                resp["msg"].as_str().unwrap_or_default()
            )),
            None if status == 200 => Err("claim 响应无 code".to_string()),
            None => Err(format!("claim HTTP {status}")),
        }
    }

    async fn fetch_balance(&self, base: &str, jwt: &str) -> Result<serde_json::Value, String> {
        use aikun_channels::adapters::zcode::{BALANCE_PATH, BILLING_CLIENT_VERSION};
        let path = format!("{BALANCE_PATH}?app_version={BILLING_CLIENT_VERSION}");
        let (status, body) = self
            .request_json(reqwest::Method::GET, base, &path, jwt, None)
            .await?;
        if status != 200 {
            return Err(format!("balance HTTP {status}"));
        }
        if body["code"].as_i64() != Some(0) {
            return Err(format!(
                "balance code {}",
                body["code"].as_i64().unwrap_or(-1)
            ));
        }
        Ok(body["data"].clone())
    }

    async fn confirm_campaign(
        &self,
        base: &str,
        jwt: &str,
        campaign_id: &str,
    ) -> Result<(), String> {
        use aikun_channels::adapters::zcode::TOUCH_ACTION_PATH;
        let body = serde_json::json!({"campaign_id": campaign_id, "action_type": "confirm"});
        let (status, resp) = self
            .request_json(
                reqwest::Method::POST,
                base,
                TOUCH_ACTION_PATH,
                jwt,
                Some(&body),
            )
            .await?;
        if status != 200 {
            return Err(format!("touch confirm HTTP {status}"));
        }
        if resp["code"].as_i64() != Some(0) {
            return Err(format!(
                "touch confirm code {} {}",
                resp["code"].as_i64().unwrap_or(-1),
                resp["msg"].as_str().unwrap_or_default()
            ));
        }
        Ok(())
    }
}
