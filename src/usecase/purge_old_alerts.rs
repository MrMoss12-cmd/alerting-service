// src/usecase/purge_old_alerts.rs
//! Purga de alertas antiguas (Retention / GC).
//!
//! Características implementadas:
//! - Políticas configurables por tenant (edad, severidad, estado).
//! - Operación en batch/incremental con límite de concurrencia.
//! - Backup opcional antes de borrar (sink inyectable).
//! - Dry-run para simular purga sin eliminar datos.
//! - Auditoría de acciones (AuditSink) con motivo y summary.
//! - Seguridad: requiere confirmación explícita (force) para ejecuciones "live".
//! - Multi-tenant: respeta políticas aisladas por tenant.
//! - Compatibilidad con compliance: permite marcar que items se archivan en lugar de borrar.
//!
//! Nota: El AlertStore es un trait inyectable que debe implementar la capacidad de
//! consultar IDs y eliminar por lotes de forma eficiente (server-side deletes preferible).
//! En demo incluimos un `InMemoryAlertStore` simple.

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt::Debug,
    sync::Arc,
    time::Duration as StdDuration,
};
use tokio::{sync::Semaphore, time::sleep};
use uuid::Uuid;

/// Severidad y estado representativos (simplificados)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Info,
    Warning,
    Critical,
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlertState {
    Pending,
    Delivered,
    Failed,
    Archived,
}

/// Filtro de selección para purga
#[derive(Debug, Clone)]
pub struct PurgeFilter {
    pub tenant_id: Option<String>,
    pub older_than: Option<DateTime<Utc>>, // borrar alerts older than this timestamp
    pub severities: Option<Vec<Severity>>,
    pub states: Option<Vec<AlertState>>,
    pub limit: Option<usize>, // max items to consider in this batch
}

/// Política de retención por tenant
#[derive(Debug, Clone)]
pub struct RetentionPolicy {
    pub tenant_id: String,
    pub retain_days: i64, // edad mínima (days) para borrar; 0 = borrar todo viejo
    pub except_severities: Vec<Severity>, // severidades que no se borran
    pub except_states: Vec<AlertState>, // estados que no se borran
    pub archive_instead_of_delete: bool, // si true -> archive/backup, no delete (compliance)
}

impl RetentionPolicy {
    pub fn default_for_tenant(tenant_id: impl Into<String>) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            retain_days: 90,
            except_severities: vec![Severity::Critical],
            except_states: vec![AlertState::Pending],
            archive_instead_of_delete: false,
        }
    }

    /// Produce a PurgeFilter from the policy (batch target)
    pub fn to_filter(&self) -> PurgeFilter {
        PurgeFilter {
            tenant_id: Some(self.tenant_id.clone()),
            older_than: Some(Utc::now() - Duration::days(self.retain_days)),
            severities: None,
            states: None,
            limit: Some(500), // default batch size
        }
    }
}

/// Almacén de alertas: debe permitir buscar ids por filtro y eliminar por ids.
/// En producción esto delegará a DB con operaciones eficientes (DELETE ... WHERE ... LIMIT ...)
#[async_trait::async_trait]
pub trait AlertStore: Send + Sync + 'static {
    /// Query alert IDs matching the filter. Implementations should return stable slices
    /// (e.g. by pagination) to avoid skipping items when deletes occur concurrently.
    async fn query_alert_ids(&self, filter: &PurgeFilter) -> anyhow::Result<Vec<Uuid>>;

    /// Retrieve full alert records by ids (useful for backup/archive).
    async fn fetch_alerts(&self, ids: &[Uuid]) -> anyhow::Result<Vec<AlertRecord>>;

    /// Delete alerts by ids. Return number deleted.
    async fn delete_alerts(&self, ids: &[Uuid]) -> anyhow::Result<usize>;

    /// Mark alerts as archived (if supported). Return number affected.
    async fn archive_alerts(&self, ids: &[Uuid]) -> anyhow::Result<usize>;
}

/// Estructura de alerta simple (para backup / audit)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRecord {
    pub id: Uuid,
    pub tenant_id: String,
    pub severity: Severity,
    pub state: AlertState,
    pub payload: serde_json::Value,
    pub original_timestamp: DateTime<Utc>,
}

/// Backup sink (opcional). Puede guardar en S3, object store o DB.
#[async_trait::async_trait]
pub trait BackupSink: Send + Sync + 'static {
    async fn backup_alerts(&self, tenant_id: &str, alerts: &[AlertRecord]) -> anyhow::Result<()>;
}

/// Audit sink para registrar qué se elimina y por qué.
#[async_trait::async_trait]
pub trait AuditSink: Send + Sync + 'static {
    async fn record_purge_action(&self, actor: &str, tenant_id: &str, deleted: usize, archived: usize, dry_run: bool, reason: &str);
}

