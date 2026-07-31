use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLog {
    pub id: String,
    pub user_id: Option<String>,
    pub api_key_id: Option<String>,
    pub provider_id: Option<String>,
    pub model: String,
    pub request_type: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub latency_ms: i32,
    pub status_code: i32,
    pub success: bool,
    pub error_message: Option<String>,
    pub cost: f64,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct RequestLogResponse {
    pub id: String,
    pub user_id: Option<String>,
    pub api_key_id: Option<String>,
    pub provider_id: Option<String>,
    pub model: String,
    pub request_type: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub latency_ms: i32,
    pub status_code: i32,
    pub success: bool,
    pub error_message: Option<String>,
    pub cost: f64,
    pub created_at: String,
}

impl From<RequestLog> for RequestLogResponse {
    fn from(l: RequestLog) -> Self {
        Self {
            id: l.id,
            user_id: l.user_id,
            api_key_id: l.api_key_id,
            provider_id: l.provider_id,
            model: l.model,
            request_type: l.request_type,
            prompt_tokens: l.prompt_tokens,
            completion_tokens: l.completion_tokens,
            total_tokens: l.total_tokens,
            latency_ms: l.latency_ms,
            status_code: l.status_code,
            success: l.success,
            error_message: l.error_message,
            cost: l.cost,
            created_at: l.created_at,
        }
    }
}
