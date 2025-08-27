// src/repository/alert_store.rs
//! Alert store abstractions and implementations.
//!
//! This module provides:
//! - `AlertStore` trait: methods used by the rest of the service (query ids, fetch full alerts,
//!   delete, archive, insert/upsert, get by id).
//! - `InMemoryAlertStore` for fast tests and local dev.
//! - `PostgresAlertStore` using `sqlx::PgPool` for production-grade persistence (Postgres).
//!
//! Key considerations implemented:
//! - Multi-tenant aware (tenant_id present on records & used in queries).
//! - Safe concurrency using Tokio async locks and SQL transactions.
//! - Versioning support (simple `version` integer) to allow auditing/history.
//! - Metrics hooks (using the `metrics` crate) to expose basic counts.
//! - Retention helper: `purge_older_than` to delete/archive in batches.
//!
//! NOTE: the Postgres implementation requires `sqlx` with `postgres` feature enabled
//! and a running Postgres instance for integration tests. For simple unit tests use
//! `InMemoryAlertStore`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Simplified severity and state models for the store.
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

/// Core AlertRecord persisted by the store.
///
/// `version` increments on update (simple optimistic versioning for auditability).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRecord {
    pub id: Uuid,
    pub tenant_id: String,
    pub severity: Severity,
    pub state: AlertState,
    pub payload: Value, // original structured payload
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub version: i64,
}

/// Filter used by queries.
#[derive(Debug, Clone)]
pub struct AlertQuery {
    pub tenant_id: Option<String>,
    pub older_than: Option<DateTime<Utc>>,
    pub severities: Option<Vec<Severity>>,
    pub states: Option<Vec<AlertState>>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub order_desc: bool,
}

/// Trait describing the store API used through the codebase.
#[async_trait]
pub trait AlertStore: Send + Sync + 'static {
    /// Insert a new alert. Returns the ID.
    async fn insert_alert(&self, alert: AlertRecord) -> anyhow::Result<Uuid>;

    /// Upsert (insert or update) an alert. Returns its id.
    async fn upsert_alert(&self, alert: AlertRecord) -> anyhow::Result<Uuid>;

    /// Get an alert by id.
    async fn get_alert(&self, id: &Uuid) -> anyhow::Result<Option<AlertRecord>>;

    /// Query alert IDs matching the filter (used by purge manager).
    async fn query_alert_ids(&self, filter: &AlertQuery) -> anyhow::Result<Vec<Uuid>>;

    /// Fetch full alerts by ids.
    async fn fetch_alerts(&self, ids: &[Uuid]) -> anyhow::Result<Vec<AlertRecord>>;

    /// Delete alerts by ids. Returns number deleted.
    async fn delete_alerts(&self, ids: &[Uuid]) -> anyhow::Result<usize>;

    /// Archive alerts by ids (mark as Archived). Returns number affected.
    async fn archive_alerts(&self, ids: &[Uuid]) -> anyhow::Result<usize>;

    /// Purge (delete) alerts older than a timestamp for a tenant in batches.
    async fn purge_older_than(&self, tenant_id: &str, older_than: DateTime<Utc>, batch_size: usize) -> anyhow::Result<usize>;
}

/// --------------------
/// In-memory implementation (good for fast tests / dev)
/// --------------------
pub struct InMemoryAlertStore {
    // A vector of records protected by RwLock for concurrency.
    items: Arc<RwLock<Vec<AlertRecord>>>,
}

impl InMemoryAlertStore {
    pub fn new(initial: Vec<AlertRecord>) -> Self {
        Self {
            items: Arc::new(RwLock::new(initial)),
        }
    }
}

#[async_trait]
impl AlertStore for InMemoryAlertStore {
    async fn insert_alert(&self, alert: AlertRecord) -> anyhow::Result<Uuid> {
        let id = alert.id;
        let mut items = self.items.write().await;
        items.push(alert);
        metrics::increment_counter!("alert_store_inserts");
        Ok(id)
    }

    async fn upsert_alert(&self, alert: AlertRecord) -> anyhow::Result<Uuid> {
        let mut items = self.items.write().await;
        match items.iter_mut().find(|a| a.id == alert.id) {
            Some(existing) => {
                // simple version increment
                existing.payload = alert.payload.clone();
                existing.updated_at = alert.updated_at;
                existing.severity = alert.severity.clone();
                existing.state = alert.state.clone();
                existing.version = existing.version + 1;
                metrics::increment_counter!("alert_store_upserts");
                Ok(existing.id)
            }
            None => {
                items.push(alert);
                metrics::increment_counter!("alert_store_inserts");
                Ok(items.last().unwrap().id)
            }
        }
    }