/// Resultado resumido por tenant
#[derive(Debug, Serialize)]
pub struct PurgeSummary {
    pub tenant_id: String,
    pub deleted: usize,
    pub archived: usize,
    pub errors: Vec<String>,
}

/// Manager principal de purga
pub struct PurgeManager {
    store: Arc<dyn AlertStore>,
    backup: Option<Arc<dyn BackupSink>>,
    audit: Arc<dyn AuditSink>,
    policies: Arc<tokio::sync::RwLock<HashMap<String, RetentionPolicy>>>,
    concurrency_limit: usize,
    batch_size: usize,
}

impl PurgeManager {
    pub fn new(
        store: Arc<dyn AlertStore>,
        backup: Option<Arc<dyn BackupSink>>,
        audit: Arc<dyn AuditSink>,
        concurrency_limit: usize,
        batch_size: usize,
    ) -> Self {
        Self {
            store,
            backup,
            audit,
            policies: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            concurrency_limit,
            batch_size,
        }
    }

    /// Registra o reemplaza la política para un tenant
    pub async fn set_policy(&self, policy: RetentionPolicy) {
        let mut map = self.policies.write().await;
        map.insert(policy.tenant_id.clone(), policy);
    }

    /// Ejecuta purga para todos los tenants registrados en políticas.
    ///
    /// * `actor` - quién inició la purga (audit).
    /// * `dry_run` - si true no eliminará, solo simula.
    /// * `force` - si true permite que dry_run = false operaciones (safety).
    pub async fn run_purge_all(&self, actor: &str, dry_run: bool, force: bool) -> anyhow::Result<Vec<PurgeSummary>> {
        if !dry_run && !force {
            anyhow::bail!("Refusing to run live purge without force=true");
        }

        let policies = {
            let map = self.policies.read().await;
            map.values().cloned().collect::<Vec<_>>()
        };

        let sem = Arc::new(Semaphore::new(self.concurrency_limit));
        let mut handles = Vec::new();

        for policy in policies.into_iter() {
            let permit = sem.clone().acquire_owned().await.unwrap();
            let mgr = self.clone();
            let actor = actor.to_string();
            let handle = tokio::spawn(async move {
                let res = mgr.run_purge_for_tenant(&actor, policy, dry_run).await;
                drop(permit);
                res
            });
            handles.push(handle);
        }

        let mut results = Vec::new();
        for h in handles {
            match h.await {
                Ok(Ok(summary)) => results.push(summary),
                Ok(Err(e)) => results.push(PurgeSummary {
                    tenant_id: "unknown".into(),
                    deleted: 0,
                    archived: 0,
                    errors: vec![format!("task_error: {}", e)],
                }),
                Err(join_err) => results.push(PurgeSummary {
                    tenant_id: "unknown".into(),
                    deleted: 0,
                    archived: 0,
                    errors: vec![format!("join_error: {}", join_err)],
                }),
            }
        }

        Ok(results)
    }

    /// Ejecuta purga para un tenant según su política. Retorna un summary.
    async fn run_purge_for_tenant(&self, actor: &str, policy: RetentionPolicy, dry_run: bool) -> anyhow::Result<PurgeSummary> {
        let mut deleted_total = 0usize;
        let mut archived_total = 0usize;
        let mut errors: Vec<String> = Vec::new();

        // Build filter: older than retain_days
        let mut filter = policy.to_filter();
        // override batch size
        filter.limit = Some(self.batch_size);

        loop {
            // query ids to purge in this batch
            let ids = match self.store.query_alert_ids(&filter).await {
                Ok(v) => v,
                Err(e) => {
                    errors.push(format!("query_error: {}", e));
                    break;
                }
            };

            if ids.is_empty() {
                break;
            }

            // fetch full alerts if backup/archive or for audit
            let alerts = match self.store.fetch_alerts(&ids).await {
                Ok(a) => a,
                Err(e) => {
                    errors.push(format!("fetch_error: {}", e));
                    // if we can't fetch, avoid deleting to be safe
                    break;
                }
            };

            // Apply policy-level exclusions (severities/states)
            let (to_delete_ids, to_archive_ids, to_skip) = Self::partition_by_policy(&policy, &alerts);

            // Backup if needed
            if !to_archive_ids.is_empty() || (self.backup.is_some() && !to_delete_ids.is_empty()) {
                if let Some(backup_sink) = &self.backup {
                    if !dry_run {
                        if let Err(e) = backup_sink.backup_alerts(&policy.tenant_id, &alerts).await {
                            errors.push(format!("backup_error: {}", e));
                            // continue but avoid deleting if backup fails
                            break;
                        }
                    } else {
                        // dry-run: record that we WOULD back up
                    }
                }
            }

            // Archive instead of delete if policy requests
            if policy.archive_instead_of_delete {
                if !to_delete_ids.is_empty() {
                    if dry_run {
                        archived_total += to_delete_ids.len();
                    } else {
                        match self.store.archive_alerts(&to_delete_ids).await {
                            Ok(n) => archived_total += n,
                            Err(e) => {
                                errors.push(format!("archive_error: {}", e));
                                break;
                            }
                        }
                    }
                }
            } else {
                // Delete actual deletions
                if !to_delete_ids.is_empty() {
                    if dry_run {
                        deleted_total += to_delete_ids.len();
                    } else {
                        match self.store.delete_alerts(&to_delete_ids).await {
                            Ok(n) => deleted_total += n,
                            Err(e) => {
                                errors.push(format!("delete_error: {}", e));
                                break;
                            }
                        }
                    }
                }
            }

            // mark any explicit archives
            if !to_archive_ids.is_empty() {
                if dry_run {
                    archived_total += to_archive_ids.len();
                } else {
                    match self.store.archive_alerts(&to_archive_ids).await {
                        Ok(n) => archived_total += n,
                        Err(e) => {
                            errors.push(format!("archive_error: {}", e));
                            break;
                        }
                    }
                }
            }

            // If batch returned less than requested, likely done
            if ids.len() < filter.limit.unwrap_or(self.batch_size) {
                break;
            }

            // small sleep between batches to avoid hammering DB
            sleep(StdDuration::from_millis(100)).await;
        }

        // Audit the purge action
        self.audit.record_purge_action(actor, &policy.tenant_id, deleted_total, archived_total, dry_run, "retention_job").await;

        Ok(PurgeSummary {
            tenant_id: policy.tenant_id,
            deleted: deleted_total,
            archived: archived_total,
            errors,
        })
    }

