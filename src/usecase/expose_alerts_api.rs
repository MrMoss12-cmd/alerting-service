// src/usecase/expose_alerts_api.rs
//! API HTTP para consultar y controlar alertas (REST).
//!
//! Implementación basada en `axum` + `tokio` que ofrece endpoints para:
//!  - listar alertas con filtros y paginación
//!  - obtener detalle de alerta
//!  - iniciar replay controlado de una alerta (requiere permiso)
//!
//! Características implementadas:
//!  - Extractor simple de autenticación JWT (stub) que devuelve tenant y roles.
//!  - Filtrado por fecha, severidad, tenant, canal y estado.
//!  - Paginación (page, size) con metadata en la respuesta.
//!  - Hooks de auditoría (AuditSink trait) para registrar accesos y replays.
//!  - Seguridad: tenant scoping (un usuario sólo ve sus tenants a menos que sea admin).
//!  - Documentación básica con rutas autoexplicativas (OpenAPI puede añadirse).
//!
//! Nota: Este módulo es una composición del API HTTP; integra con `AlertStore` y
//! `ReplayManager` por traits/funciones inyectadas. En producción sustituir stubs
//! por implementaciones reales (JWT validation, DB store, auditoría central).

use axum::{
    extract::{Query, Path, State, Extension},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::RwLock;
use uuid::Uuid;
use tracing::{info, warn};

/// Simplified alert record used for API responses (could map to db model).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRecord {
    pub id: Uuid,
    pub tenant_id: String,
    pub severity: String,
    pub channels_sent: Vec<String>,
    pub payload: serde_json::Value,
    pub original_timestamp: DateTime<Utc>,
    pub status: String, // pending, delivered, failed, etc.
}

/// Query params for listing alerts (filters + pagination)
#[derive(Debug, Deserialize)]
pub struct AlertsListQuery {
    pub tenant_id: Option<String>,
    pub severity: Option<String>,
    pub channel: Option<String>,
    pub status: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub page: Option<u32>,   // 1-based
    pub size: Option<u32>,   // page size
}

/// Response wrapper with pagination metadata
#[derive(Debug, Serialize)]
pub struct PagedResponse<T> {
    pub items: Vec<T>,
    pub page: u32,
    pub size: u32,
    pub total: usize,
}

/// Trait de almacenamiento de alertas (inyectable)
#[axum::async_trait]
pub trait AlertStore: Send + Sync + 'static {
    async fn query_alerts(&self, q: AlertsListQuery) -> anyhow::Result<(Vec<AlertRecord>, usize)>;
    async fn get_alert(&self, id: &Uuid) -> anyhow::Result<Option<AlertRecord>>;
}

/// Trait de auditoría (inyectable) para registrar accesos y acciones
#[axum::async_trait]
pub trait AuditSink: Send + Sync + 'static {
    async fn record_access(&self, actor: &AuthInfo, action: &str, metadata: serde_json::Value);
    async fn record_replay_request(&self, actor: &AuthInfo, alert_id: Uuid, dry_run: bool);
}

/// Información extraída del token/autenticación
#[derive(Debug, Clone)]
pub struct AuthInfo {
    pub subject: String,
    pub tenant_ids: Vec<String>, // tenants the caller can operate on
    pub roles: Vec<String>,      // e.g. ["admin", "operator"]
}

/// Extractor stub for JWT authentication.
/// In production replace with real validation (signature, expiry, scopes).
async fn extract_auth_from_header(req_auth_header: Option<String>) -> Result<AuthInfo, ApiError> {
    // Very simple stub:
    match req_auth_header {
        Some(token) if token.starts_with("Bearer ") => {
            let raw = token.trim_start_matches("Bearer ").to_string();
            // Decode stub: if token contains "admin" then admin role & wildcard tenant
            if raw.contains("admin") {
                Ok(AuthInfo {
                    subject: "admin-user".into(),
                    tenant_ids: vec!["*".into()],
                    roles: vec!["admin".into()],
                })
            } else if raw.contains("tenant:") {
                // token like "tenant:tenant_123"
                let parts: Vec<&str> = raw.split(':').collect();
                let tenant_id = parts.get(1).map(|s| s.to_string()).unwrap_or_else(|| "tenant_unknown".into());
                Ok(AuthInfo {
                    subject: format!("user-{}", tenant_id),
                    tenant_ids: vec![tenant_id],
                    roles: vec!["operator".into()],
                })
            } else {
                Err(ApiError::unauthorized("Invalid token format"))
            }
        }
        _ => Err(ApiError::unauthorized("Missing Authorization header")),
    }
}

/// Simple API error type
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    fn unauthorized(msg: &str) -> Self {
        Self { status: StatusCode::UNAUTHORIZED, message: msg.into() }
    }
    fn forbidden(msg: &str) -> Self {
        Self { status: StatusCode::FORBIDDEN, message: msg.into() }
    }
    fn bad_request(msg: &str) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: msg.into() }
    }
    fn internal(msg: &str) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: msg.into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let body = serde_json::json!({
            "error": self.message
        });
        (self.status, Json(body)).into_response()
    }
}

