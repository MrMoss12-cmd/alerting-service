// src/service/alert_classifier.rs
//! Alert classifier service
//!
//! Responsibilities:
//! - Assign severity levels and tags to incoming alerts using configurable rules.
//! - Support tenant-specific rules and pluggable ML models.
//! - Provide deterministic, consistent classification across nodes.
//! - Record audit entries describing which rule/model produced the decision.
//!
//! Notes:
//! - This module uses an in-memory, concurrent store for rules and models. In production
//!   you should persist rule definitions and ML model references to a durable store
//!   and ensure model binaries/weights are distributed consistently across instances.
//! - Rules are evaluated in deterministic order: tenant rules (ordered), global rules (ordered),
//!   then ML model (if present), then fallback/default.
//! - AuditSink is an injectable trait that receives classification decisions for tracing/audit.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use uuid::Uuid;

use async_trait::async_trait;

/// Standard severity levels (extensible via Custom)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeverityLevel {
    Info,
    Warning,
    Critical,
    Custom(String),
}

impl ToString for SeverityLevel {
    fn to_string(&self) -> String {
        match self {
            SeverityLevel::Info => "Info".into(),
            SeverityLevel::Warning => "Warning".into(),
            SeverityLevel::Critical => "Critical".into(),
            SeverityLevel::Custom(s) => s.clone(),
        }
    }
}

/// Classification result returned by the classifier
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassificationResult {
    pub alert_id: Uuid,
    pub tenant_id: Option<String>,
    pub assigned_severity: SeverityLevel,
    pub rule_id: Option<String>, // id of rule that matched, if any
    pub model_id: Option<String>, // id of model used (if ML)
    pub reason: String,          // human readable reason / explanation
    pub evaluated_at: DateTime<Utc>,
}

/// Simple alert input accepted by the classifier
#[derive(Debug, Clone)]
pub struct AlertInput {
    pub id: Uuid,
    pub tenant_id: Option<String>,
    pub event_type: Option<String>,
    pub payload: Value, // arbitrary JSON
    pub received_at: DateTime<Utc>,
}

/// Rule function signature: returns Some(severity) if rule matches.
pub type RuleFn = Box<dyn Fn(&AlertInput) -> Option<SeverityLevel> + Send + Sync + 'static>;

/// Rule metadata
#[derive(Debug, Clone)]
pub struct Rule {
    pub id: String,
    pub description: Option<String>,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub rule_fn: RuleFn,
}

/// Trait for ML models (pluggable)
#[async_trait]
pub trait MLModel: Send + Sync + 'static {
    /// Predict severity for an alert input. Return None if model declines to predict.
    async fn predict(&self, input: &AlertInput) -> Option<SeverityLevel>;
    fn id(&self) -> String;
}

/// Audit sink trait to record classification decisions
#[async_trait]
pub trait AuditSink: Send + Sync + 'static {
    async fn record_classification(&self, result: &ClassificationResult);
}

/// Simple tracing audit sink (prints via tracing)
pub struct TracingAuditSink;

#[async_trait]
impl AuditSink for TracingAuditSink {
    async fn record_classification(&self, result: &ClassificationResult) {
        tracing::info!(
            alert_id = %result.alert_id,
            tenant = %result.tenant_id.clone().unwrap_or_else(|| "<none>".into()),
            severity = %result.assigned_severity.to_string(),
            rule = %result.rule_id.clone().unwrap_or_else(|| "<none>".into()),
            model = %result.model_id.clone().unwrap_or_else(|| "<none>".into()),
            reason = %result.reason,
            "classification_recorded"
        );
    }
}

