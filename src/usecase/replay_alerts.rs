// src/usecase/replay_alerts.rs
//! Reprocesamiento (replay) de alertas históricas.
//!
//! Este módulo define un `ReplayManager` que permite seleccionar alertas históricas
//! desde un almacén (AlertStore), reaplicarles procesamiento (reclasificar, rutar,
//! despachar) en modo `dry_run` o real, y controlar el volumen/concurrency.
//!
//! Diseño clave:
//! - Selección precisa (filtros por tiempo, severidad, tenant, canal).
//! - Seguridad: `dry_run` por defecto; requiere autorización para enviar notificaciones.
//! - Trazabilidad: cada proceso de replay genera `ReplayRecord` y llama al `AuditSink`.
//! - Control de volumen: límite de concurrencia configurable (Semaphore).
//! - Integración: el procesamiento real se realiza por una función inyectada `process_fn`
//!   que recibe la alerta y un `ReplayContext` (contiene flags como `dry_run`).
//!
//! Nota: este archivo usa Tokio para concurrencia; el almacenamiento y envío real deben
//! implementarse externamente y pasarse por las funciones/traits de integración.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fmt::Debug,
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{Semaphore, RwLock},
    task::JoinHandle,
    time::sleep,
};
use uuid::Uuid;

/// --- Tipos básicos usados por el replay ---

/// Representa una alerta histórica tal y como está almacenada.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRecord {
    pub id: Uuid,
    pub tenant_id: String,
    pub severity: String,
    pub channels_sent: Vec<String>, // Canales a los que se envió originalmente
    pub payload: serde_json::Value,
    pub original_timestamp: DateTime<Utc>,
    /// Metadatos adicionales (p.ej. original delivery status, audit ids)
    pub metadata: serde_json::Value,
}

/// Filtro para seleccionar alertas a replayear.
#[derive(Debug, Clone)]
pub struct ReplayQuery {
    pub tenant_ids: Option<Vec<String>>,
    pub severity_in: Option<Vec<String>>,
    pub channel_in: Option<Vec<String>>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<usize>, // máximo de registros a recuperar
}

/// Opciones de ejecución del replay.
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    pub dry_run: bool,          // Si true, no envía notificaciones reales
    pub max_concurrent: usize,  // paralelismo máximo
    pub skip_if_recent: Option<Duration>, // evitar replays de alertas muy recientes
    pub throttle_ms: Option<u64>, // pausa entre lanzamientos para control de flujo
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            dry_run: true,
            max_concurrent: 10,
            skip_if_recent: Some(Duration::from_secs(60)), // evitar replays de últimos 60s por defecto
            throttle_ms: Some(50),
        }
    }
}

/// Contexto pasado al `process_fn` para que el callee sepa que esto es un replay.
#[derive(Debug, Clone)]
pub struct ReplayContext {
    pub replay_id: Uuid,
    pub started_at: DateTime<Utc>,
    pub dry_run: bool,
    // marcador para auditoría/trace
    pub replay_note: Option<String>,
}

/// Registro de resultado de procesado de una alerta en el replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayResult {
    pub alert_id: Uuid,
    pub success: bool,
    pub error: Option<String>,
    pub processed_at: DateTime<Utc>,
    pub dry_run: bool,
}

/// Interfaz simple para el almacén de alertas históricos.
/// En producción esto debe implementarse usando DB, S3, o similar.
#[async_trait::async_trait]
pub trait AlertStore: Send + Sync + 'static {
    async fn query_alerts(&self, q: ReplayQuery) -> anyhow::Result<Vec<AlertRecord>>;
}

/// Interfaz para sink de auditoría (puede ser el AuditLogger del sistema).
#[async_trait::async_trait]
pub trait AuditSink: Send + Sync + 'static {
    async fn record_replay_event(&self, replay_id: Uuid, alert_id: Uuid, event: &str, metadata: serde_json::Value);
}

/// Tipo de función que procesa una alerta (reaplica reglas y despacha).
/// Debe respetar el flag `dry_run` dentro de `ReplayContext`.
pub type ProcessFn = Arc<
    dyn Fn(AlertRecord, ReplayContext) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>
        + Send
        + Sync,
>;

/// --- ReplayManager ---
pub struct ReplayManager {
    store: Arc<dyn AlertStore>,
    audit: Arc<dyn AuditSink>,
    process_fn: ProcessFn,
    // estado y control
    running_replays: Arc<RwLock<Vec<JoinHandle<()>>>>,
}

