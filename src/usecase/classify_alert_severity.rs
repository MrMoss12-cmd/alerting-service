use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SeverityLevel {
    Info,
    Warning,
    Critical,
    // Extensible: nuevos niveles se agregan aquí sin romper compatibilidad
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
pub struct AlertEvent {
    pub id: Uuid,
    pub tenant_id: String,
    pub payload: serde_json::Value,
    pub current_severity: Option<SeverityLevel>,
}

#[derive(Debug, Clone)]
pub struct ClassificationResult {
    pub alert_id: Uuid,
    pub tenant_id: String,
    pub assigned_severity: SeverityLevel,
    pub rule_applied: String,
    pub reason: String,
}

/// Reglas de severidad específicas por tenant
/// Por simplicidad aquí las reglas son un mapa de campos JSON con umbrales,
/// por ejemplo: si payload["error_count"] > 10 => Critical
#[derive(Debug, Default)]
pub struct TenantSeverityRules {
    /// Nombre de regla -> función heurística que devuelve Option<SeverityLevel>
    rules: HashMap<String, Box<dyn Fn(&serde_json::Value) -> Option<SeverityLevel> + Send + Sync>>,
}

impl TenantSeverityRules {
    pub fn new() -> Self {
        TenantSeverityRules {
            rules: HashMap::new(),
        }
    }

    pub fn add_rule<F>(&mut self, name: &str, func: F)
    where
        F: Fn(&serde_json::Value) -> Option<SeverityLevel> + Send + Sync + 'static,
    {
        self.rules.insert(name.to_string(), Box::new(func));
    }

    /// Aplica las reglas en orden y devuelve la primera severidad encontrada
    pub fn classify(&self, payload: &serde_json::Value) -> Option<(SeverityLevel, String)> {
        for (name, rule) in &self.rules {
            if let Some(sev) = rule(payload) {
                return Some((sev, name.clone()));
            }
        }
        None
    }
}

/// Servicio de clasificación con soporte multi-tenant y trazabilidad
#[derive(Debug, Default, Clone)]
pub struct SeverityClassifier {
    tenant_rules: Arc<RwLock<HashMap<String, TenantSeverityRules>>>,
}

impl SeverityClassifier {
    pub fn new() -> Self {
        SeverityClassifier {
            tenant_rules: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Registra reglas para un tenant
    pub async fn register_tenant_rules(&self, tenant_id: &str, rules: TenantSeverityRules) {
        let mut map = self.tenant_rules.write().await;
        map.insert(tenant_id.to_string(), rules);
    }

    /// Clasifica la severidad de un evento según reglas tenant específicas,
    /// si no hay reglas aplica una clasificación default basada en un campo "severity"
    pub async fn classify(
        &self,
        alert: &AlertEvent,
    ) -> ClassificationResult {
        let map = self.tenant_rules.read().await;
        if let Some(rules) = map.get(&alert.tenant_id) {
            if let Some((severity, rule_name)) = rules.classify(&alert.payload) {
                return ClassificationResult {
                    alert_id: alert.id,
                    tenant_id: alert.tenant_id.clone(),
                    assigned_severity: severity,
                    rule_applied: rule_name,
                    reason: "Regla tenant aplicada".to_string(),
                };
            }
        }

        // Clasificación default simple: busca campo "severity" en payload
        let default_severity = match alert
            .payload
            .get("severity")
            .and_then(|v| v.as_str())
            .unwrap_or("info")
            .to_lowercase()
            .as_str()
        {
            "critical" => SeverityLevel::Critical,
            "warning" => SeverityLevel::Warning,
            _ => SeverityLevel::Info,
        };

        ClassificationResult {
            alert_id: alert.id,
            tenant_id: alert.tenant_id.clone(),
            assigned_severity: default_severity,
            rule_applied: "Default classification".to_string(),
            reason: "Campo severity en payload o valor por defecto".to_string(),
        }
    }
}

#[tokio::main]
async fn main() {
    let classifier = SeverityClassifier::new();

    // Definir reglas para un tenant específico
    let mut tenant_rules = TenantSeverityRules::new();
    tenant_rules.add_rule("HighErrorCount", |payload| {
        if let Some(error_count) = payload.get("error_count").and_then(|v| v.as_u64()) {
            if error_count > 10 {
                return Some(SeverityLevel::Critical);
            }
            if error_count > 3 {
                return Some(SeverityLevel::Warning);
            }
        }
        None
    });
    classifier.register_tenant_rules("tenant_123", tenant_rules).await;

    // Evento de prueba
    let alert = AlertEvent {
        id: Uuid::new_v4(),
        tenant_id: "tenant_123".to_string(),
        payload: serde_json::json!({
            "error_count": 15,
            "other_data": "test"
        }),
        current_severity: None,
    };

    let result = classifier.classify(&alert).await;

    println!(
        "Alert ID: {}, Tenant: {}, Severidad: {}, Regla aplicada: {}, Razón: {}",
        result.alert_id,
        result.tenant_id,
        result.assigned_severity.to_string(),
        result.rule_applied,
        result.reason
    );
}
