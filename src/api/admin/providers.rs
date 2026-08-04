use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use rusqlite::params;
use serde_json::json;
use uuid::Uuid;

use crate::AppState;
use crate::models::{
    channel_protocol, channel_protocols, first_defaultable_protocol, has_defaultable_protocol,
    row_to_provider, valid_default_protocol, valid_protocol_list, CreateProviderRequest,
    ProviderResponse, UpdateProviderRequest,
};
use crate::router::health::check_provider_health;

pub async fn list_providers(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let conn = match state.pool.read().lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let mut stmt = match conn.prepare(
        "SELECT id, name, provider_type, openai_base_url, anthropic_base_url, api_key, models, priority, weight,
                is_active, health_status, latency_ms, error_rate, last_health_check,
                max_retries, timeout_secs, created_at, updated_at, proxy_url,
                model_mapping, consecutive_failures, disabled_reason,
                protocols, default_protocol, note, website_url
         FROM providers ORDER BY priority DESC, name ASC"
    ) {
        Ok(s) => s,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "query_failed"}))),
    };

    let providers: Vec<ProviderResponse> = match stmt.query_map([], row_to_provider) {
            Ok(rows) => rows
                .filter_map(|r| r.ok())
                .map(|p| {
                    let mut resp = ProviderResponse::from(p);
                    // proxy_url 可能内嵌凭证，列表响应一律脱敏回显
                    resp.proxy_url = crate::proxy::client::sanitize_proxy_url(&resp.proxy_url);
                    resp
                })
                .collect(),
            // 查询失败要报 500 而不是返回空列表——否则 DB 故障在前端
            // 表现为"没有任何渠道",掩盖真实故障。
            Err(e) => {
                tracing::error!("Failed to list providers: {}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "query_failed"})));
            }
        };

    (StatusCode::OK, Json(json!(providers)))
}

/// Provider timeout must stay within sane bounds: 0 means "no timeout" and
/// would let a stuck upstream wedge health probes and streams forever;
/// negative values wrap to astronomical u64 values downstream.
pub fn valid_timeout_secs(t: i32) -> bool {
    (1..=600).contains(&t)
}

/// provider_type 白名单，与 providers 表 CHECK 约束一致（非法直接 400，
/// 而不是撞库约束返回 500）。
pub fn valid_provider_type(t: &str) -> bool {
    // 不接受 "azure":Azure OpenAI 需要 api-key 头而非 Bearer,当前
    // apply_auth 不支持,放开会创建出必然 401 的渠道。DB 的 CHECK
    // 约束仍容忍历史 azure 行。
    matches!(t, "openai" | "anthropic" | "custom")
}

/// weight 必须有限且在 0.1..=1000：inf/NaN 会持久化并让选路
/// random_range panic。
pub fn valid_weight(w: f64) -> bool {
    w.is_finite() && (0.1..=1000.0).contains(&w)
}

pub fn valid_priority(p: i32) -> bool {
    (0..=100).contains(&p)
}

pub fn valid_max_retries(r: i32) -> bool {
    (0..=10).contains(&r)
}

