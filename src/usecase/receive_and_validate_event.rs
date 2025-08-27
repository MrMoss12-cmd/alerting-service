use chrono::{DateTime, Utc};
use std::{collections::HashSet, sync::Arc};
use tokio::sync::Mutex;
use uuid::Uuid;

// Definición básica de evento entrante
#[derive(Debug, Clone)]
pub struct IncomingEvent {
    pub id: Uuid,
    pub tenant_id: String,
    pub source: String,      // Origen del evento (ej: "payment.service")
    pub timestamp: DateTime<Utc>,
    pub severity: String,    // "info", "warning", "critical"
    pub payload: serde_json::Value,
    pub jwt_token: Option<String>,   // JWT para autenticidad
    pub signature: Option<String>,   // Firma digital alternativa
}

// Resultado de validación
#[derive(Debug)]
pub enum ValidationResult {
    Valid(IncomingEvent),
    Invalid(EventError),
}

// Tipos de error posibles en la validación
#[derive(Debug)]
pub enum EventError {
    AuthenticationFailed(String),  // JWT o firma inválida
    SchemaInvalid(String),          // Campos faltantes o inválidos
    DuplicateEvent(String),         // Evento ya procesado
    BusinessRuleViolation(String),  // Reglas de negocio no cumplidas
}

// Define si error es recuperable o no
impl EventError {
    pub fn is_recoverable(&self) -> bool {
        match self {
            EventError::AuthenticationFailed(_) => false, // no reintentar
            EventError::SchemaInvalid(_) => false,
            EventError::DuplicateEvent(_) => true, // deduplicación, puede ignorar
            EventError::BusinessRuleViolation(_) => false,
        }
    }
}

/// Estado global simple para simular deduplicación (en memoria)
#[derive(Clone, Default)]
pub struct DeduplicationStore {
    processed_ids: Arc<Mutex<HashSet<Uuid>>>,
}

impl DeduplicationStore {
    pub async fn is_duplicate(&self, id: &Uuid) -> bool {
        let processed = self.processed_ids.lock().await;
        processed.contains(id)
    }

    pub async fn mark_processed(&self, id: Uuid) {
        let mut processed = self.processed_ids.lock().await;
        processed.insert(id);
    }
}

/// Función principal de recepción y validación
pub async fn receive_and_validate_event(
    event: IncomingEvent,
    dedup_store: DeduplicationStore,
) -> ValidationResult {
    // --- Paso 1: Validar autenticidad ---
    if !validate_authenticity(&event).await {
        return ValidationResult::Invalid(EventError::AuthenticationFailed(
            "JWT o firma inválida".to_string(),
        ));
    }

    // --- Paso 2: Validar esquema y semántica ---
    if let Err(e) = validate_schema_and_semantics(&event) {
        return ValidationResult::Invalid(EventError::SchemaInvalid(e));
    }

    // --- Paso 3: Validar reglas de negocio (ejemplo simple) ---
    if !validate_business_rules(&event) {
        return ValidationResult::Invalid(EventError::BusinessRuleViolation(
            "Severidad no permitida para tenant".to_string(),
        ));
    }

    // --- Paso 4: Verificar duplicados ---
    if dedup_store.is_duplicate(&event.id).await {
        return ValidationResult::Invalid(EventError::DuplicateEvent(format!(
            "Evento {} ya procesado",
            event.id
        )));
    }

    // --- Paso 5: Marcar como procesado para evitar duplicados ---
    dedup_store.mark_processed(event.id).await;

    // --- Paso 6: Registrar para auditoría (aquí solo println) ---
    audit_log(&event).await;

    ValidationResult::Valid(event)
}

/// Simulación de validación de autenticidad
async fn validate_authenticity(event: &IncomingEvent) -> bool {
    // Ejemplo simple: debe tener JWT o firma (no valida contenido real)
    event.jwt_token.is_some() || event.signature.is_some()
}

/// Simulación de validación de esquema y datos semánticos
fn validate_schema_and_semantics(event: &IncomingEvent) -> Result<(), String> {
    // Validar campos mínimos
    if event.tenant_id.trim().is_empty() {
        return Err("tenant_id vacío".to_string());
    }

    // Severidad aceptada
    let severities = ["info", "warning", "critical"];
    if !severities.contains(&event.severity.to_lowercase().as_str()) {
        return Err(format!("Severidad desconocida: {}", event.severity));
    }

    // Timestamp no futuro
    if event.timestamp > Utc::now() {
        return Err("Timestamp en el futuro".to_string());
    }

    Ok(())
}

/// Simulación sencilla de reglas de negocio (ejemplo: tenant no acepta severidad baja)
fn validate_business_rules(event: &IncomingEvent) -> bool {
    // Por ejemplo, tenant "tenant_forbid_info" no acepta severidad info
    if event.tenant_id == "tenant_forbid_info" && event.severity.to_lowercase() == "info" {
        return false;
    }
    true
}

/// Simulación de registro para auditoría
async fn audit_log(event: &IncomingEvent) {
    println!(
        "[AUDIT] Evento recibido: id={}, tenant={}, severity={}, source={}, timestamp={}",
        event.id, event.tenant_id, event.severity, event.source, event.timestamp
    );
}

// --- Ejemplo de test manual usando Tokio runtime ---
#[tokio::main]
async fn main() {
    let dedup_store = DeduplicationStore::default();

    let event = IncomingEvent {
        id: Uuid::new_v4(),
        tenant_id: "tenant_123".to_string(),
        source: "payment.service".to_string(),
        timestamp: Utc::now(),
        severity: "critical".to_string(),
        payload: serde_json::json!({"amount": 100, "currency": "USD"}),
        jwt_token: Some("fake_jwt_token".to_string()),
        signature: None,
    };

    match receive_and_validate_event(event.clone(), dedup_store.clone()).await {
        ValidationResult::Valid(ev) => println!("Evento válido: {:?}", ev),
        ValidationResult::Invalid(err) => println!("Error en evento: {:?}, recuperable: {}", err, err.is_recoverable()),
    }

    // Intentar procesar duplicado
    match receive_and_validate_event(event, dedup_store.clone()).await {
        ValidationResult::Valid(ev) => println!("Evento válido: {:?}", ev),
        ValidationResult::Invalid(err) => println!("Error en evento duplicado: {:?}, recuperable: {}", err, err.is_recoverable()),
    }
}
