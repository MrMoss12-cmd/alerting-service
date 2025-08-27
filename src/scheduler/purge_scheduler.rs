use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;
use tokio::time::{interval, Duration as TokioDuration};
use uuid::Uuid;
use log::{info, error};

/// Configuración de políticas de retención por tenant
#[derive(Debug, Clone, Deserialize)]
pub struct RetentionPolicy {
    /// Tiempo en días que se mantienen las alertas
    pub retention_days: i64,
    /// Hora preferida de ejecución (ej. baja carga)
    pub preferred_hour_utc: Option<u8>,
}

/// Representación de una alerta almacenada
#[derive(Debug, Clone)]
pub struct AlertRecord {
    pub id: Uuid,
    pub tenant_id: String,
    pub created_at: DateTime<Utc>,
    pub sensitive_data: bool,
}

/// Simulación de un almacén de alertas
#[async_trait::async_trait]
pub trait AlertStore: Send + Sync {
    async fn fetch_old_alerts(&self, tenant_id: &str, older_than: DateTime<Utc>) -> Vec<AlertRecord>;
    async fn delete_alerts(&self, tenant_id: &str, alert_ids: Vec<Uuid>) -> anyhow::Result<()>;
    async fn audit_purge(&self, tenant_id: &str, deleted_ids: &[Uuid]);
}

/// Programador de purga de alertas viejas
pub struct PurgeScheduler {
    store: Arc<dyn AlertStore>,
    policies: Arc<Mutex<HashMap<String, RetentionPolicy>>>,
}

impl PurgeScheduler {
    pub fn new(store: Arc<dyn AlertStore>) -> Self {
        Self {
            store,
            policies: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Configura la política de retención para un tenant
    pub async fn set_policy(&self, tenant_id: String, policy: RetentionPolicy) {
        self.policies.lock().await.insert(tenant_id, policy);
    }

    /// Inicia la tarea programada de purga
    pub async fn start(self: Arc<Self>) {
        let mut ticker = interval(TokioDuration::from_secs(3600)); // cada hora
        loop {
            ticker.tick().await;
            if let Err(e) = self.run_purge_cycle().await {
                error!("Error en el ciclo de purga: {:?}", e);
            }
        }
    }

    /// Ejecuta un ciclo de purga
    async fn run_purge_cycle(&self) -> anyhow::Result<()> {
        let now = Utc::now();
        let policies_snapshot = self.policies.lock().await.clone();

        for (tenant_id, policy) in policies_snapshot {
            // Si hay hora preferida, validar
            if let Some(hour) = policy.preferred_hour_utc {
                if now.hour() != hour as u32 {
                    continue; // esperar a la hora indicada
                }
            }

            let cutoff_date = now - Duration::days(policy.retention_days);
            let old_alerts = self.store.fetch_old_alerts(&tenant_id, cutoff_date).await;

            if !old_alerts.is_empty() {
                let ids: Vec<Uuid> = old_alerts.iter().map(|a| a.id).collect();

                // Registro de auditoría antes de eliminación
                self.store.audit_purge(&tenant_id, &ids).await;

                // Eliminación en lote optimizada
                self.store.delete_alerts(&tenant_id, ids.clone()).await?;

                info!(
                    "Purgadas {} alertas del tenant {} (anteriores a {})",
                    ids.len(),
                    tenant_id,
                    cutoff_date
                );
            }
        }
        Ok(())
    }
}