pub async fn create_provider(
    State(state): State<AppState>,
    Json(req): Json<CreateProviderRequest>,
) -> impl IntoResponse {
    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let id = Uuid::new_v4().to_string();
    let models_json = serde_json::to_string(&req.models).unwrap_or_else(|_| "[]".to_string());
    // Protocols the channel can speak; default to openai-only when omitted.
    let protocols = match req.protocols {
        Some(list) => {
            if !valid_protocol_list(&list) {
                return (StatusCode::BAD_REQUEST, Json(json!({
                    "error": "invalid_protocols",
                    "message": "protocols 不能为空，且仅支持 openai / anthropic / responses"
                })));
            }
            if !has_defaultable_protocol(&list) {
                return (StatusCode::BAD_REQUEST, Json(json!({
                    "error": "invalid_protocols",
                    "message": "responses 为附加协议，需同时勾选 openai 或 anthropic"
                })));
            }
            list
        }
        None => vec!["openai".to_string()],
    };
    let default_protocol = match req.default_protocol {
        Some(d) if valid_default_protocol(&d, &protocols) => d,
        Some(_) => {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "error": "invalid_default_protocol",
                "message": "default_protocol 必须是已勾选的 openai / anthropic 协议之一"
            })));
        }
        None => first_defaultable_protocol(&protocols),
    };
    let protocols_json = serde_json::to_string(&protocols)
        .unwrap_or_else(|_| "[\"openai\"]".to_string());
    // Keep the legacy provider_type column in sync with the default protocol
    // so old fallback paths stay consistent.
    let provider_type = req.provider_type.unwrap_or_else(|| default_protocol.clone());
    let priority = req.priority.unwrap_or(0);
    let weight = req.weight.unwrap_or(1.0);
    let max_retries = req.max_retries.unwrap_or(3);
    let timeout_secs = req.timeout_secs.unwrap_or(120);

    let name = req.name.trim().to_string();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "invalid_name",
            "message": "name 不能为空"
        })));
    }
    // 两种协议的上游地址独立配置;至少填一个,留空的一路回退到另一个。
    let openai_base_url = req
        .openai_base_url
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .to_string();
    let anthropic_base_url = req
        .anthropic_base_url
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .to_string();
    // 勾选了哪个协议就必须填对应的上游地址;responses 复用 openai 地址。
    if protocols.iter().any(|p| p == "openai" || p == "responses") && openai_base_url.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "invalid_base_url",
            "message": "勾选 openai / responses 协议时必须填写 openai_base_url"
        })));
    }
    if protocols.iter().any(|p| p == "anthropic") && anthropic_base_url.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "invalid_base_url",
            "message": "勾选 anthropic 协议时必须填写 anthropic_base_url"
        })));
    }
    if !valid_provider_type(&provider_type) {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "invalid_provider_type",
            "message": "provider_type 仅支持 openai / anthropic / custom"
        })));
    }
    if !valid_priority(priority) {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "invalid_priority",
            "message": "priority must be between 0 and 100"
        })));
    }
    if !valid_weight(weight) {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "invalid_weight",
            "message": "weight must be finite and between 0.1 and 1000"
        })));
    }
    if !valid_max_retries(max_retries) {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "invalid_max_retries",
            "message": "max_retries must be between 0 and 10"
        })));
    }
    if !valid_timeout_secs(timeout_secs) {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "invalid_timeout",
            "message": "timeout_secs must be between 1 and 600"
        })));
    }
    let proxy_url = req.proxy_url.unwrap_or_default();
    let model_mapping = serde_json::to_string(&req.model_mapping.unwrap_or_default())
        .unwrap_or_else(|_| "{}".to_string());
    let note = req.note.unwrap_or_default().trim().to_string();
    let website_url = req.website_url.unwrap_or_default().trim().to_string();
    // 静态加密落库(enc:v1: 前缀),明文只存在于本次请求内存中。
    let encrypted_key = state.pool.cipher.encrypt(&req.api_key);

    match conn.execute(
        "INSERT INTO providers (id, name, provider_type, openai_base_url, anthropic_base_url,
                                api_key, models, priority, weight,
                                max_retries, timeout_secs, proxy_url, model_mapping,
                                protocols, default_protocol, note, website_url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![id, name, provider_type, openai_base_url, anthropic_base_url,
                 encrypted_key, models_json,
                 priority, weight, max_retries, timeout_secs, proxy_url, model_mapping,
                 protocols_json, default_protocol, note, website_url],
    ) {
        Ok(_) => (StatusCode::CREATED, Json(json!(ProviderResponse {
            id,
            name,
            provider_type,
            openai_base_url,
            anthropic_base_url,
            models: req.models,
            priority,
            weight,
            is_active: true,
            health_status: "unknown".to_string(),
            latency_ms: 0.0,
            error_rate: 0.0,
            last_health_check: None,
            max_retries,
            timeout_secs,
            proxy_url: crate::proxy::client::sanitize_proxy_url(&proxy_url),
            model_mapping: serde_json::from_str(&model_mapping).unwrap_or_default(),
            disabled_reason: String::new(),
            protocols,
            default_protocol,
            note,
            website_url,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        }))),
        Err(e) => {
            tracing::error!("Failed to create provider: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                "error": "create_failed",
                "message": "Internal server error"
            })))
        }
    }
}

