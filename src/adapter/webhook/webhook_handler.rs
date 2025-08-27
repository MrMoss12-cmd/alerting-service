// adapter/webhook/webhook_handler.rs

use axum::{
    extract::{Extension, Json, RawBody},
    http::{StatusCode, HeaderMap},
    response::IntoResponse,
    routing::post,
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{info, warn, error};
use std::{collections::HashSet, net::IpAddr, sync::Arc};
use std::time::{SystemTime, UNIX_EPOCH};
use std::str;

#[derive(Clone)]
pub struct WebhookConfig {
    pub valid_tokens: HashSet<String>,
    pub ip_whitelist: HashSet<IpAddr>,
}

#[derive(Clone)]
pub struct WebhookState {
    // Simple in-memory deduplication cache for last N events (in prod use Redis or similar)
    recent_event_ids: Arc<Mutex<HashSet<String>>>,
}

impl WebhookState {
    pub fn new() -> Self {
        Self {
            recent_event_ids: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

// Define webhook payload as flexible JSON map
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum WebhookPayload {
    Json(Value),
    // Could add XML, FormData etc., but axum JSON extractor handles JSON well
}

// Incoming webhook wrapper
#[derive(Debug, Deserialize)]
pub struct WebhookRequest {
    pub event_id: String,
    pub timestamp: u64,
    #[serde(flatten)]
    pub payload: Value, // arbitrary payload
}

// Response structure
#[derive(Serialize)]
struct WebhookResponse {
    status: String,
    message: Option<String>,
}

// Main webhook handler function
pub async fn webhook_handler(
    headers: HeaderMap,
    Json(body): Json<WebhookRequest>,
    Extension(config): Extension<WebhookConfig>,
    Extension(state): Extension<WebhookState>,
    // We could also accept RawBody for XML or form-data, omitted here for brevity
) -> impl IntoResponse {
    let start_time = SystemTime::now();
    info!("Received webhook event_id={} at timestamp={}", body.event_id, body.timestamp);

    // 1. Verify authenticity: token in headers
    match headers.get("x-auth-token").and_then(|v| v.to_str().ok()) {
        Some(token) if config.valid_tokens.contains(token) => (),
        _ => {
            warn!("Unauthorized webhook attempt with missing or invalid token");
            return (StatusCode::UNAUTHORIZED, Json(WebhookResponse {
                status: "error".to_string(),
                message: Some("Unauthorized".to_string()),
            }));
        }
    }

    // 2. Validate IP whitelist (if needed)
    // Note: Getting IP from request context or extensions might be framework-dependent
    // For example: let remote_ip = extract_remote_ip(&request).await;
    // Here skipped for brevity, assume passed or integrate middleware for IP whitelist

    // 3. Protect against replay (deduplication)
    {
        let mut recent_ids = state.recent_event_ids.lock().await;
        if recent_ids.contains(&body.event_id) {
            warn!("Duplicate webhook event_id: {}", body.event_id);
            return (StatusCode::CONFLICT, Json(WebhookResponse {
                status: "error".to_string(),
                message: Some("Duplicate event".to_string()),
            }));
        }
        // Store new event ID; keep cache size limited in prod!
        recent_ids.insert(body.event_id.clone());
    }

    // 4. Validate timestamp freshness (example: reject older than 5 mins)
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if body.timestamp + 300 < now_secs {
        warn!("Stale webhook event timestamp: {}", body.timestamp);
        return (StatusCode::BAD_REQUEST, Json(WebhookResponse {
            status: "error".to_string(),
            message: Some("Stale event timestamp".to_string()),
        }));
    }

    // 5. Audit log event details (avoid sensitive data)
    info!("Webhook payload: {:?}", body.payload);

    // 6. Enqueue or process event in domain (placeholder)
    // TODO: send to event queue, trigger domain logic, etc.

    // 7. Respond success
    let duration = SystemTime::now().duration_since(start_time).unwrap_or_default();
    info!("Webhook processed in {}ms", duration.as_millis());

    (StatusCode::OK, Json(WebhookResponse {
        status: "ok".to_string(),
        message: None,
    }))
}

// Setup axum router for webhook adapter
pub fn webhook_router(config: WebhookConfig, state: WebhookState) -> Router {
    Router::new()
        .route("/webhook", post(webhook_handler))
        .layer(Extension(config))
        .layer(Extension(state))
}