/// Core classifier service
pub struct AlertClassifier {
    /// tenant_id -> ordered list of rules (first match wins)
    tenant_rules: Arc<RwLock<HashMap<String, Vec<Rule>>>>,
    /// global rules applied if tenant rules do not match (ordered)
    global_rules: Arc<RwLock<Vec<Rule>>>,
    /// tenant-specific ML models (optional)
    tenant_models: Arc<RwLock<HashMap<String, Arc<dyn MLModel>>>>,
    /// global ML model fallback
    global_model: Arc<RwLock<Option<Arc<dyn MLModel>>>>,
    /// audit sink
    audit: Arc<dyn AuditSink>,
}

impl AlertClassifier {
    /// Create new classifier with provided audit sink
    pub fn new(audit: Arc<dyn AuditSink>) -> Self {
        Self {
            tenant_rules: Arc::new(RwLock::new(HashMap::new())),
            global_rules: Arc::new(RwLock::new(Vec::new())),
            tenant_models: Arc::new(RwLock::new(HashMap::new())),
            global_model: Arc::new(RwLock::new(None)),
            audit,
        }
    }

    /// Register a rule for a tenant (appended at end -> lower priority)
    pub async fn register_tenant_rule(&self, tenant_id: &str, rule: Rule) {
        let mut map = self.tenant_rules.write().await;
        map.entry(tenant_id.to_string()).or_default().push(rule);
    }

    /// Register a global rule
    pub async fn register_global_rule(&self, rule: Rule) {
        let mut list = self.global_rules.write().await;
        list.push(rule);
    }

    /// Replace all rules for a tenant atomically
    pub async fn set_tenant_rules(&self, tenant_id: &str, rules: Vec<Rule>) {
        let mut map = self.tenant_rules.write().await;
        map.insert(tenant_id.to_string(), rules);
    }

    /// Register tenant ML model
    pub async fn register_tenant_model(&self, tenant_id: &str, model: Arc<dyn MLModel>) {
        let mut m = self.tenant_models.write().await;
        m.insert(tenant_id.to_string(), model);
    }

    /// Set global model
    pub async fn set_global_model(&self, model: Option<Arc<dyn MLModel>>) {
        let mut gm = self.global_model.write().await;
        *gm = model;
    }

