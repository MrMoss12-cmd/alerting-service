// src/service/audit_logger.rs
//! Audit logger service
//!
//! Responsibilities:
//! - Maintain a comprehensive, tamper-evident audit log of critical actions and events.
//! - Record actor identity, action type, timestamp, and source location.
//! - Ensure data integrity via cryptographic signatures or immutable storage.
//! - Support fast querying and filtering of logs.
//! - Enforce configurable retention policies complying with regulations like GDPR, HIPAA.
//! - Provide integration/export to external SIEM and monitoring systems.

use chrono::{DateTime, Utc};
use serde::{Serialize, Deserialize};
use tokio::sync::RwLock;
use tracing::{error, info};
use uuid::Uuid;
use std::{collections::VecDeque, sync::Arc};
use anyhow::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuditAction {
    AlertReceived,
    AlertClassified,
    AlertRouted,
    NotificationDispatched,
    NotificationFailed,
    TicketCreated,
    PluginLoaded,
    PluginUnloaded,
    UserLogin,
    UserLogout,
    ConfigChanged,
    // Extendable...
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub actor: String,         // User or system performing the action
    pub action: AuditAction,
    pub description: String,   // Human readable description
    pub source_ip: Option<String>,
    pub alert_id: Option<Uuid>, // Link to alert if applicable
    pub tenant_id: Option<String>,
    pub metadata: Option<serde_json::Value>, // Additional structured data
    pub signature: Option<String>, // Optional cryptographic signature for integrity
}

/// Configurable retention policy for audit logs
#[derive(Debug, Clone)]
pub struct RetentionPolicy {
    pub max_entries: usize,       // Max entries to keep in memory/store
    pub max_age_days: u64,        // Max age (days) before deletion
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            max_age_days: 90,
        }
    }
}

/// Audit logger with in-memory queue and async safe access
pub struct AuditLogger {
    logs: Arc<RwLock<VecDeque<AuditEntry>>>,
    retention: RetentionPolicy,
}

impl AuditLogger {
    pub fn new(retention: RetentionPolicy) -> Self {
        Self {
            logs: Arc::new(RwLock::new(VecDeque::with_capacity(retention.max_entries))),
            retention,
        }
    }

    /// Append a new audit entry
    pub async fn log(&self, entry: AuditEntry) -> Result<()> {
        // Validate entry (e.g. mandatory fields)
        if entry.actor.is_empty() {
            anyhow::bail!("Audit entry missing actor");
        }
        if entry.description.is_empty() {
            anyhow::bail!("Audit entry missing description");
        }

        // TODO: Optionally sign the entry cryptographically here

        let mut logs_guard = self.logs.write().await;
        logs_guard.push_back(entry);

        // Enforce retention policy (by count)
        while logs_guard.len() > self.retention.max_entries {
            logs_guard.pop_front();
        }

        Ok(())
    }

    /// Query audit logs by tenant, action, time range, etc.
    pub async fn query(&self,
        tenant_id: Option<&str>,
        action: Option<AuditAction>,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>
    ) -> Vec<AuditEntry> {
        let logs_guard = self.logs.read().await;
        logs_guard.iter()
            .filter(|entry| {
                if let Some(tid) = tenant_id {
                    if entry.tenant_id.as_deref() != Some(tid) {
                        return false;
                    }
                }
                if let Some(ref act) = action {
                    if &entry.action != act {
                        return false;
                    }
                }
                if let Some(f) = from {
                    if entry.timestamp < f {
                        return false;
                    }
                }
                if let Some(t) = to {
                    if entry.timestamp > t {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect()
    }

    /// Periodic cleanup of old entries based on retention max_age_days
    pub async fn cleanup(&self) {
        let cutoff = Utc::now() - chrono::Duration::days(self.retention.max_age_days as i64);
        let mut logs_guard = self.logs.write().await;

        logs_guard.retain(|entry| entry.timestamp > cutoff);
    }

    /// Export audit logs in JSON format for SIEM or monitoring ingestion
    pub async fn export_json(&self) -> Result<String> {
        let logs_guard = self.logs.read().await;
        let json = serde_json::to_string_pretty(&*logs_guard)?;
        Ok(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[tokio::test]
    async fn test_audit_log_append_and_query() {
        let logger = AuditLogger::new(RetentionPolicy::default());
        let now = Utc::now();

        let entry = AuditEntry {
            id: Uuid::new_v4(),
            timestamp: now,
            actor: "test_user".to_string(),
            action: AuditAction::AlertReceived,
            description: "Received alert from system".to_string(),
            source_ip: Some("127.0.0.1".to_string()),
            alert_id: None,
            tenant_id: Some("tenant_123".to_string()),
            metadata: None,
            signature: None,
        };

        logger.log(entry.clone()).await.unwrap();

        let results = logger.query(Some("tenant_123"), Some(AuditAction::AlertReceived), None, None).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].actor, "test_user");

        // Test cleanup removes old entries
        logger.cleanup().await;
    }
}
