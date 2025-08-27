// src/domain/model/tenant.rs

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use super::severity::Severity;

/// Representa un cliente/organización que recibe alertas.
/// Mantiene políticas y preferencias específicas por tenant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    /// Identificador único y seguro (UUID) para trazabilidad.
    pub id: Uuid,
    /// Nombre legible de la organización o cliente.
    pub name: String,
    /// Configuraciones personalizadas del tenant.
    pub policy: TenantPolicy,
    /// Metadatos adicionales (SLA, límites, contactos, etc.).
    pub metadata: HashMap<String, String>,
}

/// Configuraciones de políticas específicas por tenant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantPolicy {
    /// Canales habilitados (e.g., "email", "sms", "slack").
    pub enabled_channels: Vec<String>,
    /// Nivel mínimo de severidad a notificar.
    pub min_severity: Severity,
    /// Horarios permitidos para notificaciones (formato HH:MM-HH:MM).
    pub allowed_schedules: Vec<String>,
    /// Reglas de fallback por canal.
    /// Ej: {"slack": "email"} → Si falla Slack, enviar por email.
    pub fallback_channels: HashMap<String, String>,
}

impl Tenant {
    /// Crea un nuevo tenant con políticas por defecto.
    pub fn new(name: impl Into<String>) -> Self {
        Tenant {
            id: Uuid::new_v4(),
            name: name.into(),
            policy: TenantPolicy::default(),
            metadata: HashMap::new(),
        }
    }

    /// Verifica si una alerta con la severidad dada debe notificarse según las políticas.
    pub fn should_notify(&self, severity: &Severity) -> bool {
        severity.priority() >= self.policy.min_severity.priority()
    }

    /// Obtiene el canal de fallback para uno dado.
    pub fn get_fallback_channel(&self, channel: &str) -> Option<&String> {
        self.policy.fallback_channels.get(channel)
    }

    /// Agrega o actualiza un metadato del tenant.
    pub fn set_metadata(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.metadata.insert(key.into(), value.into());
    }
}

impl TenantPolicy {
    /// Verifica si el canal está habilitado.
    pub fn is_channel_enabled(&self, channel: &str) -> bool {
        self.enabled_channels.iter().any(|c| c == channel)
    }
}

impl Default for TenantPolicy {
    fn default() -> Self {
        TenantPolicy {
            enabled_channels: vec!["email".into()],
            min_severity: Severity::Warning,
            allowed_schedules: vec!["00:00-23:59".into()], // por defecto todo el día
            fallback_channels: HashMap::new(),
        }
    }
}