pub async fn get_provider(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
) -> impl IntoResponse {
    let conn = match state.pool.read().lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let provider = conn.query_row(
        "SELECT id, name, provider_type, openai_base_url, anthropic_base_url, api_key, models, priority, weight,
                is_active, health_status, latency_ms, error_rate, last_health_check,
                max_retries, timeout_secs, created_at, updated_at, proxy_url,
                model_mapping, consecutive_failures, disabled_reason,
                protocols, default_protocol, note, website_url
         FROM providers WHERE id = ?1",
        params![provider_id],
        row_to_provider,
    );

    match provider {
        Ok(p) => {
            let mut resp = ProviderResponse::from(p);
            // proxy_url 可能内嵌凭证,详情响应同样脱敏
            resp.proxy_url = crate::proxy::client::sanitize_proxy_url(&resp.proxy_url);
            (StatusCode::OK, Json(json!(resp)))
        }
        Err(_) => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
    }
}

pub async fn update_provider(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
    Json(req): Json<UpdateProviderRequest>,
) -> impl IntoResponse {
    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let mut updates = Vec::new();
    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(name) = &req.name {
        let name = name.trim();
        if name.is_empty() {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "error": "invalid_name",
                "message": "name 不能为空"
            })));
        }
        updates.push("name = ?");
        params_vec.push(Box::new(name.to_string()));
    }
    if let Some(p_type) = &req.provider_type {
        if !valid_provider_type(p_type) {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "error": "invalid_provider_type",
                "message": "provider_type 仅支持 openai / anthropic / custom"
            })));
        }
        updates.push("provider_type = ?");
        params_vec.push(Box::new(p_type.clone()));
    }
    // 两个地址都允许更新;置空即回退到另一路(见 models::base_url_for)。
    if let Some(url) = &req.openai_base_url {
        let url = url.trim().trim_end_matches('/').to_string();
        updates.push("openai_base_url = ?");
        params_vec.push(Box::new(url));
    }
    if let Some(url) = &req.anthropic_base_url {
        let url = url.trim().trim_end_matches('/').to_string();
        updates.push("anthropic_base_url = ?");
        params_vec.push(Box::new(url));
    }
    if let Some(note) = &req.note {
        updates.push("note = ?");
        params_vec.push(Box::new(note.trim().to_string()));
    }
    if let Some(website_url) = &req.website_url {
        updates.push("website_url = ?");
        params_vec.push(Box::new(website_url.trim().to_string()));
    }
    if let Some(key) = &req.api_key {
        updates.push("api_key = ?");
        // 静态加密落库(enc:v1: 前缀)。
        params_vec.push(Box::new(state.pool.cipher.encrypt(key)));
    }
    if let Some(models) = &req.models {
        if let Ok(json_str) = serde_json::to_string(models) {
            updates.push("models = ?");
            params_vec.push(Box::new(json_str));
        }
    }
    if let Some(priority) = req.priority {
        if !valid_priority(priority) {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "error": "invalid_priority",
                "message": "priority must be between 0 and 100"
            })));
        }
        updates.push("priority = ?");
        params_vec.push(Box::new(priority));
    }
    if let Some(weight) = req.weight {
        if !valid_weight(weight) {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "error": "invalid_weight",
                "message": "weight must be finite and between 0.1 and 1000"
            })));
        }
        updates.push("weight = ?");
        params_vec.push(Box::new(weight));
    }
    if let Some(active) = req.is_active {
        updates.push("is_active = ?");
        params_vec.push(Box::new(active as i32));
        if active {
            // Manual re-enable: clear auto-disable state.
            updates.push("disabled_reason = ''");
            updates.push("consecutive_failures = 0");
        }
    }
    if let Some(retries) = req.max_retries {
        if !valid_max_retries(retries) {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "error": "invalid_max_retries",
                "message": "max_retries must be between 0 and 10"
            })));
        }
        updates.push("max_retries = ?");
        params_vec.push(Box::new(retries));
    }
    if let Some(timeout) = req.timeout_secs {
        if !valid_timeout_secs(timeout) {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "error": "invalid_timeout",
                "message": "timeout_secs must be between 1 and 600"
            })));
        }
        updates.push("timeout_secs = ?");
        params_vec.push(Box::new(timeout));
    }
    if let Some(proxy) = &req.proxy_url {
        // 显式传入即更新,空串表示清除代理;字段缺失才不修改。
        updates.push("proxy_url = ?");
        params_vec.push(Box::new(proxy.trim().to_string()));
    }
    if let Some(mapping) = &req.model_mapping {
        updates.push("model_mapping = ?");
        params_vec.push(Box::new(
            serde_json::to_string(mapping).unwrap_or_else(|_| "{}".to_string()),
        ));
    }
    if req.protocols.is_some() || req.default_protocol.is_some() {
        // Resolve against the stored values: changing the protocol list may
        // invalidate the stored default, and vice versa.
        let stored = conn.query_row(
            "SELECT protocols, default_protocol FROM providers WHERE id = ?1",
            params![provider_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        );
        let (stored_protocols, stored_default) = match stored {
            Ok(s) => s,
            Err(_) => return (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
        };
        let protocols = match &req.protocols {
            Some(list) => {
                if !valid_protocol_list(list) {
                    return (StatusCode::BAD_REQUEST, Json(json!({
                        "error": "invalid_protocols",
                        "message": "protocols 不能为空，且仅支持 openai / anthropic / responses"
                    })));
                }
                if !has_defaultable_protocol(list) {
                    return (StatusCode::BAD_REQUEST, Json(json!({
                        "error": "invalid_protocols",
                        "message": "responses 为附加协议，需同时勾选 openai 或 anthropic"
                    })));
                }
                list.clone()
            }
            None => serde_json::from_str(&stored_protocols)
                .unwrap_or_else(|_| vec!["openai".to_string()]),
        };
        let default_protocol = match &req.default_protocol {
            Some(d) => {
                if !valid_default_protocol(d, &protocols) {
                    return (StatusCode::BAD_REQUEST, Json(json!({
                        "error": "invalid_default_protocol",
                        "message": "default_protocol 必须是已勾选的 openai / anthropic 协议之一"
                    })));
                }
                d.clone()
            }
            // Stored default fell out of the new list: reset to the first
            // defaultable one.
            None if !protocols.contains(&stored_default) => {
                first_defaultable_protocol(&protocols)
            }
            None => stored_default,
        };
        if req.protocols.is_some() {
            updates.push("protocols = ?");
            params_vec.push(Box::new(
                serde_json::to_string(&protocols).unwrap_or_else(|_| "[]".to_string()),
            ));
        }
        updates.push("default_protocol = ?");
        params_vec.push(Box::new(default_protocol));
    }

    // 与 create 同一约束:勾选的协议必须有对应上游地址。协议或地址任一
    // 被修改时,按更新后的最终状态校验,防止造出运行时必失败的渠道。
    if req.protocols.is_some() || req.openai_base_url.is_some() || req.anthropic_base_url.is_some() {
        let stored = conn.query_row(
            "SELECT protocols, openai_base_url, anthropic_base_url FROM providers WHERE id = ?1",
            params![provider_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        );
        let (stored_protocols, stored_openai, stored_anthropic) = match stored {
            Ok(s) => s,
            Err(_) => return (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
        };
        let final_protocols: Vec<String> = match &req.protocols {
            Some(list) => list.clone(),
            None => serde_json::from_str(&stored_protocols)
                .unwrap_or_else(|_| vec!["openai".to_string()]),
        };
        let final_openai = req
            .openai_base_url
            .as_deref()
            .unwrap_or(&stored_openai)
            .trim();
        let final_anthropic = req
            .anthropic_base_url
            .as_deref()
            .unwrap_or(&stored_anthropic)
            .trim();
        // responses 复用 openai 地址,勾选其一即要求 openai_base_url。
        if final_protocols.iter().any(|p| p == "openai" || p == "responses") && final_openai.is_empty() {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "error": "invalid_base_url",
                "message": "勾选 openai / responses 协议时必须填写 openai_base_url"
            })));
        }
        if final_protocols.iter().any(|p| p == "anthropic") && final_anthropic.is_empty() {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "error": "invalid_base_url",
                "message": "勾选 anthropic 协议时必须填写 anthropic_base_url"
            })));
        }
    }

    if updates.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "no_fields_to_update"})));
    }

    updates.push("updated_at = datetime('now')");
    let sql = format!(
        "UPDATE providers SET {} WHERE id = ?",
        updates.join(", ")
    );

    let mut params: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
    params.push(&provider_id);

    match conn.execute(&sql, params.as_slice()) {
        Ok(n) if n > 0 => (StatusCode::OK, Json(json!({"message": "updated"}))),
        Ok(_) => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
        Err(e) => {
            tracing::error!("Failed to update provider: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                "error": "update_failed",
                "message": "Internal server error"
            })))
        }
    }
}

