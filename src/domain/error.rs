use chrono::{DateTime, Utc};
use std::fmt;
use thiserror::Error;

/// Nivel de criticidad del error para clasificación operativa
#[derive(Debug, Clone)]
pub enum ErrorImpact {
    Recoverable,   // Puede reintentarse o aplicar fallback
    NonRecoverable // Debe descartarse o enviarse a DLQ
}

/// Información contextual para trazabilidad y métricas
#[derive(Debug, Clone)]
pub struct ErrorContext {
    pub alert_id: Option<String>,
    pub tenant_id: Option<String>,
    pub severity: Option<String>,
    pub channel: Option<String>, // Ej: "email", "slack", "sms"
    pub timestamp: DateTime<Utc>,
}

impl ErrorContext {
    pub fn new(
        alert_id: Option<String>,
        tenant_id: Option<String>,
        severity: Option<String>,
        channel: Option<String>,
    ) -> Self {
        Self {
            alert_id,
            tenant_id,
            severity,
            channel,
            timestamp: Utc::now(),
        }
    }
}

/// Tipos de error específicos del dominio de alerting-service
#[derive(Error, Debug)]
pub enum AlertingError {
    #[error("Error de validación de evento: {message}")]
    EventValidationError {
        message: String,
        context: ErrorContext,
        impact: ErrorImpact,
    },

    #[error("Evento duplicado detectado: {message}")]
    DuplicateEventError {
        message: String,
        context: ErrorContext,
        impact: ErrorImpact,
    },

    #[error("Violación de política del tenant: {message}")]
    TenantPolicyViolationError {
        message: String,
        context: ErrorContext,
        impact: ErrorImpact,
    },

    #[error("Error de ruteo de alerta: {message}")]
    RoutingError {
        message: String,
        context: ErrorContext,
        impact: ErrorImpact,
    },

    #[error("Error de entrega de notificación: {message}")]
    NotificationDeliveryError {
        message: String,
        context: ErrorContext,
        impact: ErrorImpact,
    },

    #[error("Error desconocido: {message}")]
    UnknownError {
        message: String,
        context: ErrorContext,
        impact: ErrorImpact,
    },
}

/// Implementaciones auxiliares para extracción de datos
impl AlertingError {
    pub fn impact(&self) -> &ErrorImpact {
        match self {
            AlertingError::EventValidationError { impact, .. }
            | AlertingError::DuplicateEventError { impact, .. }
            | AlertingError::TenantPolicyViolationError { impact, .. }
            | AlertingError::RoutingError { impact, .. }
            | AlertingError::NotificationDeliveryError { impact, .. }
            | AlertingError::UnknownError { impact, .. } => impact,
        }
    }

    pub fn context(&self) -> &ErrorContext {
        match self {
            AlertingError::EventValidationError { context, .. }
            | AlertingError::DuplicateEventError { context, .. }
            | AlertingError::TenantPolicyViolationError { context, .. }
            | AlertingError::RoutingError { context, .. }
            | AlertingError::NotificationDeliveryError { context, .. }
            | AlertingError::UnknownError { context, .. } => context,
        }
    }
}

/// Ejemplo de uso asincrónico con Tokio
#[tokio::main]
async fn main() {
    let context = ErrorContext::new(
        Some("alert-123".to_string()),
        Some("tenant-456".to_string()),
        Some("critical".to_string()),
        Some("slack".to_string()),
    );

    let err = AlertingError::NotificationDeliveryError {
        message: "Timeout en canal Slack".to_string(),
        context,
        impact: ErrorImpact::Recoverable,
    };

    println!("Ocurrió un error: {}", err);
    println!("Impacto: {:?}", err.impact());
    println!("Contexto: {:?}", err.context());
}
