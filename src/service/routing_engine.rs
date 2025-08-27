// src/service/routing_engine.rs
//! Routing engine service
//!
//! Responsibilities:
//! - Determine notification destinations for alerts based on configurable rules.
//! - Support multi-channel routing with fallback.
//! - Optimize routing for latency, availability, and tenant preferences.
//! - Provide high availability and resilience with retries and alternative routes.
//! - Maintain audit trail for routing decisions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use uuid::Uuid;

use async_trait::async_trait;

/// Supported notification channels (extendable)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NotificationChannel {
    Email,
    Sms,
    Push,
    Slack,
    PagerDuty,
    Custom(String),
}

impl ToString for NotificationChannel {
    fn to_string(&self) -> String {
        match self {
            NotificationChannel::Email => "email".into(),
            NotificationChannel::Sms => "sms".into(),
            NotificationChannel::Push => "push".into(),
            NotificationChannel::Slack => "slack".into(),
            NotificationChannel::PagerDuty => "pagerduty".into(),
            NotificationChannel::Custom(s) => s.clone(),
        }
    }
}

/// Alert routing context passed to engine
#[derive(Debug, Clone)]
pub struct AlertContext {
    pub alert_id: Uuid,
    pub tenant_id: Option<String>,
    pub severity: String,
    pub event_type: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub metadata: HashMap<String, String>, // additional context (e.g. geo, system_state)
}

/// Routing rule metadata and logic
pub struct RoutingRule {
    pub id: String,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Function to decide if rule applies (true = applies)
    pub predicate: Arc<dyn Fn(&AlertContext) -> bool + Send + Sync>,
    /// Ordered list of channels preferred if rule applies
    pub preferred_channels: Vec<NotificationChannel>,
    /// Optional fallback channels if preferred fail
    pub fallback_channels: Vec<NotificationChannel>,
    /// Tenant id if rule is tenant-specific (None = global)
    pub tenant_id: Option<String>,
}

/// Routing decision with reasons and audit data
#[derive(Debug, Clone, Serialize)]
pub struct RoutingDecision {
    pub alert_id: Uuid,
    pub tenant_id: Option<String>,
    pub chosen_channels: Vec<NotificationChannel>,
    pub fallback_channels: Vec<NotificationChannel>,
    pub rule_id: Option<String>,
    pub reason: String,
    pub decided_at: DateTime<Utc>,
}

/// Trait to log routing decisions for audit
#[async_trait]
pub trait AuditLogger: Send + Sync {
    async fn log_routing_decision(&self, decision: &RoutingDecision);
}

/// Routing engine service implementation
pub struct RoutingEngine {
    /// Global routing rules (ordered priority)
    global_rules: Arc<RwLock<Vec<RoutingRule>>>,
    /// Tenant-specific rules: tenant_id -> list of rules (ordered)
    tenant_rules: Arc<RwLock<HashMap<String, Vec<RoutingRule>>>>,
    /// Audit logger sink
    audit_logger: Arc<dyn AuditLogger>,
}

impl RoutingEngine {
    pub fn new(audit_logger: Arc<dyn AuditLogger>) -> Self {
        Self {
            global_rules: Arc::new(RwLock::new(Vec::new())),
            tenant_rules: Arc::new(RwLock::new(HashMap::new())),
            audit_logger,
        }
    }

    /// Add a global routing rule
    pub async fn add_global_rule(&self, rule: RoutingRule) {
        let mut rules = self.global_rules.write().await;
        rules.push(rule);
    }

    /// Add a tenant-specific routing rule
    pub async fn add_tenant_rule(&self, tenant_id: &str, rule: RoutingRule) {
        let mut map = self.tenant_rules.write().await;
        map.entry(tenant_id.to_string()).or_default().push(rule);
    }