impl ReplayManager {
    pub fn new(
        store: Arc<dyn AlertStore>,
        audit: Arc<dyn AuditSink>,
        process_fn: ProcessFn,
    ) -> Self {
        Self {
            store,
            audit,
            process_fn,
            running_replays: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Ejecuta un replay con la query y opciones dadas. Retorna un vector de resultados (summary).
    /// El método lanza tasks en background con límite de concurrencia.
    pub async fn run_replay(&self, query: ReplayQuery, opts: ReplayOptions, note: Option<String>) -> anyhow::Result<Vec<ReplayResult>> {
        // Seguridad: dry_run por defecto. Si dry_run == false, se debe asumir que caller tiene permiso.
        let replay_id = Uuid::new_v4();
        let started_at = Utc::now();

        // 1) recuperar alertas según selección precisa
        let mut alerts = self.store.query_alerts(query.clone()).await?;

        // aplicar limit si viene en opts or in query
        if let Some(limit) = query.limit {
            alerts.truncate(limit);
        }

        // 2) Filtrado extra por skip_if_recent
        if let Some(skip_dur) = opts.skip_if_recent {
            alerts.retain(|a| {
                let age = Utc::now().signed_duration_since(a.original_timestamp).to_std().unwrap_or_default();
                age >= skip_dur
            });
        }

        // 3) Preparar concurrencia
        let semaphore = Arc::new(Semaphore::new(opts.max_concurrent));
        let mut tasks: Vec<JoinHandle<(Uuid, ReplayResult)>> = Vec::new();

        // Shared references
        let audit = Arc::clone(&self.audit);
        let process_fn = Arc::clone(&self.process_fn);

        for alert in alerts.into_iter() {
            let s = Arc::clone(&semaphore);
            let proc = Arc::clone(&process_fn);
            let audit = Arc::clone(&audit);
            let rid = replay_id;
            let started = started_at;
            let note_inner = note.clone();
            let throttle_ms = opts.throttle_ms.unwrap_or(0);
            let dry_run = opts.dry_run;

            // Acquire permit asynchronously at spawn-time to limit number of running tasks
            let permit_fut = s.clone().acquire_owned();

            let handle = tokio::spawn(async move {
                // wait permit
                let _permit = permit_fut.await.expect("semaphore closed");
                // optional throttle to avoid bursts
                if throttle_ms > 0 {
                    sleep(Duration::from_millis(throttle_ms)).await;
                }

                // Mark in audit: replay started for this alert
                let _ = audit.record_replay_event(rid, alert.id, "replay_started", serde_json::json!({
                    "dry_run": dry_run,
                    "note": note_inner,
                    "original_ts": alert.original_timestamp
                })).await;

                // Build context
                let ctx = ReplayContext {
                    replay_id: rid,
                    started_at: started,
                    dry_run,
                    replay_note: note_inner.clone(),
                };

                // Execute processing function (re-aplicar políticas)
                let res = (proc)(alert.clone(), ctx).await;

                let success = res.is_ok();
                let error_str = res.err().map(|e| format!("{:#}", e));

                // audit result
                let _ = audit.record_replay_event(rid, alert.id, "replay_completed", serde_json::json!({
                    "success": success,
                    "error": error_str,
                })).await;

                let result = ReplayResult {
                    alert_id: alert.id,
                    success,
                    error: error_str,
                    processed_at: Utc::now(),
                    dry_run,
                };

                (alert.id, result)
            });

            tasks.push(handle);
        }

        // collect results
        let mut results: Vec<ReplayResult> = Vec::new();
        for t in tasks {
            match t.await {
                Ok((_alert_id, res)) => results.push(res),
                Err(join_err) => {
                    // Task panicked or cancelled: record a failed result placeholder
                    results.push(ReplayResult {
                        alert_id: Uuid::nil(),
                        success: false,
                        error: Some(format!("task_failed: {}", join_err)),
                        processed_at: Utc::now(),
                        dry_run: opts.dry_run,
                    });
                }
            }
        }

        Ok(results)
    }

    /// Retorna número de replays actualmente en flight (approx).
    pub async fn running_count(&self) -> usize {
        let v = self.running_replays.read().await;
        v.len()
    }
}

/// --------------------------
/// Implementaciones de ejemplo (In-memory) para desarrollo / tests
/// --------------------------

/// Simple in-memory AlertStore para demo/testing.
pub struct InMemoryAlertStore {
    records: Arc<RwLock<Vec<AlertRecord>>>,
}

impl InMemoryAlertStore {
    pub fn new(records: Vec<AlertRecord>) -> Self {
        Self {
            records: Arc::new(RwLock::new(records)),
        }
    }
}

#[async_trait::async_trait]
impl AlertStore for InMemoryAlertStore {
    async fn query_alerts(&self, q: ReplayQuery) -> anyhow::Result<Vec<AlertRecord>> {
        let recs = self.records.read().await;
        let mut out: Vec<AlertRecord> = recs.clone()
            .into_iter()
            .filter(|r| {
                if let Some(ref tenants) = q.tenant_ids {
                    if !tenants.contains(&r.tenant_id) {
                        return false;
                    }
                }
                if let Some(ref severities) = q.severity_in {
                    if !severities.iter().any(|s| s.eq_ignore_ascii_case(&r.severity)) {
                        return false;
                    }
                }
                if let Some(ref channels) = q.channel_in {
                    if !r.channels_sent.iter().any(|c| channels.iter().any(|cc| cc.eq_ignore_ascii_case(c))) {
                        return false;
                    }
                }
                if let Some(from) = q.from {
                    if r.original_timestamp < from {
                        return false;
                    }
                }
                if let Some(to) = q.to {
                    if r.original_timestamp > to {
                        return false;
                    }
                }
                true
            })
            .collect();

        if let Some(limit) = q.limit {
            out.truncate(limit);
        }

        Ok(out)
    }
}

/// Simple AuditSink que escribe en stdout (puede ser reemplazado por el AuditLogger real)
pub struct StdoutAuditSink;

#[async_trait::async_trait]
impl AuditSink for StdoutAuditSink {
    async fn record_replay_event(&self, replay_id: Uuid, alert_id: Uuid, event: &str, metadata: serde_json::Value) {
        println!(
            "[AUDIT] replay_id={} alert_id={} event={} meta={}",
            replay_id, alert_id, event, metadata
        );
    }
}

/// --------------------------
/// Ejemplo de uso
/// --------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDur;

    #[tokio::test]
    async fn test_replay_manager_dry_run() {
        // create some sample alerts
        let now = Utc::now();
        let a1 = AlertRecord {
            id: Uuid::new_v4(),
            tenant_id: "tenant-a".into(),
            severity: "critical".into(),
            channels_sent: vec!["email".into()],
            payload: serde_json::json!({"x":1}),
            original_timestamp: now - ChronoDur::minutes(10),
            metadata: serde_json::json!({}),
        };
        let a2 = AlertRecord {
            id: Uuid::new_v4(),
            tenant_id: "tenant-b".into(),
            severity: "warning".into(),
            channels_sent: vec!["slack".into()],
            payload: serde_json::json!({"y":2}),
            original_timestamp: now - ChronoDur::minutes(30),
            metadata: serde_json::json!({}),
        };

        let store = Arc::new(InMemoryAlertStore::new(vec![a1.clone(), a2.clone()]));
        let audit = Arc::new(StdoutAuditSink);

        // process_fn: in dry_run we should not send; just log and return Ok
        let process_fn: ProcessFn = Arc::new(move |alert: AlertRecord, ctx: ReplayContext| {
            Box::pin(async move {
                println!("Processing alert {} dry_run={}", alert.id, ctx.dry_run);
                // if not dry_run, in real impl you'd call classifier/router/dispatcher here
                if ctx.dry_run {
                    Ok(())
                } else {
                    // Simulate sending but for tests just ok
                    Ok(())
                }
            })
        });

        let mgr = ReplayManager::new(store, audit, process_fn);

        let q = ReplayQuery {
            tenant_ids: None,
            severity_in: Some(vec!["critical".into()]),
            channel_in: None,
            from: None,
            to: None,
            limit: None,
        };

        let opts = ReplayOptions {
            dry_run: true,
            max_concurrent: 2,
            skip_if_recent: None,
            throttle_ms: Some(1),
        };

        let res = mgr.run_replay(q, opts, Some("test_dry".into())).await.unwrap();
        assert_eq!(res.len(), 1);
        assert!(res[0].dry_run);
        assert!(res[0].success);
    }
}