pub async fn delete_provider(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
) -> impl IntoResponse {
    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    match conn.execute("DELETE FROM providers WHERE id = ?1", params![provider_id]) {
        Ok(n) if n > 0 => (StatusCode::OK, Json(json!({"message": "deleted"}))),
        Ok(_) => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
        Err(e) => {
            tracing::error!("Failed to delete provider: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                "error": "delete_failed",
                "message": "Internal server error"
            })))
        }
    }
}

/// Create a copy of an existing provider. Configuration (base URLs, api_key,
/// models, priority, weight, retries, timeout, proxy) is copied; runtime
/// state (health, latency, error rate) is reset. Useful for managing multiple
/// accounts of the same provider.
pub async fn duplicate_provider(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
) -> impl IntoResponse {
    let conn = match state.pool.conn.lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let source = conn.query_row(
        "SELECT name, provider_type, openai_base_url, anthropic_base_url, api_key, models,
                priority, weight, max_retries, timeout_secs, proxy_url, model_mapping,
                protocols, default_protocol, note, website_url
         FROM providers WHERE id = ?1",
        params![provider_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i32>(6)?,
                row.get::<_, f64>(7)?,
                row.get::<_, i32>(8)?,
                row.get::<_, i32>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, String>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, String>(13)?,
                row.get::<_, String>(14)?,
                row.get::<_, String>(15)?,
            ))
        },
    );

    let (name, provider_type, openai_base_url, anthropic_base_url, api_key, models,
         priority, weight, max_retries, timeout_secs, proxy_url, model_mapping,
         protocols, default_protocol, note, website_url) = match source {
        Ok(s) => s,
        Err(_) => return (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
    };

    let new_id = Uuid::new_v4().to_string();
    let new_name = format!("{} (副本)", name);

    match conn.execute(
        "INSERT INTO providers (id, name, provider_type, openai_base_url, anthropic_base_url,
                                api_key, models, priority, weight,
                                max_retries, timeout_secs, proxy_url, model_mapping,
                                protocols, default_protocol, note, website_url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![new_id, new_name, provider_type, openai_base_url, anthropic_base_url,
                api_key, models, priority, weight, max_retries, timeout_secs,
                proxy_url, model_mapping, protocols, default_protocol, note, website_url],
    ) {
        Ok(_) => {
            let models_parsed: Vec<String> =
                serde_json::from_str(&models).unwrap_or_default();
            (StatusCode::CREATED, Json(json!(ProviderResponse {
                id: new_id,
                name: new_name,
                provider_type,
                openai_base_url,
                anthropic_base_url,
                models: models_parsed,
                priority,
                weight,
                is_active: true,
                health_status: "unknown".to_string(),
                latency_ms: 0.0,
                error_rate: 0.0,
                last_health_check: None,
                max_retries,
                timeout_secs,
                proxy_url: crate::proxy::client::sanitize_proxy_url(&proxy_url),
                model_mapping: serde_json::from_str(&model_mapping).unwrap_or_default(),
                disabled_reason: String::new(),
                protocols: serde_json::from_str(&protocols).unwrap_or_default(),
                default_protocol,
                note,
                website_url,
                created_at: chrono::Utc::now().to_rfc3339(),
                updated_at: chrono::Utc::now().to_rfc3339(),
            })))
        }
        Err(e) => {
            tracing::error!("Failed to duplicate provider: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                "error": "duplicate_failed",
                "message": "Internal server error"
            })))
        }
    }
}

