// src/domain/model/severity.rs

use serde::{Deserialize, Serialize};
use std::fmt;

/// Representa el nivel de criticidad de una alerta.
/// Se usa para priorizar, enrutar y decidir canales de notificación.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Severity {
    /// Información general, sin impacto operativo inmediato.
    Info,
    /// Requiere atención pero no interrumpe operaciones críticas.
    Warning,
    /// Impacto alto o riesgo inminente, requiere acción inmediata.
    Critical,
    /// Permite extensiones futuras sin romper integraciones.
    Custom(String),
}

impl Severity {
    /// Obtiene una descripción operativa estándar para cada severidad.
    pub fn description(&self) -> &'static str {
        match self {
            Severity::Info => "Información: sin impacto operativo inmediato.",
            Severity::Warning => "Advertencia: requiere atención preventiva o correctiva.",
            Severity::Critical => "Crítica: acción inmediata requerida.",
            Severity::Custom(_) => "Personalizada: definida por el tenant o política.",
        }
    }

    /// Determina la prioridad numérica asociada a la severidad.
    /// Números más altos = mayor prioridad en la cola.
    pub fn priority(&self) -> u8 {
        match self {
            Severity::Info => 1,
            Severity::Warning => 5,
            Severity::Critical => 10,
            Severity::Custom(_) => 3, // Valor por defecto para personalizadas
        }
    }

    /// Permite mapear a umbrales de SLA de entrega en milisegundos.
    pub fn sla_ms(&self) -> u64 {
        match self {
            Severity::Info => 30_000,      // 30 segundos
            Severity::Warning => 10_000,   // 10 segundos
            Severity::Critical => 2_000,   // 2 segundos
            Severity::Custom(_) => 15_000, // Por defecto para personalizadas
        }
    }

    /// Crea una severidad desde texto, permitiendo personalizadas.
    pub fn from_str_flexible(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "info" => Severity::Info,
            "warning" | "warn" => Severity::Warning,
            "critical" | "crit" => Severity::Critical,
            other => Severity::Custom(other.to_string()),
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Info => write!(f, "INFO"),
            Severity::Warning => write!(f, "WARNING"),
            Severity::Critical => write!(f, "CRITICAL"),
            Severity::Custom(name) => write!(f, "CUSTOM({})", name),
        }
    }
}