/// Application state injected into handlers
#[derive(Clone)]
pub struct ApiState {
    pub store: Arc<dyn AlertStore>,
    pub audit: Arc<dyn AuditSink>,
}

type SharedState = Arc<ApiState>;

/// Handler: list alerts with filters and pagination
async fn list_alerts_handler(
    State(state): State<SharedState>,
    Query(query): Query<AlertsListQuery>,
    Extension(auth_header): Extension<Option<String>>,
) -> Result<impl IntoResponse, ApiError> {
    // Auth
    let auth = extract_auth_from_header(auth_header).await?;
    // Tenant scope: if caller not admin, force tenant filter to their tenant
    let mut actual_query = query;
    if !auth.roles.contains(&"admin".to_string()) {
        // if query.tenant_id provided, ensure it's allowed
        if let Some(ref qtid) = actual_query.tenant_id {
            if !auth.tenant_ids.contains(&"*".to_string()) && !auth.tenant_ids.contains(qtid) {
                return Err(ApiError::forbidden("Not allowed for this tenant"));
            }
        } else {
            // enforce tenant from auth
            actual_query.tenant_id = auth.tenant_ids.get(0).cloned();
        }
    }

    // default pagination
    let page = actual_query.page.unwrap_or(1).max(1);
    let size = actual_query.size.unwrap_or(50).clamp(1, 100);

    // Query store (store returns items + total count)
    let (items, total) = state.store.query_alerts(actual_query.clone()).await.map_err(|e| {
        warn!("AlertStore query failed: {:?}", e);
        ApiError::internal("Failed to query alerts")
    })?;

    // Apply simple pagination over returned slice for demo (store may implement server-side)
    let start = ((page - 1) as usize) * (size as usize);
    let end = start + (size as usize);
    let paged_items = items.into_iter().skip(start).take(size as usize).collect::<Vec<_>>();

    // Audit the access
    state.audit.record_access(&auth, "list_alerts", serde_json::json!({
        "page": page,
        "size": size,
        "filters": {
            "tenant_id": actual_query.tenant_id,
            "severity": actual_query.severity,
            "channel": actual_query.channel,
            "status": actual_query.status,
        }
    })).await;

    Ok((StatusCode::OK, Json(PagedResponse { items: paged_items, page, size, total })))
}

/// Handler: get alert detail
async fn get_alert_handler(
    State(state): State<SharedState>,
    Path(alert_id): Path<Uuid>,
    Extension(auth_header): Extension<Option<String>>,
) -> Result<impl IntoResponse, ApiError> {
    let auth = extract_auth_from_header(auth_header).await?;
    let maybe = state.store.get_alert(&alert_id).await.map_err(|e| {
        warn!("AlertStore get failed: {:?}", e);
        ApiError::internal("Failed to get alert")
    })?;

    let alert = match maybe {
        Some(a) => a,
        None => return Err(ApiError::bad_request("Alert not found")),
    };

    // tenant scoping
    if !auth.roles.contains(&"admin".to_string()) && !auth.tenant_ids.contains(&alert.tenant_id) {
        return Err(ApiError::forbidden("Not allowed for this tenant"));
    }

    state.audit.record_access(&auth, "get_alert", serde_json::json!({"alert_id": alert_id.to_string()})).await;

    Ok((StatusCode::OK, Json(alert)))
}

/// Request body for replay API
#[derive(Debug, Deserialize)]
pub struct ReplayRequest {
    pub dry_run: Option<bool>, // default true
    pub reason: Option<String>,
}

/// Handler: trigger replay for a single alert (controlled)
async fn replay_alert_handler(
    State(state): State<SharedState>,
    Path(alert_id): Path<Uuid>,
    Extension(auth_header): Extension<Option<String>>,
    Json(payload): Json<ReplayRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let auth = extract_auth_from_header(auth_header).await?;

    let dry_run = payload.dry_run.unwrap_or(true);

    // Only admin or tenant operators of the alert's tenant can trigger replays.
    let maybe = state.store.get_alert(&alert_id).await.map_err(|e| {
        warn!("AlertStore get failed: {:?}", e);
        ApiError::internal("Failed to get alert")
    })?;

    let alert = maybe.ok_or_else(|| ApiError::bad_request("Alert not found"))?;

    if !auth.roles.contains(&"admin".to_string()) && !auth.tenant_ids.contains(&alert.tenant_id) {
        return Err(ApiError::forbidden("Not allowed for this tenant"));
    }

    // Security: if not admin and dry_run == false, forbid (require explicit admin)
    if !dry_run && !auth.roles.contains(&"admin".to_string()) {
        return Err(ApiError::forbidden("Only admin can execute non-dry-run replays"));
    }

    // Audit the replay request
    state.audit.record_replay_request(&auth, alert_id, dry_run).await;

    // For demo: we won't actually call a full ReplayManager; in production,
    // this would enqueue a job or call ReplayManager::run_replay for that alert.
    // Here we respond with accepted and metadata.
    let response = serde_json::json!({
        "alert_id": alert_id.to_string(),
        "dry_run": dry_run,
        "status": "accepted",
        "message": if dry_run { "Replay scheduled in dry-run mode" } else { "Replay scheduled (live)" },
        "requested_by": auth.subject,
    });

    Ok((StatusCode::ACCEPTED, Json(response)))
}