pub async fn test_provider(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
) -> impl IntoResponse {
    // Step 1: Read provider info from DB (synchronous, drop lock before await)
    let provider_info = {
        let conn = match state.pool.read().lock() {
            Ok(c) => c,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
        };

        match conn.query_row(
            "SELECT id, name, openai_base_url, anthropic_base_url, api_key,
                    COALESCE(NULLIF(default_protocol, ''), provider_type),
                    timeout_secs, proxy_url FROM providers WHERE id = ?1",
            params![provider_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i32>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        ) {
            Ok(info) => info,
            Err(_) => return (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
        }
    }; // MutexGuard dropped here

    let (id, name, openai_url, anthropic_url, api_key, protocol, timeout, proxy_url) = provider_info;
    // api_key 出库解密(enc:v1: 前缀密文,明文兼容)
    let api_key = crate::crypto::decrypt_or_plain(&state.pool.cipher, &api_key);

    // 按渠道的默认协议选对应的上游地址;留空的一路回退到另一个。
    let base_url = if protocol == "anthropic" {
        if anthropic_url.is_empty() { &openai_url } else { &anthropic_url }
    } else if openai_url.is_empty() {
        &anthropic_url
    } else {
        &openai_url
    };

    // Step 2: Make health check request (async, no lock held)
    let (status, latency) = check_provider_health(&state.clients, base_url, &api_key, &protocol, timeout.clamp(1, 600) as u64, &proxy_url).await;

    // Step 3: Re-lock DB to update status
    {
        let conn = match state.pool.conn.lock() {
            Ok(c) => c,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
        };

        let now = chrono::Utc::now().to_rfc3339();
        // 注意:不更新 latency_ms —— ping 延迟不代表真实请求延迟,
        // 覆盖会污染选路用的 EMA(与后台健康检查循环行为一致)。
        if let Err(e) = conn.execute(
            "UPDATE providers SET health_status = ?1, last_health_check = ?2 WHERE id = ?3",
            params![status, now, id],
        ) {
            tracing::error!("Failed to update provider health: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({
                "error": "update_failed",
                "message": "Internal server error"
            })));
        }
    } // MutexGuard dropped here

    (StatusCode::OK, Json(json!({
        "provider_id": id,
        "name": name,
        "status": status,
        "latency_ms": latency,
        "message": format!("Provider {} is {} ({}ms)", name, status, latency as i64)
    })))
}
#[cfg(test)]
mod tests {
    use super::{valid_max_retries, valid_priority, valid_provider_type, valid_timeout_secs, valid_weight};

