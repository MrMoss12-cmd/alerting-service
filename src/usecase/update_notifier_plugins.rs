// src/usecase/update_notifier_plugins.rs
//! Gestión dinámica de plugins notificadores (register / update / rollback / remove).
//!
//! Diseño y características:
//! - Registro concurrente y seguro (Arc + RwLock).
//! - Metadata y versionado por plugin (puede revertirse a versiones anteriores).
//! - Staging area para validación antes de hacer hot-swap en producción.
//! - Soporte de configuración específica por tenant.
//! - Hooks de auditoría para registrar cambios y quién los hizo.
//! - Validaciones básicas (checksum/signature stub) y sandboxing conceptual
//!   (ejecutar plugin calls en tasks aisladas; en producción usar procesos o wasm).
//! - Mecanismo de health-check y soft-disable para degradación segura.
//!
//! Nota: esta implementación es una base portable y testable; en producción
//! la carga dinámica real (dylib / wasm / subprocess) y sandboxing deben usarse.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt::Display,
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::RwLock;
use uuid::Uuid;

/// Resultado genérico
type Result<T> = std::result::Result<T, PluginError>;

/// Errores del gestor de plugins
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("Plugin not found: {0}")]
    NotFound(String),

    #[error("Plugin already exists: {0}")]
    AlreadyExists(String),

    #[error("Validation failed: {0}")]
    ValidationFailed(String),

    #[error("Invalid operation: {0}")]
    InvalidOperation(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Other: {0}")]
    Other(String),
}

/// Identificador de plugin (p.ej. "email-notifier")
pub type PluginId = String;

/// Versión de plugin
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginVersion {
    pub version: String,
    pub checksum_sha256: String,
    pub uploaded_at: chrono::DateTime<Utc>,
    pub uploaded_by: String,
    pub source_path: Option<PathBuf>, // ubicación de artefacto (opcional)
    pub notes: Option<String>,
}

/// Estado operativo del plugin
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PluginState {
    Active,
    Staged,
    Disabled,
    Failed,
}

/// Metadata y registro por versión
#[derive(Debug, Clone)]
pub struct PluginRecord {
    pub id: PluginId,
    pub current_version: Option<PluginVersion>,
    pub history: Vec<PluginVersion>, // orden cronológico ascendente
    pub state: PluginState,
    pub tenants_config: HashMap<String, serde_json::Value>, // tenant_id -> config
}

/// Trait que implementan los plugins notifiers.
/// En producción esto puede mapear a objetos cargados dinámicamente o a adaptadores wasm.
#[async_trait::async_trait]
pub trait NotifierPlugin: Send + Sync {
    /// Envía notificación; implementación plugin-specific
    async fn send(&self, tenant_id: &str, payload: serde_json::Value) -> std::result::Result<(), String>;

    /// Simple healthcheck (puede ser usada antes de hacer hot-swap)
    async fn healthcheck(&self) -> bool;

    /// Retorna a qué canal corresponde este plugin (ej: "email", "slack", "sms")
    fn channel(&self) -> String;
}

/// Stub de plugin en memoria para tests/demos.
pub struct StubPlugin {
    channel_name: String,
}

impl StubPlugin {
    pub fn new(channel_name: impl Into<String>) -> Self {
        Self { channel_name: channel_name.into() }
    }
}

#[async_trait::async_trait]
impl NotifierPlugin for StubPlugin {
    async fn send(&self, _tenant_id: &str, _payload: serde_json::Value) -> std::result::Result<(), String> {
        // simulación: ok
        Ok(())
    }

    async fn healthcheck(&self) -> bool {
        true
    }

    fn channel(&self) -> String {
        self.channel_name.clone()
    }
}

/// AuditSink (inyectable) — registra quién y cuándo hizo cambios.
#[async_trait::async_trait]
pub trait AuditSink: Send + Sync {
    async fn record(&self, actor: &str, action: &str, plugin_id: &str, details: serde_json::Value);
}

/// Simple audit sink that logs to stdout via tracing.
pub struct TracingAudit;