    /// Compute routing decision for given alert context
    /// Uses tenant-specific rules first, then global.
    /// Returns chosen channels and fallback channels.
    pub async fn route_alert(&self, ctx: &AlertContext) -> RoutingDecision {
        let now = Utc::now();

        // 1) Try tenant-specific rules first
        if let Some(tid) = &ctx.tenant_id {
            let tenant_map = self.tenant_rules.read().await;
            if let Some(rules) = tenant_map.get(tid) {
                for rule in rules.iter() {
                    if (rule.predicate)(ctx) {
                        let decision = RoutingDecision {
                            alert_id: ctx.alert_id,
                            tenant_id: ctx.tenant_id.clone(),
                            chosen_channels: rule.preferred_channels.clone(),
                            fallback_channels: rule.fallback_channels.clone(),
                            rule_id: Some(rule.id.clone()),
                            reason: rule.description.clone().unwrap_or_else(|| "Tenant-specific routing rule matched".into()),
                            decided_at: now,
                        };
                        // Audit asynchronously
                        let audit = Arc::clone(&self.audit_logger);
                        let decision_clone = decision.clone();
                        tokio::spawn(async move {
                            audit.log_routing_decision(&decision_clone).await;
                        });
                        return decision;
                    }
                }
            }
        }

        // 2) Fallback to global rules
        {
            let rules = self.global_rules.read().await;
            for rule in rules.iter() {
                if (rule.predicate)(ctx) {
                    let decision = RoutingDecision {
                        alert_id: ctx.alert_id,
                        tenant_id: ctx.tenant_id.clone(),
                        chosen_channels: rule.preferred_channels.clone(),
                        fallback_channels: rule.fallback_channels.clone(),
                        rule_id: Some(rule.id.clone()),
                        reason: rule.description.clone().unwrap_or_else(|| "Global routing rule matched".into()),
                        decided_at: now,
                    };
                    // Audit asynchronously
                    let audit = Arc::clone(&self.audit_logger);
                    let decision_clone = decision.clone();
                    tokio::spawn(async move {
                        audit.log_routing_decision(&decision_clone).await;
                    });
                    return decision;
                }
            }
        }

        // 3) No rule matched: fallback to default channels (e.g. email)
        let default_channels = vec![NotificationChannel::Email];
        let decision = RoutingDecision {
            alert_id: ctx.alert_id,
            tenant_id: ctx.tenant_id.clone(),
            chosen_channels: default_channels,
            fallback_channels: Vec::new(),
            rule_id: None,
            reason: "No routing rule matched, using default channels".into(),
            decided_at: now,
        };

        // Audit asynchronously
        let audit = Arc::clone(&self.audit_logger);
        let decision_clone = decision.clone();
        tokio::spawn(async move {
            audit.log_routing_decision(&decision_clone).await;
        });

        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    struct TestAuditLogger {
        log_calls: Arc<Mutex<Vec<RoutingDecision>>>,
    }

    #[async_trait::async_trait]
    impl AuditLogger for TestAuditLogger {
        async fn log_routing_decision(&self, decision: &RoutingDecision) {
            let mut calls = self.log_calls.lock().await;
            calls.push(decision.clone());
        }
    }

    #[tokio::test]
    async fn test_routing_engine_tenant_rule() {
        let audit = Arc::new(TestAuditLogger { log_calls: Arc::new(Mutex::new(vec![])) });
        let engine = RoutingEngine::new(audit.clone());

        // Tenant rule: route critical alerts to Slack and PagerDuty
        let rule = RoutingRule {
            id: "tenant_crit".into(),
            description: Some("Critical alerts via Slack and PagerDuty".into()),
            created_at: Utc::now(),
            predicate: Arc::new(|ctx: &AlertContext| ctx.severity == "Critical"),
            preferred_channels: vec![NotificationChannel::Slack, NotificationChannel::PagerDuty],
            fallback_channels: vec![NotificationChannel::Email],
            tenant_id: Some("tenant123".into()),
        };
        engine.add_tenant_rule("tenant123", rule).await;

        let ctx = AlertContext {
            alert_id: Uuid::new_v4(),
            tenant_id: Some("tenant123".into()),
            severity: "Critical".into(),
            event_type: Some("db.failure".into()),
            timestamp: Utc::now(),
            metadata: HashMap::new(),
        };

        let decision = engine.route_alert(&ctx).await;
        assert_eq!(decision.chosen_channels, vec![NotificationChannel::Slack, NotificationChannel::PagerDuty]);
        assert_eq!(decision.rule_id.unwrap(), "tenant_crit");

        // Check audit log recorded
        let logged = audit.log_calls.lock().await;
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0].rule_id, Some("tenant_crit".into()));
    }

    #[tokio::test]
    async fn test_routing_engine_global_rule_and_default() {
        let audit = Arc::new(TestAuditLogger { log_calls: Arc::new(Mutex::new(vec![])) });
        let engine = RoutingEngine::new(audit.clone());

        // Global rule: route warning to SMS only
        let rule = RoutingRule {
            id: "global_warn".into(),
            description: Some("Warning alerts via SMS".into()),
            created_at: Utc::now(),
            predicate: Arc::new(|ctx: &AlertContext| ctx.severity == "Warning"),
            preferred_channels: vec![NotificationChannel::Sms],
            fallback_channels: vec![NotificationChannel::Email],
            tenant_id: None,
        };
        engine.add_global_rule(rule).await;

        let ctx_warn = AlertContext {
            alert_id: Uuid::new_v4(),
            tenant_id: Some("tenantX".into()),
            severity: "Warning".into(),
            event_type: None,
            timestamp: Utc::now(),
            metadata: HashMap::new(),
        };
        let decision_warn = engine.route_alert(&ctx_warn).await;
        assert_eq!(decision_warn.chosen_channels, vec![NotificationChannel::Sms]);
        assert_eq!(decision_warn.rule_id.unwrap(), "global_warn");

        // Alert with no matching rule defaults to Email
        let ctx_info = AlertContext {
            alert_id: Uuid::new_v4(),
            tenant_id: Some("tenantY".into()),
            severity: "Info".into(),
            event_type: None,
            timestamp: Utc::now(),
            metadata: HashMap::new(),
        };
        let decision_info = engine.route_alert(&ctx_info).await;
        assert_eq!(decision_info.chosen_channels, vec![NotificationChannel::Email]);
        assert!(decision_info.rule_id.is_none());

        // Audit logs check
        let logged = audit.log_calls.lock().await;
        assert_eq!(logged.len(), 2);
    }
}