    #[test]
    fn timeout_bounds() {
        assert!(!valid_timeout_secs(0)); // 0 = no timeout, rejected
        assert!(!valid_timeout_secs(-1)); // negative wraps downstream
        assert!(valid_timeout_secs(1));
        assert!(valid_timeout_secs(120));
        assert!(valid_timeout_secs(600));
        assert!(!valid_timeout_secs(601));
    }

    #[test]
    fn provider_type_whitelist() {
        assert!(valid_provider_type("openai"));
        assert!(valid_provider_type("anthropic"));
        assert!(!valid_provider_type("azure")); // apply_auth 不支持 api-key 头,禁用
        assert!(valid_provider_type("custom"));
        assert!(!valid_provider_type("gemini"));
        assert!(!valid_provider_type(""));
    }

    #[test]
    fn weight_bounds_and_finite() {
        assert!(valid_weight(1.0));
        assert!(valid_weight(0.1));
        assert!(valid_weight(1000.0));
        assert!(!valid_weight(0.09));
        assert!(!valid_weight(1000.1));
        assert!(!valid_weight(f64::INFINITY)); // 1e400 反序列化为 inf
        assert!(!valid_weight(f64::NAN));
    }

    #[test]
    fn priority_and_retries_bounds() {
        assert!(valid_priority(0));
        assert!(valid_priority(100));
        assert!(!valid_priority(-1));
        assert!(!valid_priority(101));
        assert!(valid_max_retries(0));
        assert!(valid_max_retries(10));
        assert!(!valid_max_retries(-1));
        assert!(!valid_max_retries(11));
    }
}

// ============================================================================
// Model utilities: fetch upstream model list, per-model live test
// ============================================================================

#[derive(Debug, serde::Deserialize)]
pub struct FetchModelsRequest {
    pub base_url: String,
    pub api_key: Option<String>,
    /// Wire protocol to authenticate with (openai/anthropic). Falls back to
    /// the legacy provider_type field for older clients.
    pub protocol: Option<String>,
    pub provider_type: Option<String>,
    pub proxy_url: Option<String>,
    /// Set when editing an existing channel: the form keeps the key empty
    /// ("unchanged"), so fall back to the stored key in that case.
    pub provider_id: Option<String>,
}