/// Compose router with state and routes
pub fn make_router(state: SharedState) -> Router {
    Router::new()
        .route("/api/v1/alerts", get(list_alerts_handler))
        .route("/api/v1/alerts/:id", get(get_alert_handler))
        .route("/api/v1/alerts/:id/replay", post(replay_alert_handler))
        .layer(axum::extract::Extension::<Option<String>>::layer(None)) // placeholder extension for auth header injection
        .with_state(state)
}

/// ---------------------------
/// Simple in-memory store & audit implementations for demo/testing
/// ---------------------------

/// Very small in-memory AlertStore (not production-grade).
pub struct InMemoryAlertStore {
    items: Arc<RwLock<Vec<AlertRecord>>>,
}

impl InMemoryAlertStore {
    pub fn new(initial: Vec<AlertRecord>) -> Self {
        Self { items: Arc::new(RwLock::new(initial)) }
    }
}

#[axum::async_trait]
impl AlertStore for InMemoryAlertStore {
    async fn query_alerts(&self, q: AlertsListQuery) -> anyhow::Result<(Vec<AlertRecord>, usize)> {
        let items = self.items.read().await;
        let mut out: Vec<AlertRecord> = items.clone().into_iter().filter(|a| {
            if let Some(ref tenant) = q.tenant_id {
                if &a.tenant_id != tenant { return false; }
            }
            if let Some(ref sev) = q.severity {
                if !a.severity.eq_ignore_ascii_case(sev) { return false; }
            }
            if let Some(ref ch) = q.channel {
                if !a.channels_sent.iter().any(|c| c.eq_ignore_ascii_case(ch)) { return false; }
            }
            if let Some(ref status) = q.status {
                if !a.status.eq_ignore_ascii_case(status) { return false; }
            }
            if let Some(from) = q.from {
                if a.original_timestamp < from { return false; }
            }
            if let Some(to) = q.to {
                if a.original_timestamp > to { return false; }
            }
            true
        }).collect();
        let total = out.len();
        Ok((out, total))
    }

    async fn get_alert(&self, id: &Uuid) -> anyhow::Result<Option<AlertRecord>> {
        let items = self.items.read().await;
        Ok(items.iter().cloned().find(|a| &a.id == id))
    }
}

/// Simple AuditSink that logs to tracing (stdout)
pub struct TracingAuditSink;

#[axum::async_trait]
impl AuditSink for TracingAuditSink {
    async fn record_access(&self, actor: &AuthInfo, action: &str, metadata: serde_json::Value) {
        info!(actor = %actor.subject, action = %action, meta = %metadata.to_string(), "audit_access");
    }
    async fn record_replay_request(&self, actor: &AuthInfo, alert_id: Uuid, dry_run: bool) {
        info!(actor = %actor.subject, alert_id = %alert_id.to_string(), dry_run = dry_run, "audit_replay");
    }
}

/// ---------------------------
/// Example main to run the API server (demo)
/// ---------------------------
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Seed with some sample alerts
    let sample_alerts = vec![
        AlertRecord {
            id: Uuid::new_v4(),
            tenant_id: "tenant_alpha".into(),
            severity: "critical".into(),
            channels_sent: vec!["email".into()],
            payload: serde_json::json!({"msg":"disk full"}),
            original_timestamp: Utc::now(),
            status: "failed".into(),
        },
        AlertRecord {
            id: Uuid::new_v4(),
            tenant_id: "tenant_alpha".into(),
            severity: "warning".into(),
            channels_sent: vec!["slack".into()],
            payload: serde_json::json!({"msg":"high latency"}),
            original_timestamp: Utc::now(),
            status: "delivered".into(),
        },
    ];

    let store = InMemoryAlertStore::new(sample_alerts);
    let audit = TracingAuditSink;

    let state = Arc::new(ApiState {
        store: Arc::new(store),
        audit: Arc::new(audit),
    });

    // Build router
    let app = make_router(state.clone())
        // middleware to inject a fake Authorization header string (for demo)
        .layer(axum::middleware::from_fn(|req, next| async move {
            // For demo: read Authorization header and store into Extension so handlers can access it.
            // In real usage the extract_auth_from_header would read from headers directly.
            let auth_header = req.headers().get("authorization").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
            let req = req
                .map(|mut parts| {
                    parts.extensions_mut().insert::<Option<String>>(auth_header.clone());
                    parts
                });
            next.run(req).await
        }));

    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));
    info!("Starting alerts API on {}", addr);
    axum::Server::bind(&addr).serve(app.into_make_service()).await?;

    Ok(())
}