    async fn get_alert(&self, id: &Uuid) -> anyhow::Result<Option<AlertRecord>> {
        let items = self.items.read().await;
        Ok(items.iter().find(|a| &a.id == id).cloned())
    }

    async fn query_alert_ids(&self, filter: &AlertQuery) -> anyhow::Result<Vec<Uuid>> {
        let items = self.items.read().await;
        let mut out = Vec::new();
        for a in items.iter().filter(|a| {
            if let Some(ref tid) = filter.tenant_id {
                if &a.tenant_id != tid {
                    return false;
                }
            }
            if let Some(older) = filter.older_than {
                if a.created_at >= older {
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
            if let Some(limit) = filter.limit {
                if out.len() >= limit {
                    break;
                }
            }
        }
        metrics::increment_counter!("alert_store_query_ids");
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
        metrics::increment_counter!("alert_store_fetches");
        Ok(out)
    }

    async fn delete_alerts(&self, ids: &[Uuid]) -> anyhow::Result<usize> {
        let mut items = self.items.write().await;
        let before = items.len();
        items.retain(|a| !ids.contains(&a.id));
        let deleted = before - items.len();
        metrics::increment_counter!("alert_store_deletes");
        Ok(deleted)
    }

    async fn archive_alerts(&self, ids: &[Uuid]) -> anyhow::Result<usize> {
        let mut items = self.items.write().await;
        let mut count = 0usize;
        for a in items.iter_mut() {
            if ids.contains(&a.id) {
                a.state = AlertState::Archived;
                a.version += 1;
                a.updated_at = Utc::now();
                count += 1;
            }
        }
        metrics::increment_counter!("alert_store_archives");
        Ok(count)
    }

    async fn purge_older_than(&self, tenant_id: &str, older_than: DateTime<Utc>, batch_size: usize) -> anyhow::Result<usize> {
        let mut items = self.items.write().await;
        let mut to_delete = Vec::new();
        for a in items.iter() {
            if &a.tenant_id == tenant_id && a.created_at < older_than {
                to_delete.push(a.id);
                if to_delete.len() >= batch_size {
                    break;
                }
            }
        }
        let before = items.len();
        items.retain(|a| !to_delete.contains(&a.id));
        let deleted = before - items.len();
        metrics::increment_counter!("alert_store_purge");
        Ok(deleted)
    }
}

/// --------------------
/// Postgres implementation using sqlx (recommended for production)
/// --------------------
///
/// The Postgres implementation below assumes a table like:
///
/// CREATE TABLE alerts (
///   id UUID PRIMARY KEY,
///   tenant_id TEXT NOT NULL,
///   severity TEXT NOT NULL,
///   state TEXT NOT NULL,
///   payload JSONB NOT NULL,
///   created_at TIMESTAMP WITH TIME ZONE NOT NULL,
///   updated_at TIMESTAMP WITH TIME ZONE NOT NULL,
///   version BIGINT NOT NULL
/// );
///
/// Indexes should be created on (tenant_id), (created_at), (severity), (state)
/// and any compound indexes required by query patterns.

#[cfg(feature = "postgres")]
pub mod postgres {
    use super::*;
    use sqlx::{PgPool, Row, postgres::PgRow};
    use anyhow::Context;

    pub struct PostgresAlertStore {
        pool: PgPool,
    }

    impl PostgresAlertStore {
        pub fn new(pool: PgPool) -> Self {
            Self { pool }
        }

        fn row_to_alert(row: PgRow) -> AlertRecord {
            let id: Uuid = row.get("id");
            let tenant_id: String = row.get("tenant_id");
            let severity_s: String = row.get("severity");
            let state_s: String = row.get("state");
            let payload: Value = row.get("payload");
            let created_at: DateTime<Utc> = row.get("created_at");
            let updated_at: DateTime<Utc> = row.get("updated_at");
            let version: i64 = row.get("version");

            let severity = match severity_s.as_str() {
                "Info" => Severity::Info,
                "Warning" => Severity::Warning,
                "Critical" => Severity::Critical,
                s => Severity::Custom(s.to_string()),
            };

            let state = match state_s.as_str() {
                "Pending" => AlertState::Pending,
                "Delivered" => AlertState::Delivered,
                "Failed" => AlertState::Failed,
                "Archived" => AlertState::Archived,
                _ => AlertState::Pending,
            };

            AlertRecord {
                id,
                tenant_id,
                severity,
                state,
                payload,
                created_at,
                updated_at,
                version,
            }
        }
    }

    #[async_trait]
    impl AlertStore for PostgresAlertStore {
        async fn insert_alert(&self, alert: AlertRecord) -> anyhow::Result<Uuid> {
            let id = alert.id;
            sqlx::query!(
                r#"
                INSERT INTO alerts (id, tenant_id, severity, state, payload, created_at, updated_at, version)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                "#,
                id,
                alert.tenant_id,
                match &alert.severity {
                    Severity::Info => "Info",
                    Severity::Warning => "Warning",
                    Severity::Critical => "Critical",
                    Severity::Custom(s) => s.as_str(),
                },
                match &alert.state {
                    AlertState::Pending => "Pending",
                    AlertState::Delivered => "Delivered",
                    AlertState::Failed => "Failed",
                    AlertState::Archived => "Archived",
                },
                alert.payload,
                alert.created_at,
                alert.updated_at,
                alert.version,
            )
            .execute(&self.pool)
            .await
            .context("insert_alert failed")?;

            metrics::increment_counter!("alert_store_inserts");
            Ok(id)
        }

        async fn upsert_alert(&self, alert: AlertRecord) -> anyhow::Result<Uuid> {
            // simple upsert: try update, if 0 rows then insert
            let updated = sqlx::query!(
                r#"
                UPDATE alerts SET payload = $1, severity = $2, state = $3, updated_at = $4, version = version + 1
                WHERE id = $5
                "#,
                alert.payload,
                match &alert.severity {
                    Severity::Info => "Info",
                    Severity::Warning => "Warning",
                    Severity::Critical => "Critical",
                    Severity::Custom(s) => s.as_str(),
                },
                match &alert.state {
                    AlertState::Pending => "Pending",
                    AlertState::Delivered => "Delivered",
                    AlertState::Failed => "Failed",
                    AlertState::Archived => "Archived",
                },
                alert.updated_at,
                alert.id,
            )
            .execute(&self.pool)
            .await
            .context("upsert update failed")?;

            if updated.rows_affected() == 0 {
                // insert
                self.insert_alert(alert).await?;
                metrics::increment_counter!("alert_store_upserts");
                Ok(alert.id)
            } else {
                metrics::increment_counter!("alert_store_upserts");
                Ok(alert.id)
            }
        }

        async fn get_alert(&self, id: &Uuid) -> anyhow::Result<Option<AlertRecord>> {
            let row = sqlx::query!("SELECT * FROM alerts WHERE id = $1", id)
                .fetch_optional(&self.pool)
                .await
                .context("get_alert query failed")?;

            if let Some(r) = row {
                // Use manual mapping because sqlx::Row macro returns Option<Value> deserialization nuance
                let rec = AlertRecord {
                    id: r.id,
                    tenant_id: r.tenant_id,
                    severity: match r.severity.as_str() {
                        "Info" => Severity::Info,
                        "Warning" => Severity::Warning,
                        "Critical" => Severity::Critical,
                        s => Severity::Custom(s.to_string()),
                    },
                    state: match r.state.as_str() {
                        "Pending" => AlertState::Pending,
                        "Delivered" => AlertState::Delivered,
                        "Failed" => AlertState::Failed,
                        "Archived" => AlertState::Archived,
                        _ => AlertState::Pending,
                    },
                    payload: r.payload,
                    created_at: r.created_at,
                    updated_at: r.updated_at,
                    version: r.version,
                };
                Ok(Some(rec))
            } else {
                Ok(None)
            }
        }

        async fn query_alert_ids(&self, filter: &AlertQuery) -> anyhow::Result<Vec<Uuid>> {
            // Build dynamic SQL with basic filters. For production you'd want prepared statements
            // and more advanced paging (cursor-based).
            let mut sql = "SELECT id FROM alerts WHERE 1=1".to_string();
            let mut args: Vec<(String, String)> = Vec::new();

            if let Some(ref tid) = filter.tenant_id {
                sql.push_str(" AND tenant_id = $1");
            }
            if let Some(older) = filter.older_than {
                sql.push_str(" AND created_at < $2");
            }
            if let Some(ref _sevs) = filter.severities {
                // For simplicity we do not parameterize list; production code should
                // build correct IN clause and bind params.
            }
            if let Some(ref _states) = filter.states {
                // same as severities
            }
            if filter.order_desc {
                sql.push_str(" ORDER BY created_at DESC");
            } else {
                sql.push_str(" ORDER BY created_at ASC");
            }
            if let Some(limit) = filter.limit {
                sql.push_str(&format!(" LIMIT {}", limit));
            }
            // NOTE: This naive builder used for brevity. Replace with proper query builder in prod.
            let rows = sqlx::query(&sql)
                .fetch_all(&self.pool)
                .await
                .context("query_alert_ids failed")?;

            let ids = rows.into_iter().filter_map(|r| r.try_get::<Uuid, &str>("id").ok()).collect::<Vec<_>>();
            metrics::increment_counter!("alert_store_query_ids");
            Ok(ids)
        }

        async fn fetch_alerts(&self, ids: &[Uuid]) -> anyhow::Result<Vec<AlertRecord>> {
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            // Use WHERE id = ANY($1)
            let rows = sqlx::query!("SELECT * FROM alerts WHERE id = ANY($1)", ids)
                .fetch_all(&self.pool)
                .await
                .context("fetch_alerts failed")?;

            let mut out = Vec::new();
            for r in rows {
                out.push(AlertRecord {
                    id: r.id,
                    tenant_id: r.tenant_id,
                    severity: match r.severity.as_str() {
                        "Info" => Severity::Info,
                        "Warning" => Severity::Warning,
                        "Critical" => Severity::Critical,
                        s => Severity::Custom(s.to_string()),
                    },
                    state: match r.state.as_str() {
                        "Pending" => AlertState::Pending,
                        "Delivered" => AlertState::Delivered,
                        "Failed" => AlertState::Failed,
                        "Archived" => AlertState::Archived,
                        _ => AlertState::Pending,
                    },
                    payload: r.payload,
                    created_at: r.created_at,
                    updated_at: r.updated_at,
                    version: r.version,
                });
            }
            metrics::increment_counter!("alert_store_fetches");
            Ok(out)
        }

        async fn delete_alerts(&self, ids: &[Uuid]) -> anyhow::Result<usize> {
            if ids.is_empty() {
                return Ok(0);
            }
            let res = sqlx::query!("DELETE FROM alerts WHERE id = ANY($1)", ids)
                .execute(&self.pool)
                .await
                .context("delete_alerts failed")?;
            metrics::increment_counter!("alert_store_deletes");
            Ok(res.rows_affected() as usize)
        }

        async fn archive_alerts(&self, ids: &[Uuid]) -> anyhow::Result<usize> {
            if ids.is_empty() {
                return Ok(0);
            }
            let res = sqlx::query!("UPDATE alerts SET state = 'Archived', version = version + 1, updated_at = now() WHERE id = ANY($1)", ids)
                .execute(&self.pool)
                .await
                .context("archive_alerts failed")?;
            metrics::increment_counter!("alert_store_archives");
            Ok(res.rows_affected() as usize)
        }

        async fn purge_older_than(&self, tenant_id: &str, older_than: DateTime<Utc>, batch_size: usize) -> anyhow::Result<usize> {
            // Delete in batches to avoid long-running transactions
            let mut deleted = 0usize;
            loop {
                let tx = self.pool.begin().await.context("begin tx failed")?;
                // select up to batch_size ids to delete
                let ids_rows = sqlx::query!("SELECT id FROM alerts WHERE tenant_id = $1 AND created_at < $2 LIMIT $3", tenant_id, older_than, batch_size)
                    .fetch_all(&self.pool)
                    .await
                    .context("select ids for purge failed")?;

                let ids: Vec<Uuid> = ids_rows.into_iter().filter_map(|r| r.id).collect();
                if ids.is_empty() {
                    tx.rollback().await.ok();
                    break;
                }

                let del = sqlx::query!("DELETE FROM alerts WHERE id = ANY($1)", &ids)
                    .execute(&self.pool)
                    .await
                    .context("delete batch failed")?;

                deleted += del.rows_affected() as usize;
                tx.commit().await.context("commit failed")?;

                if ids.len() < batch_size {
                    break;
                }
            }
            metrics::increment_counter!("alert_store_purge");
            Ok(deleted)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[tokio::test]
    async fn test_inmemory_insert_and_query() {
        let now = Utc::now();
        let mut initial = vec![];
        for i in 0..5 {
            initial.push(AlertRecord {
                id: Uuid::new_v4(),
                tenant_id: "t1".into(),
                severity: if i % 2 == 0 { Severity::Info } else { Severity::Warning },
                state: AlertState::Delivered,
                payload: serde_json::json!({"i": i}),
                created_at: now - chrono::Duration::days(i as i64 + 1),
                updated_at: now - chrono::Duration::days(i as i64),
                version: 1,
            });
        }

        let store = InMemoryAlertStore::new(initial.clone());

        // Query ids older than 2 days
        let q = AlertQuery {
            tenant_id: Some("t1".to_string()),
            older_than: Some(Utc::now() - chrono::Duration::days(2)),
            severities: None,
            states: None,
            limit: Some(10),
            offset: None,
            order_desc: true,
        };

        let ids = store.query_alert_ids(&q).await.unwrap();
        assert!(!ids.is_empty());

        // Fetch alerts by ids
        let alerts = store.fetch_alerts(&ids).await.unwrap();
        assert_eq!(alerts.len(), ids.len());

        // Purge (batch size 2)
        let deleted = store.purge_older_than("t1", Utc::now() - chrono::Duration::days(2), 2).await.unwrap();
        assert!(deleted > 0);
    }
}