/// Fetch the available model list from an upstream for a (possibly
/// not-yet-saved) channel configuration. Powers the "获取模型列表" button
/// in the provider modal.
pub async fn fetch_upstream_models(
    State(state): State<AppState>,
    Json(req): Json<FetchModelsRequest>,
) -> impl IntoResponse {
    let base_url = req.base_url.trim().trim_end_matches('/').to_string();
    let mut api_key = req.api_key.unwrap_or_default();
    let protocol = req
        .protocol
        .or(req.provider_type)
        .unwrap_or_else(|| "openai".to_string());
    if !matches!(protocol.as_str(), "openai" | "anthropic") {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "invalid_protocol",
            "message": "protocol 仅支持 openai / anthropic"
        })));
    }
    let mut proxy_url = req.proxy_url.unwrap_or_default();

    // 编辑表单里 api_key / 代理都是"留空不修改":任一为空且有 provider_id
    // 时回退到库存值,否则带认证的代理在编辑态下获取模型列表必失败。
    if (api_key.is_empty() || proxy_url.is_empty()) && req.provider_id.is_some() {
        let pid = req.provider_id.as_deref().unwrap_or_default();
        let stored = {
            let conn = match state.pool.read().lock() {
                Ok(c) => c,
                Err(_) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"})))
                }
            };
            conn.query_row(
                "SELECT api_key, proxy_url FROM providers WHERE id = ?1",
                params![pid],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
        };
        match stored {
            Ok((k, p)) => {
                if api_key.is_empty() {
                    // 库存为密文(enc:v1: 前缀)时先解密
                    api_key = crate::crypto::decrypt_or_plain(&state.pool.cipher, &k);
                }
                if proxy_url.is_empty() {
                    proxy_url = p;
                }
            }
            Err(_) => return (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
        }
    }

    if base_url.is_empty() || api_key.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "missing_config",
            "message": "base_url 和 API Key 不能为空"
        })));
    }

    let client = crate::proxy::client::cached_client(&state.clients, &proxy_url, "fetch-models", 15);
    let url = crate::router::health::build_health_url(&base_url);
    let upstream_req = client.get(&url);
    let upstream_req = match protocol.as_str() {
        // 与 proxy/client.rs 的 apply_auth 保持一致:同时携带 x-api-key 与
        // Authorization Bearer,兼容只检查 Authorization 的 Anthropic 上游。
        "anthropic" => upstream_req
            .header("x-api-key", &api_key)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("anthropic-version", "2023-06-01"),
        _ => upstream_req.header("Authorization", format!("Bearer {}", api_key)),
    };

    match upstream_req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            // 响应体加 8MB 上限，避免异常大的上游响应撑爆内存
            let body: serde_json::Value = match crate::router::model_test::read_json_limited(resp, 8 * 1024 * 1024).await {
                Ok(v) => v,
                Err(e) => {
                    return (StatusCode::BAD_GATEWAY, Json(json!({
                        "error": "upstream_error",
                        "message": format!("上游响应读取失败: {}", e)
                    })));
                }
            };
            if status != 200 {
                let msg = body["error"]["message"]
                    .as_str()
                    .unwrap_or("upstream rejected the request");
                return (StatusCode::BAD_GATEWAY, Json(json!({
                    "error": "upstream_error",
                    "status": status,
                    "message": format!("上游返回 {}: {}", status, msg)
                })));
            }
            // Both OpenAI and Anthropic list models as {"data": [{"id": ...}]}.
            let models: Vec<String> = body["data"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m["id"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if models.is_empty() {
                return (StatusCode::BAD_GATEWAY, Json(json!({
                    "error": "no_models",
                    "message": "上游响应中没有模型列表"
                })));
            }
            (StatusCode::OK, Json(json!({"models": models})))
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({
            "error": "upstream_error",
            // 只回显 host，避免泄露完整 URL（可能含敏感路径/参数）；
            // reqwest 错误本身也含 URL，用 without_url 去掉
            "message": format!("无法连接上游 {}: {}", url_host(&base_url), e.without_url())
        }))),
    }
}

/// 提取 URL 的 host 部分（含端口），用于错误消息。
fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    rest.split('/').next().unwrap_or(rest)
}

#[derive(Debug, serde::Deserialize)]
pub struct TestModelRequest {
    pub model: String,
    pub message: Option<String>,
    /// Wire protocol to test with; must be one of the channel's declared
    /// protocols. Defaults to the channel's default_protocol.
    pub protocol: Option<String>,
    /// Full message list; overrides `message` when provided.
    pub messages: Option<Vec<TestChatMessage>>,
    /// Test the streaming (SSE) path instead of a plain JSON response.
    pub stream: Option<bool>,
}

#[derive(Debug, serde::Deserialize)]
pub struct TestChatMessage {
    pub role: String,
    pub content: String,
}