#[async_trait::async_trait]
impl AuditSink for TracingAudit {
    async fn record(&self, actor: &str, action: &str, plugin_id: &str, details: serde_json::Value) {
        tracing::info!(actor = actor, action = action, plugin = plugin_id, details = %details.to_string(), "plugin_audit");
    }
}

/// Gestor principal de plugins
pub struct PluginManager {
    /// Map plugin_id -> record (metadata & history)
    records: Arc<RwLock<HashMap<PluginId, PluginRecord>>>,

    /// Map plugin_id -> runtime instance (boxed trait object)
    runtime: Arc<RwLock<HashMap<PluginId, Arc<dyn NotifierPlugin>>>>,

    /// Audit sink
    audit: Arc<dyn AuditSink>,
}

impl PluginManager {
    pub fn new(audit: Arc<dyn AuditSink>) -> Self {
        Self {
            records: Arc::new(RwLock::new(HashMap::new())),
            runtime: Arc::new(RwLock::new(HashMap::new())),
            audit,
        }
    }

    /// Valida un paquete de plugin (checksum + firma stub). En producción
    /// esto haría verificación de firma digital y análisis estático.
    pub async fn validate_plugin_package(&self, bytes: &[u8], expected_checksum: Option<&str>) -> Result<String> {
        // checksum SHA-256
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let checksum = format!("{:x}", hasher.finalize());

        if let Some(exp) = expected_checksum {
            if exp != checksum {
                return Err(PluginError::ValidationFailed("checksum mismatch".into()));
            }
        }

        // aquí podríamos ejecutar análisis estático o virus-scan; lo omitimos
        Ok(checksum)
    }

    /// Staged upload: recibe el package bytes y metadata, valida y crea un new version entry
    /// No hace hot-swap hasta confirm_update().
    pub async fn stage_new_version(
        &self,
        plugin_id: &str,
        bytes: Vec<u8>,
        version: &str,
        uploaded_by: &str,
        notes: Option<String>,
    ) -> Result<PluginVersion> {
        // basic validate
        let checksum = self.validate_plugin_package(&bytes, None).await?;
        let ver = PluginVersion {
            version: version.to_string(),
            checksum_sha256: checksum.clone(),
            uploaded_at: Utc::now(),
            uploaded_by: uploaded_by.to_string(),
            source_path: None,
            notes,
        };

        // create or update record in staged state
        let mut rec_map = self.records.write().await;
        let mut rec = rec_map.entry(plugin_id.to_string()).or_insert_with(|| PluginRecord {
            id: plugin_id.to_string(),
            current_version: None,
            history: vec![],
            state: PluginState::Staged,
            tenants_config: HashMap::new(),
        });

        // append to history as staged version
        rec.history.push(ver.clone());
        rec.state = PluginState::Staged;

        // audit
        self.audit.record(uploaded_by, "stage_new_version", plugin_id, serde_json::json!({
            "version": ver.version,
            "checksum": ver.checksum_sha256,
        })).await;

        Ok(ver)
    }

    /// Confirmar y activar la versión más reciente (hot-swap).
    /// `actor` es quien realiza la activación (auditoría).
    pub async fn confirm_update(&self, plugin_id: &str, actor: &str) -> Result<()> {
        // lock records & perform validation and swap
        let mut rec_map = self.records.write().await;
        let rec = rec_map.get_mut(plugin_id).ok_or_else(|| PluginError::NotFound(plugin_id.to_string()))?;

        // must have at least one history entry
        let v = rec.history.last().cloned().ok_or_else(|| PluginError::InvalidOperation("no staged version".into()))?;

        // Simulate loading runtime instance from package: in real world we'd load the binary/wasm
        // For safety we perform a healthcheck on a stub instance or a factory.
        // Here we create a StubPlugin for demonstration.
        let plugin_instance: Arc<dyn NotifierPlugin> = Arc::new(StubPlugin::new(plugin_id.to_string()));

        // healthcheck with timeout (soft)
        let healthy = tokio::time::timeout(std::time::Duration::from_millis(500), plugin_instance.healthcheck()).await
            .map(|r| r.unwrap_or(false))
            .unwrap_or(false);

        if !healthy {
            rec.state = PluginState::Failed;
            self.audit.record(actor, "confirm_update_failed", plugin_id, serde_json::json!({
                "version": v.version,
                "reason": "healthcheck_failed"
            })).await;
            return Err(PluginError::ValidationFailed("plugin healthcheck failed".into()));
        }

        // Hot-swap: atomically put into runtime map, and mark as Active
        {
            let mut rt = self.runtime.write().await;
            rt.insert(plugin_id.to_string(), plugin_instance);
        }
        rec.current_version = Some(v.clone());
        rec.state = PluginState::Active;

        // audit
        self.audit.record(actor, "confirm_update", plugin_id, serde_json::json!({
            "version": v.version,
        })).await;

        Ok(())
    }

