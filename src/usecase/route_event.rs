use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SeverityLevel {
    Info,
    Warning,
    Critical,
    Custom(String),
}

impl ToString for SeverityLevel {
    fn to_string(&self) -> String {
        match self {
            SeverityLevel::Info => "Info".to_string(),
            SeverityLevel::Warning => "Warning".to_string(),
            SeverityLevel::Critical => "Critical".to_string(),
            SeverityLevel::Custom(name) => name.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum NotificationChannel {
    Email,
    Sms,
    Slack,
    PagerDuty,
    Custom(String),
}

impl ToString for NotificationChannel {
    fn to_string(&self) -> String {
        match self {
            NotificationChannel::Email => "Email".to_string(),
            NotificationChannel::Sms => "SMS".to_string(),
            NotificationChannel::Slack => "Slack".to_string(),
            NotificationChannel::PagerDuty => "PagerDuty".to_string(),
            NotificationChannel::Custom(name) => name.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AlertEvent {
    pub id: Uuid,
    pub tenant_id: String,
    pub severity: SeverityLevel,
    pub event_type: String,   // Tipo de evento, ej: "payment.failure"
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct RoutingDecision {
    pub alert_id: Uuid,
    pub tenant_id: String,
    pub routed_channels: Vec<NotificationChannel>,
    pub reason: String,
}

/// Política de ruteo por tenant, severidad y canal
#[derive(Debug, Default)]
pub struct TenantRoutingPolicy {
    /// Canales habilitados y prioridad, p.ej. ["Email", "Slack", "Sms"] en orden preferido
    pub preferred_channels: Vec<NotificationChannel>,
    /// Severidad mínima requerida para cada canal
    pub channel_severity_thresholds: HashMap<NotificationChannel, SeverityLevel>,
    /// Horarios permitidos (ej: 24/7 o franjas horarias) - aquí simplificado
    pub allowed_hours: Option<(u8, u8)>, // (hora_inicio, hora_fin) 0-23
    /// Canales no autorizados para datos sensibles
    pub sensitive_data_blocked_channels: Vec<NotificationChannel>,
}

/// Servicio de enrutamiento con soporte multi-tenant y trazabilidad
#[derive(Debug, Default, Clone)]
pub struct EventRouter {
    policies: Arc<RwLock<HashMap<String, TenantRoutingPolicy>>>,
}

impl EventRouter {
    pub fn new() -> Self {
        Self {
            policies: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn register_policy(&self, tenant_id: &str, policy: TenantRoutingPolicy) {
        let mut policies = self.policies.write().await;
        policies.insert(tenant_id.to_string(), policy);
    }

    pub async fn route_event(
        &self,
        alert: &AlertEvent,
        contains_sensitive_data: bool,
        current_hour: u8,
    ) -> RoutingDecision {
        let policies = self.policies.read().await;

        let policy = match policies.get(&alert.tenant_id) {
            Some(p) => p,
            None => {
                // Política default: todos los canales, sin restricciones
                return RoutingDecision {
                    alert_id: alert.id,
                    tenant_id: alert.tenant_id.clone(),
                    routed_channels: vec![NotificationChannel::Email, NotificationChannel::Slack],
                    reason: "Política default aplicada (sin configuración tenant)".to_string(),
                };
            }
        };

        // Validar horario permitido
        if let Some((start, end)) = policy.allowed_hours {
            if !Self::is_hour_allowed(current_hour, start, end) {
                return RoutingDecision {
                    alert_id: alert.id,
                    tenant_id: alert.tenant_id.clone(),
                    routed_channels: vec![],
                    reason: format!("Hora {} fuera de horario permitido", current_hour),
                };
            }
        }

        // Filtrar canales según severidad y datos sensibles
        let mut allowed_channels = vec![];
        for channel in &policy.preferred_channels {
            // Comprobar bloqueo por datos sensibles
            if contains_sensitive_data && policy.sensitive_data_blocked_channels.contains(channel) {
                continue;
            }

            // Comprobar severidad mínima para canal
            let min_sev = policy
                .channel_severity_thresholds
                .get(channel)
                .unwrap_or(&SeverityLevel::Info);

            if Self::severity_geq(&alert.severity, min_sev) {
                allowed_channels.push(channel.clone());
            }
        }

        // Si no hay canales disponibles, fallback a Email
        let routed_channels = if allowed_channels.is_empty() {
            vec![NotificationChannel::Email]
        } else {
            allowed_channels
        };

        RoutingDecision {
            alert_id: alert.id,
            tenant_id: alert.tenant_id.clone(),
            routed_channels,
            reason: "Enrutamiento aplicado según política tenant".to_string(),
        }
    }

    fn severity_geq(a: &SeverityLevel, b: &SeverityLevel) -> bool {
        fn severity_value(s: &SeverityLevel) -> u8 {
            match s {
                SeverityLevel::Info => 1,
                SeverityLevel::Warning => 2,
                SeverityLevel::Critical => 3,
                SeverityLevel::Custom(_) => 0, // Los custom no tienen orden definido
            }
        }
        severity_value(a) >= severity_value(b)
    }

    fn is_hour_allowed(current: u8, start: u8, end: u8) -> bool {
        if start <= end {
            current >= start && current < end
        } else {
            // Horario que cruza medianoche, ej: 22 a 6
            current >= start || current < end
        }
    }
}

#[tokio::main]
async fn main() {
    let router = EventRouter::new();

    // Política ejemplo para tenant
    let mut channel_thresholds = HashMap::new();
    channel_thresholds.insert(NotificationChannel::Email, SeverityLevel::Info);
    channel_thresholds.insert(NotificationChannel::Slack, SeverityLevel::Warning);
    channel_thresholds.insert(NotificationChannel::PagerDuty, SeverityLevel::Critical);

    let policy = TenantRoutingPolicy {
        preferred_channels: vec![
            NotificationChannel::PagerDuty,
            NotificationChannel::Slack,
            NotificationChannel::Email,
        ],
        channel_severity_thresholds: channel_thresholds,
        allowed_hours: Some((8, 22)), // 8am a 10pm
        sensitive_data_blocked_channels: vec![NotificationChannel::Slack],
    };

    router.register_policy("tenant_abc", policy).await;

    let alert = AlertEvent {
        id: Uuid::new_v4(),
        tenant_id: "tenant_abc".to_string(),
        severity: SeverityLevel::Warning,
        event_type: "payment.failure".to_string(),
        payload: serde_json::json!({"amount": 123}),
    };

    // Hora simulada
    let current_hour = 21;

    let decision = router.route_event(&alert, true, current_hour).await;

    println!(
        "Alert ID: {}, Tenant: {}, Canales: {:?}, Motivo: {}",
        decision.alert_id,
        decision.tenant_id,
        decision
            .routed_channels
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>(),
        decision.reason
    );
}