/// Send a real chat request through one specific channel to verify that a
/// model works end-to-end (auth + protocol + upstream, optionally streamed).
/// Powers the 模型测试 page and matrix; every outcome is persisted to
/// model_health so the models page reflects measured results.
pub async fn test_provider_model(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
    Json(req): Json<TestModelRequest>,
) -> impl IntoResponse {
    let model = req.model.trim().to_string();
    if model.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "missing_model",
            "message": "model 不能为空"
        })));
    }

    let provider = {
        let conn = match state.pool.read().lock() {
            Ok(c) => c,
            Err(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"})))
            }
        };
        match conn.query_row(
            "SELECT id, name, provider_type, openai_base_url, anthropic_base_url, api_key, models, priority, weight,
                    is_active, health_status, latency_ms, error_rate, last_health_check,
                    max_retries, timeout_secs, created_at, updated_at, proxy_url,
                    model_mapping, consecutive_failures, disabled_reason,
                    protocols, default_protocol, note, website_url
             FROM providers WHERE id = ?1",
            params![provider_id],
            row_to_provider,
        ) {
            Ok(mut p) => {
                // api_key 出库解密(enc:v1: 前缀密文,明文兼容)
                p.api_key = crate::crypto::decrypt_or_plain(&state.pool.cipher, &p.api_key);
                p
            }
            Err(_) => return (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))),
        }
    };

    let protocol = match req.protocol.as_deref() {
        Some(p) => {
            if !channel_protocols(&provider).iter().any(|s| s == p) {
                return (StatusCode::BAD_REQUEST, Json(json!({
                    "error": "unsupported_protocol",
                    "message": format!("该渠道不支持 {} 协议", p)
                })));
            }
            p
        }
        None => channel_protocol(&provider),
    };
    // Both protocols share the same minimal shape for a smoke test.
    let messages: Vec<serde_json::Value> = match req.messages {
        Some(list) if !list.is_empty() => list
            .into_iter()
            .map(|m| json!({"role": m.role, "content": m.content}))
            .collect(),
        _ => {
            let message = req
                .message
                .filter(|m| !m.trim().is_empty())
                .unwrap_or_else(|| "Say OK".to_string());
            vec![json!({"role": "user", "content": message})]
        }
    };

    let stream = req.stream.unwrap_or(false);
    let probe = crate::router::model_test::probe_model(
        &state.clients, &provider, protocol, &model, messages, 1024, stream,
    ).await;
    crate::router::model_test::record_model_health_async(
        state.pool.clone(),
        provider.id.clone(),
        model,
        probe.ok,
        probe.latency_ms,
        probe.error.clone(),
    )
    .await;

    let mut out = json!({
        "ok": probe.ok,
        "latency_ms": probe.latency_ms,
        "protocol": protocol,
        "stream": stream,
    });
    if let Some(snippet) = probe.snippet {
        out["snippet"] = json!(snippet);
    }
    if let Some(response) = probe.response {
        out["response"] = response;
    }
    if !probe.ok {
        out["status"] = json!(probe.status);
        out["error"] = json!(probe.error.unwrap_or_else(|| "upstream error".to_string()));
    }
    (StatusCode::OK, Json(out))
}

/// Latest measured health per (channel, model), newest probe per pair.
/// Powers the models page and the test matrix's initial cell colors.
pub async fn list_model_health(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let conn = match state.pool.read().lock() {
        Ok(c) => c,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "internal_error"}))),
    };

    let mut stmt = match conn.prepare(
        "SELECT provider_id, model, status, latency_ms, error, checked_at FROM model_health"
    ) {
        Ok(s) => s,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "query_failed"}))),
    };

    let rows: Vec<serde_json::Value> = match stmt.query_map([], |row| {
        Ok(json!({
            "provider_id": row.get::<_, String>(0)?,
            "model": row.get::<_, String>(1)?,
            "status": row.get::<_, String>(2)?,
            "latency_ms": row.get::<_, f64>(3)?,
            "error": row.get::<_, String>(4)?,
            "checked_at": row.get::<_, Option<String>>(5)?,
        }))
    }) {
        Ok(r) => r.filter_map(|x| x.ok()).collect(),
        // 同上:查询失败报 500,不静默返回空列表。
        Err(e) => {
            tracing::error!("Failed to list model health: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "query_failed"})));
        }
    };

    (StatusCode::OK, Json(json!(rows)))
}
