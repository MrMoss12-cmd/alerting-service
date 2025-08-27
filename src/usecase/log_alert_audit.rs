use chrono::{DateTime, Utc};
use serde::{Serialize, Deserialize};
use sha2::{Sha256, Digest};
use tokio::sync::RwLock;
use std::{collections::VecDeque, sync::Arc};

/// Evento en el ciclo de vida de una alerta
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum AuditEventType {
    Created,
    Classified { severity: String, rule_id: Option<String> },
    Routed { destination: String, channel: String },
    Delivered { channel: String, status: String },
    RetryAttempt { channel: String, attempt_number: u32 },
    Closed { reason: String },
    Other(String),
}

/// Registro individual de auditoría
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AuditLogEntry {
    pub alert_id: String,
    pub tenant_id: String,
    pub timestamp: DateTime<Utc>,
    pub event_type: AuditEventType,
    pub source: Option<String>,       // Ej: "rule_engine", "dispatcher"
    pub policy_id: Option<String>,    // Regla o política aplicada si aplica
    pub metadata: Option<serde_json::Value>, // Datos adicionales no sensibles
    pub integrity_hash: String,       // Hash para garantizar integridad
}

/// Log de auditoría con almacenamiento WORM simulado en memoria (fifo limitado)
#[derive(Clone)]
pub struct AuditLogger {
    entries: Arc<RwLock<VecDeque<AuditLogEntry>>>,
    capacity: usize, // Máximo registros a mantener en memoria
}

impl AuditLogger {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Arc::new(RwLock::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    /// Crea el hash SHA256 para integridad de cada registro
    fn compute_hash(entry: &AuditLogEntry) -> String {
        let json = serde_json::to_string(entry).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(json.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Anonimiza información personal si es necesario (placeholder simple)
    fn anonymize_metadata(metadata: &Option<serde_json::Value>) -> Option<serde_json::Value> {
        if let Some(data) = metadata {
            // Aquí se podría hacer un análisis y remover info sensible
            // Ejemplo simple: eliminar campos "email" y "phone"
            let mut map = data.as_object().cloned().unwrap_or_default();
            map.remove("email");
            map.remove("phone");
            Some(serde_json::Value::Object(map))
        } else {
            None
        }
    }

    /// Registra un evento de auditoría con integridad y anonimización
    pub async fn log_event(
        &self,
        alert_id: &str,
        tenant_id: &str,
        event_type: AuditEventType,
        source: Option<String>,
        policy_id: Option<String>,
        metadata: Option<serde_json::Value>,
    ) {
        let timestamp = Utc::now();
        let anonymized_metadata = Self::anonymize_metadata(&metadata);

        let mut entry = AuditLogEntry {
            alert_id: alert_id.to_string(),
            tenant_id: tenant_id.to_string(),
            timestamp,
            event_type,
            source,
            policy_id,
            metadata: anonymized_metadata,
            integrity_hash: String::new(), // temporal, se calcula luego
        };

        // Calcular hash de integridad
        entry.integrity_hash = Self::compute_hash(&entry);

        // Insertar en log con límite de capacidad (WORM simulado)
        let mut entries = self.entries.write().await;
        if entries.len() == self.capacity {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// Exporta registros en formato JSON para auditorías externas
    pub async fn export_logs_json(&self) -> String {
        let entries = self.entries.read().await;
        serde_json::to_string_pretty(&*entries).unwrap_or_default()
    }
}

#[tokio::main]
async fn main() {
    let audit_logger = AuditLogger::new(1000); // mantiene últimos 1000 eventos

    audit_logger.log_event(
        "alert-123",
        "tenant-abc",
        AuditEventType::Created,
        Some("ingestion_service".into()),
        None,
        Some(serde_json::json!({"details": "Alerta recibida"})),
    ).await;

    audit_logger.log_event(
        "alert-123",
        "tenant-abc",
        AuditEventType::Classified {
            severity: "Critical".into(),
            rule_id: Some("rule-789".into()),
        },
        Some("classification_engine".into()),
        Some("rule-789".into()),
        None,
    ).await;

    // Exportar para auditoría o monitoreo
    let exported = audit_logger.export_logs_json().await;
    println!("Registros de auditoría:\n{}", exported);
}