    /// Main classify function: deterministic order: tenant rules -> global rules -> tenant model -> global model -> default.
    pub async fn classify(&self, input: &AlertInput) -> ClassificationResult {
        let now = Utc::now();
        let tenant_id = input.tenant_id.clone();

        // 1) Tenant rules
        if let Some(tid) = &tenant_id {
            let rules_map = self.tenant_rules.read().await;
            if let Some(rules) = rules_map.get(tid) {
                for rule in rules.iter() {
                    if let Some(sev) = (rule.rule_fn)(input) {
                        let res = ClassificationResult {
                            alert_id: input.id,
                            tenant_id: tenant_id.clone(),
                            assigned_severity: sev,
                            rule_id: Some(rule.id.clone()),
                            model_id: None,
                            reason: rule.description.clone().unwrap_or_else(|| "tenant rule matched".into()),
                            evaluated_at: now,
                        };
                        // audit asynchronously (don't block classification)
                        let audit = Arc::clone(&self.audit);
                        let res_clone = res.clone();
                        tokio::spawn(async move {
                            audit.record_classification(&res_clone).await;
                        });
                        return res;
                    }
                }
            }
        }

        // 2) Global rules
        {
            let rules = self.global_rules.read().await;
            for rule in rules.iter() {
                if let Some(sev) = (rule.rule_fn)(input) {
                    let res = ClassificationResult {
                        alert_id: input.id,
                        tenant_id: tenant_id.clone(),
                        assigned_severity: sev,
                        rule_id: Some(rule.id.clone()),
                        model_id: None,
                        reason: rule.description.clone().unwrap_or_else(|| "global rule matched".into()),
                        evaluated_at: now,
                    };
                    let audit = Arc::clone(&self.audit);
                    let res_clone = res.clone();
                    tokio::spawn(async move {
                        audit.record_classification(&res_clone).await;
                    });
                    return res;
                }
            }
        }

        // 3) Tenant ML model
        if let Some(tid) = &tenant_id {
            let m = self.tenant_models.read().await;
            if let Some(model) = m.get(tid) {
                if let Some(sev) = model.predict(input).await {
                    let res = ClassificationResult {
                        alert_id: input.id,
                        tenant_id: tenant_id.clone(),
                        assigned_severity: sev,
                        rule_id: None,
                        model_id: Some(model.id()),
                        reason: "tenant ML model prediction".into(),
                        evaluated_at: now,
                    };
                    let audit = Arc::clone(&self.audit);
                    let res_clone = res.clone();
                    tokio::spawn(async move {
                        audit.record_classification(&res_clone).await;
                    });
                    return res;
                }
            }
        }

        // 4) Global ML model
        {
            let gm = self.global_model.read().await;
            if let Some(model) = gm.as_ref() {
                if let Some(sev) = model.predict(input).await {
                    let res = ClassificationResult {
                        alert_id: input.id,
                        tenant_id: tenant_id.clone(),
                        assigned_severity: sev,
                        rule_id: None,
                        model_id: Some(model.id()),
                        reason: "global ML model prediction".into(),
                        evaluated_at: now,
                    };
                    let audit = Arc::clone(&self.audit);
                    let res_clone = res.clone();
                    tokio::spawn(async move {
                        audit.record_classification(&res_clone).await;
                    });
                    return res;
                }
            }
        }

        // 5) Fallback default severity
        let res = ClassificationResult {
            alert_id: input.id,
            tenant_id: tenant_id.clone(),
            assigned_severity: SeverityLevel::Info,
            rule_id: None,
            model_id: None,
            reason: "default fallback".into(),
            evaluated_at: now,
        };
        let audit = Arc::clone(&self.audit);
        let res_clone = res.clone();
        tokio::spawn(async move {
            audit.record_classification(&res_clone).await;
        });
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // Dummy ML model: marks as Critical if payload.error_count > threshold
    struct DummyModel {
        id_str: String,
        threshold: u64,
    }

    #[async_trait]
    impl MLModel for DummyModel {
        async fn predict(&self, input: &AlertInput) -> Option<SeverityLevel> {
            if let Some(count) = input.payload.get("error_count").and_then(|v| v.as_u64()) {
                if count >= self.threshold {
                    return Some(SeverityLevel::Critical);
                } else if count >= (self.threshold / 2) {
                    return Some(SeverityLevel::Warning);
                }
            }
            None
        }
        fn id(&self) -> String {
            self.id_str.clone()
        }
    }

    // Simple audit sink that records calls
    struct CountingAudit {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AuditSink for CountingAudit {
        async fn record_classification(&self, _result: &ClassificationResult) {
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn test_rule_based_classification() {
        let audit = Arc::new(CountingAudit { calls: Arc::new(AtomicUsize::new(0)) });
        let classifier = AlertClassifier::new(audit.clone());

        // Add a global rule: if payload["urgent"] == true -> Critical
        let r = Rule {
            id: "global_urgent".into(),
            description: Some("urgent flag".into()),
            created_by: Some("sys".into()),
            created_at: Utc::now(),
            rule_fn: Box::new(|input: &AlertInput| {
                if input.payload.get("urgent").and_then(|v| v.as_bool()) == Some(true) {
                    Some(SeverityLevel::Critical)
                } else {
                    None
                }
            }),
        };
        classifier.register_global_rule(r).await;

        let alert = AlertInput {
            id: Uuid::new_v4(),
            tenant_id: Some("t1".into()),
            event_type: Some("payment.failure".into()),
            payload: serde_json::json!({"urgent": true}),
            received_at: Utc::now(),
        };

        let res = classifier.classify(&alert).await;
        assert_eq!(res.assigned_severity, SeverityLevel::Critical);
        assert_eq!(res.rule_id.unwrap(), "global_urgent");
        // audit should be recorded asynchronously; give it a tick
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(audit.calls.load(Ordering::Relaxed) >= 1);
    }

    #[tokio::test]
    async fn test_tenant_rules_override_and_ml_fallback() {
        let audit = Arc::new(CountingAudit { calls: Arc::new(AtomicUsize::new(0)) });
        let classifier = AlertClassifier::new(audit.clone());

        // Tenant rule: tenant "vip" upgrades all payment.failure to Critical
        let rule = Rule {
            id: "vip_payment_crit".into(),
            description: Some("VIP tenant escalate payments".into()),
            created_by: Some("ops".into()),
            created_at: Utc::now(),
            rule_fn: Box::new(|input: &AlertInput| {
                if input.tenant_id.as_deref() == Some("vip") && input.event_type.as_deref() == Some("payment.failure") {
                    return Some(SeverityLevel::Critical)
                }
                None
            }),
        };
        classifier.register_tenant_rule("vip", rule).await;

        // Register a tenant ML model for tenant "regular"
        let model = Arc::new(DummyModel { id_str: "model_v1".into(), threshold: 10 });
        classifier.register_tenant_model("regular", model).await;

        // 1) VIP tenant -> tenant rule wins
        let a1 = AlertInput {
            id: Uuid::new_v4(),
            tenant_id: Some("vip".into()),
            event_type: Some("payment.failure".into()),
            payload: serde_json::json!({ "error_count": 1 }),
            received_at: Utc::now(),
        };
        let r1 = classifier.classify(&a1).await;
        assert_eq!(r1.assigned_severity, SeverityLevel::Critical);
        assert_eq!(r1.rule_id.unwrap(), "vip_payment_crit");

        // 2) Regular tenant -> ML model used
        let a2 = AlertInput {
            id: Uuid::new_v4(),
            tenant_id: Some("regular".into()),
            event_type: Some("payment.failure".into()),
            payload: serde_json::json!({ "error_count": 12 }),
            received_at: Utc::now(),
        };
        let r2 = classifier.classify(&a2).await;
        assert_eq!(r2.assigned_severity, SeverityLevel::Critical);
        assert_eq!(r2.model_id.unwrap(), "model_v1");

        // 3) No tenant, no rule, no model -> default Info
        let a3 = AlertInput {
            id: Uuid::new_v4(),
            tenant_id: None,
            event_type: Some("heartbeat".into()),
            payload: serde_json::json!({}),
            received_at: Utc::now(),
        };
        let r3 = classifier.classify(&a3).await;
        assert_eq!(r3.assigned_severity, SeverityLevel::Info);

        // allow some time for async audit writes
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(audit.calls.load(Ordering::Relaxed) >= 3);
    }

    #[tokio::test]
    async fn test_consistency_across_calls() {
        let audit = Arc::new(CountingAudit { calls: Arc::new(AtomicUsize::new(0)) });
        let classifier = AlertClassifier::new(audit.clone());

        // deterministic rule: severity = payload.level if present
        let r = Rule {
            id: "payload_level".into(),
            description: Some("map payload.level to severity".into()),
            created_by: Some("sys".into()),
            created_at: Utc::now(),
            rule_fn: Box::new(|input: &AlertInput| {
                input.payload.get("level").and_then(|v| v.as_str()).map(|s| match s {
                    "critical" => SeverityLevel::Critical,
                    "warning" => SeverityLevel::Warning,
                    _ => SeverityLevel::Info,
                })
            }),
        };
        classifier.register_global_rule(r).await;

        let payload = serde_json::json!({"level":"warning"});
        let a = AlertInput {
            id: Uuid::new_v4(),
            tenant_id: Some("t1".into()),
            event_type: None,
            payload: payload.clone(),
            received_at: Utc::now(),
        };

        let r1 = classifier.classify(&a).await;
        let r2 = classifier.classify(&a).await;
        assert_eq!(r1.assigned_severity, r2.assigned_severity);
    }
}
