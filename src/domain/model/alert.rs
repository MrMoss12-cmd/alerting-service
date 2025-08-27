// src/domain/model/alert.rs

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

/// Importa Severity y DeliveryStatus desde otros módulos del dominio.
/// Si aún no están implementados, puedes crear stubs equivalentes.
use crate::domain::model::severity::Severity;
use crate::domain::model::delivery_status::DeliveryStatus;

/// Representa una única alerta con metadatos necesarios para
/// clasificación, enrutamiento, auditoría y entrega.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    /// Identificador único (UUID v4) para trazabilidad y deduplicación.
    pub id: String,

    /// Origen del evento (ej. "deployment.step.completed", "kernel-rm", "simulator").
    pub source: String,

    /// Dominio o categoría del evento (ej. "billing", "metrics", "infra").
    pub domain: Option<String>,

    /// Identificador del tenant (si aplica) para multitenancy.
    pub tenant_id: Option<String>,

    /// Identificador de paso o componente (opcional).
    pub step_id: Option<String>,

    /// Estado semántico de origen (ej. "success", "error", "pending").
    pub status: Option<String>,

    /// Severidad canónica usada por el enrutador/políticas.
    pub severity: Severity,

    /// Payload estructurado con datos adicionales (JSON flexible).
    pub payload: JsonValue,

    /// Metadatos arbitrarios y extensibles. Usar BTreeMap para orden determinista.
    /// (ej. prefered_channels: ["email","sms"], detector_version, etc.)
    pub metadata: BTreeMap<String, JsonValue>,

    /// Timestamp de recepción en el sistema (UTC).
    pub received_at: DateTime<Utc>,

    /// Timestamp de último procesamiento o transformación (UTC).
    pub processed_at: Option<DateTime<Utc>>,

    /// Registro inmutable de transformaciones / enriquecimientos realizados
    /// durante el ciclo de vida de la alerta (para auditoría).
    pub audit_trail: Vec<TransformationRecord>,

    /// Estado por canal (multi-canal): cada canal mantiene su DeliveryInfo.
    pub channel_delivery: HashMap<String, ChannelDeliveryInfo>,

    /// Firma u origen verificado (opcional). Puede ser JWT, HMAC u otra representación.
    /// No se expone en logs sin redacción.
    pub signature: Option<String>,

    /// Origen declarado por el emisor (ej. URL, service-name). Útil para verificación.
    pub origin: Option<String>,

    /// Campo genérico para futuras extensiones. Evita romper compatibilidad.
    #[serde(flatten)]
    pub extra: BTreeMap<String, JsonValue>,
}

/// Registro de una transformación, enriquecimiento o decisión hecha sobre la alerta.
/// Está diseñado para auditoría y trazabilidad.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformationRecord {
    /// Actor que aplicó la transformación (ej. "classifier-v1", "rule-engine", "user:admin").
    pub actor: String,

    /// Acción realizada (ej. "classified", "enriched", "redacted", "routed").
    pub action: String,

    /// Timestamp de la acción (UTC).
    pub timestamp: DateTime<Utc>,

    /// Detalles libres (estructura JSON) sobre la acción realizada.
    pub details: Option<JsonValue>,
}

/// Información de entrega por canal, con historial resumido para reintentos/fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelDeliveryInfo {
    /// Estado actual para este canal (pending, delivered, failed, retrying, skipped).
    pub status: DeliveryStatus,

    /// Número de intentos realizados.
    pub attempts: u32,

    /// Último error textual si existió (redactado para no incluir secretos).
    pub last_error: Option<String>,

    /// Timestamp del último intento (UTC).
    pub last_attempt: Option<DateTime<Utc>>,

    /// Metadata específica del canal (ej. provider_id, provider_response_code).
    pub channel_metadata: BTreeMap<String, JsonValue>,
}

impl Alert {
    /// Crea una nueva alerta mínima, generando un UUID y marcando `received_at`.
    pub fn new<S: Into<String>>(source: S, severity: Severity, payload: JsonValue) -> Self {
        Alert {
            id: Uuid::new_v4().to_string(),
            source: source.into(),
            domain: None,
            tenant_id: None,
            step_id: None,
            status: None,
            severity,
            payload,
            metadata: BTreeMap::new(),
            received_at: Utc::now(),
            processed_at: None,
            audit_trail: Vec::new(),
            channel_delivery: HashMap::new(),
            signature: None,
            origin: None,
            extra: BTreeMap::new(),
        }
    }

