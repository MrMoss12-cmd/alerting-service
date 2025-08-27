use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Canales soportados
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NotificationChannel {
    Email,
    Sms,
    Slack,
    PagerDuty,
    Custom(String),
}

/// Nivel de severidad
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SeverityLevel {
    Info,
    Warning,
    Critical,
    Custom(String),
}

/// Definición de horario de notificación (hora local 0-23)
#[derive(Debug, Clone)]
pub struct NotificationSchedule {
    pub start_hour: u8,
    pub end_hour: u8, // Exclusivo
    pub timezone: String, // Ejemplo: "America/Bogota"
}

impl NotificationSchedule {
    /// Validación sencilla del rango horario
    pub fn validate(&self) -> Result<(), String> {
        if self.start_hour > 23 || self.end_hour > 24 {
            return Err("Horas deben estar entre 0 y 24".into());
        }
        Ok(())
    }
}

/// Regla de notificación para un canal específico
#[derive(Debug, Clone)]
pub struct NotificationRule {
    pub channel: NotificationChannel,
    pub min_severity: SeverityLevel,
    pub schedule: Option<NotificationSchedule>,
    pub fallback_channels: Vec<NotificationChannel>, // Canales fallback en orden
}

/// Registro con metadatos para auditoría
#[derive(Debug, Clone)]
pub struct RuleRecord {
    pub id: Uuid,
    pub tenant_id: String,
    pub rules: Vec<NotificationRule>,
    pub version: u64,
    pub updated_by: String,
    pub updated_at: DateTime<Utc>,
}

/// Repositorio en memoria con soporte concurrente para reglas
#[derive(Debug, Default)]
pub struct NotificationRuleStore {
    // Mapa tenant_id => versión más reciente y reglas
    store: Arc<RwLock<HashMap<String, RuleRecord>>>,
}

impl NotificationRuleStore {
    pub fn new() -> Self {
        Self {
            store: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Registra o actualiza reglas de un tenant, validando consistencia
    pub async fn register_rules(
        &self,
        tenant_id: &str,
        new_rules: Vec<NotificationRule>,
        updated_by: &str,
    ) -> Result<RuleRecord, String> {
        // Validar reglas
        for rule in &new_rules {
            if let Some(schedule) = &rule.schedule {
                schedule.validate()?;
            }
        }

        // Validar conflictos básicos (ejemplo: canales duplicados)
        let mut channels_seen = std::collections::HashSet::new();
        for rule in &new_rules {
            if !channels_seen.insert(&rule.channel) {
                return Err(format!(
                    "Canal duplicado en reglas: {:?}",
                    rule.channel.to_string()
                ));
            }
        }

        // Simular control de versiones y persistencia
        let mut store = self.store.write().await;

        let new_version = match store.get(tenant_id) {
            Some(existing) => existing.version + 1,
            None => 1,
        };

        let record = RuleRecord {
            id: Uuid::new_v4(),
            tenant_id: tenant_id.to_string(),
            rules: new_rules,
            version: new_version,
            updated_by: updated_by.to_string(),
            updated_at: Utc::now(),
        };

        store.insert(tenant_id.to_string(), record.clone());

        Ok(record)
    }

    /// Obtener reglas actuales para un tenant
    pub async fn get_rules(&self, tenant_id: &str) -> Option<RuleRecord> {
        let store = self.store.read().await;
        store.get(tenant_id).cloned()
    }
}

impl NotificationChannel {
    pub fn to_string(&self) -> String {
        match self {
            NotificationChannel::Email => "Email".into(),
            NotificationChannel::Sms => "SMS".into(),
            NotificationChannel::Slack => "Slack".into(),
            NotificationChannel::PagerDuty => "PagerDuty".into(),
            NotificationChannel::Custom(name) => name.clone(),
        }
    }
}

// Ejemplo de uso
#[tokio::main]
async fn main() -> Result<(), String> {
    let store = NotificationRuleStore::new();

    let rules = vec![
        NotificationRule {
            channel: NotificationChannel::Email,
            min_severity: SeverityLevel::Info,
            schedule: Some(NotificationSchedule {
                start_hour: 8,
                end_hour: 20,
                timezone: "America/Bogota".to_string(),
            }),
            fallback_channels: vec![NotificationChannel::Sms],
        },
        NotificationRule {
            channel: NotificationChannel::Slack,
            min_severity: SeverityLevel::Warning,
            schedule: None,
            fallback_channels: vec![],
        },
    ];

    let result = store
        .register_rules("tenant_123", rules, "admin_user")
        .await?;

    println!(
        "Reglas registradas para tenant {} versión {} por {} a las {}",
        result.tenant_id, result.version, result.updated_by, result.updated_at
    );

    Ok(())
}
