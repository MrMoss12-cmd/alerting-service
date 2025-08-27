use crate::domain::model::alert::{Alert, Severity};
use crate::domain::model::tenant::TenantId;
use chrono::Utc;
use uuid::Uuid;

pub fn simulate_alert(tenant_id: TenantId) -> Alert {
    Alert {
        id: Uuid::new_v4(),
        tenant_id,
        title: "🔧 Simulated Alert".into(),
        description: "This is a test alert triggered manually.".into(),
        severity: Severity::Warning,
        timestamp: Utc::now(),
        metadata: Some("simulation=true".into()),
        source: "simulation-engine".into(),
    }
}