    /// Partition alerts according to policy: returns (to_delete_ids, to_archive_ids, to_skip_ids)
    fn partition_by_policy(policy: &RetentionPolicy, alerts: &[AlertRecord]) -> (Vec<Uuid>, Vec<Uuid>, Vec<Uuid>) {
        let mut to_delete = Vec::new();
        let mut to_archive = Vec::new();
        let mut to_skip = Vec::new();

        for a in alerts.iter() {
            // skip if severity excluded
            if policy.except_severities.iter().any(|s| match s {
                Severity::Custom(c) => match &a.severity {
                    Severity::Custom(ac) => ac == c,
                    _ => false,
                },
                _ => s == &a.severity,
            }) {
                to_skip.push(a.id);
                continue;
            }

            // skip if state excluded
            if policy.except_states.iter().any(|st| st == &a.state) {
                to_skip.push(a.id);
                continue;
            }

            // otherwise candidate for deletion (or archive depending on policy)
            to_delete.push(a.id);
        }

        // If policy requests archive instead of delete, route to_archive same as to_delete
        if policy.archive_instead_of_delete {
            let arch = to_delete.clone();
            to_delete.clear();
            return (Vec::new(), arch, to_skip);
        }

        (to_delete, Vec::new(), to_skip)
    }
}

/// Clone impl for PurgeManager (Arc fields)
impl Clone for PurgeManager {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            backup: self.backup.as_ref().map(Arc::clone),
            audit: Arc::clone(&self.audit),
            policies: Arc::clone(&self.policies),
            concurrency_limit: self.concurrency_limit,
            batch_size: self.batch_size,
        }
    }
}

/// ---------------------------
/// In-memory implementations for demo / testing
/// ---------------------------

/// Simple in-memory AlertStore for examples (not production).
pub struct InMemoryAlertStore {
    items: Arc<tokio::sync::RwLock<Vec<AlertRecord>>>,
}

impl InMemoryAlertStore {
    pub fn new(initial: Vec<AlertRecord>) -> Self {
        Self { items: Arc::new(tokio::sync::RwLock::new(initial)) }
    }
}

#[async_trait::async_trait]
impl AlertStore for InMemoryAlertStore {
    async fn query_alert_ids(&self, filter: &PurgeFilter) -> anyhow::Result<Vec<Uuid>> {
        let items = self.items.read().await;
        let mut out = Vec::new();
        for a in items.iter().filter(|a| {
            if let Some(ref tid) = filter.tenant_id {
                if &a.tenant_id != tid {
                    return false;
                }
            }
            if let Some(older) = filter.older_than {
                if a.original_timestamp >= older {
                    return false;
                }
            }
            if let Some(ref sev_list) = filter.severities {
                if !sev_list.iter().any(|s| s == &a.severity) {
                    return false;
                }
            }
            if let Some(ref states) = filter.states {
                if !states.iter().any(|st| st == &a.state) {
                    return false;
                }
            }
            true
        }) {
            out.push(a.id);
            if out.len() >= filter.limit.unwrap_or(100) {
                break;
            }
        }
        Ok(out)
    }