    /// Agrega un registro de transformación al `audit_trail`.
    pub fn add_transformation(&mut self, actor: impl Into<String>, action: impl Into<String>, details: Option<JsonValue>) {
        let record = TransformationRecord {
            actor: actor.into(),
            action: action.into(),
            timestamp: Utc::now(),
            details,
        };
        // actualiza processed_at para reflejar actividad reciente
        self.processed_at = Some(Utc::now());
        self.audit_trail.push(record);
    }

    /// Registra un intento de entrega para un canal (incrementa attempts y actualiza estado).
    pub fn register_delivery_attempt(&mut self, channel: &str, status: DeliveryStatus, last_error: Option<String>, channel_metadata: Option<BTreeMap<String, JsonValue>>) {
        let info = self.channel_delivery.entry(channel.to_string()).or_insert(ChannelDeliveryInfo {
            status: DeliveryStatus::Pending,
            attempts: 0,
            last_error: None,
            last_attempt: None,
            channel_metadata: BTreeMap::new(),
        });

        info.attempts = info.attempts.saturating_add(1);
        info.status = status;
        info.last_error = last_error.map(|e| Self::redact_error(&e));
        info.last_attempt = Some(Utc::now());
        if let Some(md) = channel_metadata {
            for (k, v) in md {
                info.channel_metadata.insert(k, v);
            }
        }

        // Añadimos una entrada de auditoría ligada al canal
        let details = serde_json::json!({
            "channel": channel,
            "status": format!("{:?}", info.status),
            "attempts": info.attempts
        });
        self.add_transformation("notification_dispatcher", "delivery_attempt", Some(details));
    }

    /// Método asíncrono para verificar la firma/origen. Acepta un verificador provisto por el caller.
    /// El verificador recibe (signature, canonical_payload) y devuelve bool.
    ///
    /// Se ejecuta en un task bloqueante si el verificador es costoso; así encaja bien con tokio.
    pub async fn verify_signature_async<F>(&self, canonical_payload: String, verifier: F) -> anyhow::Result<bool>
    where
        F: Fn(String, String) -> bool + Send + 'static,
    {
        let signature_opt = match &self.signature {
            Some(s) => s.clone(),
            None => return Ok(false),
        };

        // spawn_blocking en caso de operaciones CPU-bound (HMAC, RSA verify, etc.)
        let sig = signature_opt;
        let payload = canonical_payload;
        let res = tokio::task::spawn_blocking(move || verifier(sig, payload)).await?;
        Ok(res)
    }

    /// Serializa la alerta a JSON (útil para encolado o almacenamiento).
    pub fn to_json(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string(&self)?)
    }

    /// Produce una versión de la alerta apta para logging (sensible a redacciones).
    /// Redacta campos sensibles como `signature` y ciertos valores en `payload` si contienen claves
    /// indicadas en `sensitive_keys`.
    pub fn redact_for_logs(&self, sensitive_keys: &[&str]) -> JsonValue {
        // clonamos superficialmente y procesamos
        let mut value = serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({"error":"serialization_failed"}));

        // redact signature
        if let Some(obj) = value.as_object_mut() {
            if obj.contains_key("signature") {
                obj.insert("signature".to_string(), serde_json::json!("[REDACTED]"));
            }

            // payload redaction heuristic: si payload es objeto, reemplazar claves sensibles
            if let Some(payload) = obj.get_mut("payload") {
                if let Some(pmap) = payload.as_object_mut() {
                    for &k in sensitive_keys {
                        if pmap.contains_key(k) {
                            pmap.insert(k.to_string(), serde_json::json!("[REDACTED]"));
                        }
                    }
                }
            }
        }

        value
    }

    /// Simple helper to redact sensitive bits in error messages before persisting/logging.
    fn redact_error(err: &str) -> String {
        // Placeholder: en producción aplicar heurísticas / regex para eliminar tokens, credenciales.
        // Aquí devolvemos un acortamiento para evitar fugas.
        if err.len() > 200 {
            format!("{}...[truncated]", &err[..200])
        } else {
            err.to_string()
        }
    }
}

impl Default for Alert {
    fn default() -> Self {
        Alert::new("unknown", Severity::Info, serde_json::json!({}))
    }
}