    /// Rollback to previous version if exists.
    pub async fn rollback(&self, plugin_id: &str, actor: &str) -> Result<()> {
        let mut rec_map = self.records.write().await;
        let rec = rec_map.get_mut(plugin_id).ok_or_else(|| PluginError::NotFound(plugin_id.to_string()))?;

        if rec.history.len() < 2 {
            return Err(PluginError::InvalidOperation("no previous version to rollback".into()));
        }

        // remove last entry (current) and pick previous
        rec.history.pop();
        let prev = rec.history.last().cloned().ok_or_else(|| PluginError::InvalidOperation("no prev".into()))?;

        // instantiate stub instance for prev
        let plugin_instance: Arc<dyn NotifierPlugin> = Arc::new(StubPlugin::new(plugin_id.to_string()));

        // healthcheck
        let healthy = tokio::time::timeout(std::time::Duration::from_millis(500), plugin_instance.healthcheck()).await
            .map(|r| r.unwrap_or(false))
            .unwrap_or(false);

        if !healthy {
            rec.state = PluginState::Failed;
            self.audit.record(actor, "rollback_failed", plugin_id, serde_json::json!({
                "version": prev.version,
                "reason": "healthcheck_failed"
            })).await;
            return Err(PluginError::ValidationFailed("rollback healthcheck failed".into()));
        }

        // swap runtime
        {
            let mut rt = self.runtime.write().await;
            rt.insert(plugin_id.to_string(), plugin_instance);
        }
        rec.current_version = Some(prev.clone());
        rec.state = PluginState::Active;

        self.audit.record(actor, "rollback", plugin_id, serde_json::json!({
            "version": prev.version,
        })).await;

        Ok(())
    }

    /// Remove plugin entirely (soft delete): disables and clears runtime.
    pub async fn remove_plugin(&self, plugin_id: &str, actor: &str) -> Result<()> {
        {
            let mut rec_map = self.records.write().await;
            let rec = rec_map.get_mut(plugin_id).ok_or_else(|| PluginError::NotFound(plugin_id.to_string()))?;
            rec.state = PluginState::Disabled;
        }

        {
            let mut rt = self.runtime.write().await;
            rt.remove(plugin_id);
        }

        self.audit.record(actor, "remove_plugin", plugin_id, serde_json::json!({})).await;
        Ok(())
    }

    /// Get plugin instance for tenant: resolves plugin by id and tenant config.
    /// If plugin not active returns error.
    pub async fn get_plugin_for_tenant(&self, plugin_id: &str, tenant_id: &str) -> Result<Arc<dyn NotifierPlugin>> {
        // check runtime
        let rt = self.runtime.read().await;
        let inst = rt.get(plugin_id).cloned().ok_or_else(|| PluginError::NotFound(plugin_id.to_string()))?;

        // Optional: consult tenant-specific configuration and wrap adapter if needed.
        // For demo we just return the instance.
        Ok(inst)
    }

    /// List all plugin metadata (for management endpoints)
    pub async fn list_plugins(&self) -> Vec<PluginRecordView> {
        let rec_map = self.records.read().await;
        rec_map.values().map(|r| PluginRecordView::from(r)).collect()
    }