    async fn fetch_alerts(&self, ids: &[Uuid]) -> anyhow::Result<Vec<AlertRecord>> {
        let items = self.items.read().await;
        let mut out = Vec::new();
        for id in ids {
            if let Some(a) = items.iter().find(|x| &x.id == id) {
                out.push(a.clone());
            }
        }
        Ok(out)
    }

    async fn delete_alerts(&self, ids: &[Uuid]) -> anyhow::Result<usize> {
        let mut items = self.items.write().await;
        let before = items.len();
        items.retain(|a| !ids.contains(&a.id));
        Ok(before - items.len())
    }

    async fn archive_alerts(&self, ids: &[Uuid]) -> anyhow::Result<usize> {
        let mut items = self.items.write().await;
        let mut count = 0usize;
        for a in items.iter_mut() {
            if ids.contains(&a.id) {
                a.state = AlertState::Archived;
                count += 1;
            }
        }
        Ok(count)
    }
}

/// Backup sink demo: prints backup summary (in real-world would upload to S3)
pub struct DemoBackupSink;

#[async_trait::async_trait]
impl BackupSink for DemoBackupSink {
    async fn backup_alerts(&self, tenant_id: &str, alerts: &[AlertRecord]) -> anyhow::Result<()> {
        println!("Backup: tenant={} count={}", tenant_id, alerts.len());
        // simulate latency
        sleep(StdDuration::from_millis(50)).await;
        Ok(())
    }
}

/// Audit sink demo: logs purge events
pub struct DemoAuditSink;

#[async_trait::async_trait]
impl AuditSink for DemoAuditSink {
    async fn record_purge_action(&self, actor: &str, tenant_id: &str, deleted: usize, archived: usize, dry_run: bool, reason: &str) {
        println!(
            "[AUDIT] actor={} tenant={} deleted={} archived={} dry_run={} reason={}",
            actor, tenant_id, deleted, archived, dry_run, reason
        );
    }
}

/// ---------------------------
/// Example / tests
/// ---------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDur;

    #[tokio::test]
    async fn test_purge_manager_dry_run() {
        // create sample alerts: some old, some new
        let now = Utc::now();
        let mut items = vec![];
        for i in 0..10 {
            items.push(AlertRecord {
                id: Uuid::new_v4(),
                tenant_id: "t1".into(),
                severity: if i % 5 == 0 { Severity::Critical } else { Severity::Info },
                state: if i % 3 == 0 { AlertState::Delivered } else { AlertState::Pending },
                payload: serde_json::json!({"i": i}),
                original_timestamp: now - ChronoDur::days(120 + i), // old
            });
        }

        let store = Arc::new(InMemoryAlertStore::new(items));
        let backup = Some(Arc::new(DemoBackupSink) as Arc<dyn BackupSink>);
        let audit = Arc::new(DemoAuditSink);
        let manager = PurgeManager::new(store.clone(), backup, audit, 2, 5);

        // set a policy: retain_days 90, exclude critical
        let mut p = RetentionPolicy::default_for_tenant("t1");
        p.retain_days = 90;
        p.except_severities = vec![Severity::Critical];
        manager.set_policy(p).await;

        // dry run: should count deletable items but not remove
        let res = manager.run_purge_all("tester", true, false).await.unwrap();
        assert_eq!(res.len(), 1);
        assert!(res[0].deleted > 0 || res[0].archived > 0);

        // ensure store still contains items (dry-run)
        let all_ids = store.query_alert_ids(&PurgeFilter {
            tenant_id: Some("t1".into()),
            older_than: Some(Utc::now()),
            severities: None,
            states: None,
            limit: Some(1000),
        }).await.unwrap();
        assert!(all_ids.len() >= 10);
    }

    #[tokio::test]
    async fn test_purge_manager_live_delete() {
        // sample with fewer items
        let now = Utc::now();
        let mut items = vec![];
        for i in 0..6 {
            items.push(AlertRecord {
                id: Uuid::new_v4(),
                tenant_id: "t2".into(),
                severity: Severity::Info,
                state: AlertState::Delivered,
                payload: serde_json::json!({}),
                original_timestamp: now - ChronoDur::days(200),
            });
        }

        let store = Arc::new(InMemoryAlertStore::new(items));
        let audit = Arc::new(DemoAuditSink);
        let manager = PurgeManager::new(store.clone(), None, audit, 1, 3);

        let policy = RetentionPolicy {
            tenant_id: "t2".into(),
            retain_days: 90,
            except_severities: vec![],
            except_states: vec![],
            archive_instead_of_delete: false,
        };
        manager.set_policy(policy).await;

        // run live purge with force=true
        let res = manager.run_purge_all("ops", false, true).await.unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].deleted, 6);
    }
}