    /// Set tenant-specific config for a plugin (isolated per tenant)
    pub async fn set_tenant_config(&self, plugin_id: &str, tenant_id: &str, config: serde_json::Value, actor: &str) -> Result<()> {
        let mut rec_map = self.records.write().await;
        let rec = rec_map.get_mut(plugin_id).ok_or_else(|| PluginError::NotFound(plugin_id.to_string()))?;
        rec.tenants_config.insert(tenant_id.to_string(), config.clone());
        self.audit.record(actor, "set_tenant_config", plugin_id, serde_json::json!({
            "tenant": tenant_id,
            "config": config
        })).await;
        Ok(())
    }

    /// Healthcheck a un plugin activo (time-limited).
    pub async fn plugin_healthcheck(&self, plugin_id: &str) -> Result<bool> {
        let rt = self.runtime.read().await;
        let inst = rt.get(plugin_id).cloned().ok_or_else(|| PluginError::NotFound(plugin_id.to_string()))?;
        let healthy = tokio::time::timeout(std::time::Duration::from_millis(500), inst.healthcheck()).await
            .map(|r| r.unwrap_or(false))
            .unwrap_or(false);
        Ok(healthy)
    }
}

/// Vista ligera para exponer metadata
#[derive(Debug, Clone, Serialize)]
pub struct PluginRecordView {
    pub id: PluginId,
    pub current_version: Option<PluginVersion>,
    pub state: PluginState,
    pub tenants: Vec<String>,
}

impl From<&PluginRecord> for PluginRecordView {
    fn from(r: &PluginRecord) -> Self {
        Self {
            id: r.id.clone(),
            current_version: r.current_version.clone(),
            state: r.state.clone(),
            tenants: r.tenants_config.keys().cloned().collect(),
        }
    }
}

/// Utility: compute SHA256 of bytes as hex string
pub fn compute_sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// --------------------
/// Ejemplo / tests
/// --------------------
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn test_stage_confirm_and_list() {
        let audit = Arc::new(TracingAudit);
        let mgr = PluginManager::new(audit);

        // stage plugin v1
        let pkg_v1 = b"fake-binary-v1".to_vec();
        let ver1 = mgr.stage_new_version("email", pkg_v1, "1.0.0", "alice", Some("initial")).await.unwrap();

        // confirm
        mgr.confirm_update("email", "alice").await.unwrap();

        // list and assert active
        let list = mgr.list_plugins().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "email");
        assert_eq!(list[0].state, PluginState::Active);
        assert_eq!(list[0].current_version.as_ref().unwrap().version, "1.0.0");

        // set tenant config
        mgr.set_tenant_config("email", "tenant_a", json!({"from":"noreply@example.com"}), "alice").await.unwrap();
        // healthcheck
        let ok = mgr.plugin_healthcheck("email").await.unwrap();
        assert!(ok);
    }

    #[tokio::test]
    async fn test_rollback() {
        let audit = Arc::new(TracingAudit);
        let mgr = PluginManager::new(audit);

        // stage v1 and confirm
        mgr.stage_new_version("sms", b"v1".to_vec(), "1.0.0", "bob", None).await.unwrap();
        mgr.confirm_update("sms", "bob").await.unwrap();

        // stage v2 and confirm
        mgr.stage_new_version("sms", b"v2".to_vec(), "2.0.0", "bob", None).await.unwrap();
        mgr.confirm_update("sms", "bob").await.unwrap();

        // rollback to v1
        mgr.rollback("sms", "bob").await.unwrap();

        let list = mgr.list_plugins().await;
        assert_eq!(list[0].id, "sms");
        assert_eq!(list[0].state, PluginState::Active);
        assert_eq!(list[0].current_version.as_ref().unwrap().version, "1.0.0");
    }

    #[tokio::test]
    async fn test_remove_plugin() {
        let audit = Arc::new(TracingAudit);
        let mgr = PluginManager::new(audit);

        mgr.stage_new_version("webpush", b"v1".to_vec(), "1.0.0", "carol", None).await.unwrap();
        mgr.confirm_update("webpush", "carol").await.unwrap();

        mgr.remove_plugin("webpush", "carol").await.unwrap();

        let list = mgr.list_plugins().await;
        assert_eq!(list[0].state, PluginState::Disabled);
    }
}
