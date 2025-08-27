// src/repository/rule_store.rs
//! Rule store: tenant-scoped, versioned, validated rule management.
//!
//! Features:
//! - Accept rules in JSON or YAML (same internal representation).
//! - Simple structured conditions (path, op, value) that are safe to evaluate.
//! - Versioning and rollback per tenant+rule_id.
//! - Tenant isolation and audit hooks.
//! - In-memory implementation (fast, concurrency-safe) suitable for tests/dev.
//!
//! Example rule (JSON/YAML):
//! {
//!   "id": "r1",
//!   "name": "high_error_count",
//!   "description": "Mark as critical if error_count >= 10",
//!   "priority": 10,
//!   "conditions": [
//!     { "path": "payload.error_count", "op": ">=", "value": 10 }
//!   ],
//!   "actions": { "severity": "Critical", "notify": true }
//! }

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;
use tracing::{info, warn};

/// Supported rule formats when registering
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleFormat {
    Json,
    Yaml,
    /// Placeholder for a future DSL
    Dsl,
}

/// Simple operators supported in conditions
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Eq,
    Neq,
    Gt,
    Gte,
    Lt,
    Lte,
    Contains, // for arrays or strings
    Exists,   // check presence only (value ignored)
}

impl Op {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "==" | "eq" => Some(Op::Eq),
            "!=" | "neq" => Some(Op::Neq),
            ">" => Some(Op::Gt),
            ">=" => Some(Op::Gte),
            "<" => Some(Op::Lt),
            "<=" => Some(Op::Lte),
            "contains" => Some(Op::Contains),
            "exists" => Some(Op::Exists),
            _ => None,
        }
    }
}

/// Single condition inside a rule
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleCondition {
    /// Dot-separated path in an alert object, e.g. "payload.error_count" or "metadata.origin"
    pub path: String,
    /// Operator
    pub op: Op,
    /// Value to compare to (ignored for Exists)
    #[serde(default)]
    pub value: Value,
}

/// Actions produced when rule matches (simple structured map)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleActions {
    /// severity to assign (e.g. "Critical", "Warning")
    #[serde(default)]
    pub severity: Option<String>,
    /// whether to notify (boolean)
    #[serde(default)]
    pub notify: Option<bool>,
    /// additional metadata to attach
    #[serde(default)]
    pub metadata: Option<Value>,
}

/// Rule definition (parsed from JSON/YAML)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDefinition {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>, // higher = evaluated earlier
    #[serde(default)]
    pub conditions: Vec<RuleCondition>,
    #[serde(default)]
    pub actions: Option<RuleActions>,
}

/// A versioned rule stored in the system
#[derive(Debug, Clone)]
pub struct RuleVersion {
    pub uid: Uuid, // unique id for this version entry
    pub tenant_id: String,
    pub rule_id: String, // original rule id (logical)
    pub version: i64,    // incrementing
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub definition: RuleDefinition,
    pub active: bool, // whether this version is currently active
}

/// Result of evaluating a rule against an alert
#[derive(Debug, Clone)]
pub struct RuleMatch {
    pub tenant_id: String,
    pub rule_id: String,
    pub version: i64,
    pub actions: Option<RuleActions>,
}

/// Trait for audit sink so store can emit events
#[async_trait]
pub trait AuditSink: Send + Sync + 'static {
    async fn record_rule_change(&self, entry: &RuleVersion, action: &str);
}

/// No-op audit sink (default)
pub struct NoopAudit;
#[async_trait]
impl AuditSink for NoopAudit {
    async fn record_rule_change(&self, _entry: &RuleVersion, _action: &str) {}
}

/// Main store trait
#[async_trait]
pub trait RuleStore: Send + Sync {
    async fn create_rule(&self, tenant_id: &str, created_by: &str, raw: &str, format: RuleFormat) -> anyhow::Result<RuleVersion>;
    async fn update_rule(&self, tenant_id: &str, rule_id: &str, created_by: &str, raw: &str, format: RuleFormat) -> anyhow::Result<RuleVersion>;
    async fn get_active_rules(&self, tenant_id: &str) -> anyhow::Result<Vec<RuleVersion>>;
    async fn get_rule_versions(&self, tenant_id: &str, rule_id: &str) -> anyhow::Result<Vec<RuleVersion>>;
    async fn rollback_rule(&self, tenant_id: &str, rule_id: &str, target_version: i64, performed_by: &str) -> anyhow::Result<RuleVersion>;
    async fn deactivate_rule(&self, tenant_id: &str, rule_id: &str, performed_by: &str) -> anyhow::Result<()>;
    /// Evaluate rules for given tenant and input payload; returns matches ordered by priority and version.
    async fn evaluate(&self, tenant_id: &str, input: &Value) -> anyhow::Result<Vec<RuleMatch>>;
    /// Validate raw rule (syntax + semantics) without storing
    async fn validate_raw_rule(&self, raw: &str, format: RuleFormat) -> anyhow::Result<RuleDefinition>;
}

/// In-memory implementation
pub struct InMemoryRuleStore {
    // key: (tenant_id, rule_id) -> Vec<RuleVersion> (ordered by version asc)
    inner: Arc<RwLock<HashMap<(String, String), Vec<RuleVersion>>>>,
    // active index per tenant: tenant_id -> Vec<(priority, rule_id, active_version)>
    active_index: Arc<RwLock<HashMap<String, Vec<(i64, String, i64)>>>>,
    audit: Arc<dyn AuditSink>,
}

impl InMemoryRuleStore {
    pub fn new(audit: Arc<dyn AuditSink>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            active_index: Arc::new(RwLock::new(HashMap::new())),
            audit,
        }
    }

    /// parse raw rule (JSON or YAML) into RuleDefinition
    fn parse_raw_rule(raw: &str, format: RuleFormat) -> anyhow::Result<RuleDefinition> {
        match format {
            RuleFormat::Json => {
                let def: RuleDefinition = serde_json::from_str(raw)
                    .map_err(|e| anyhow::anyhow!("JSON parse error: {}", e))?;
                Ok(def)
            }
            RuleFormat::Yaml => {
                let def: RuleDefinition = serde_yaml::from_str(raw)
                    .map_err(|e| anyhow::anyhow!("YAML parse error: {}", e))?;
                Ok(def)
            }
            RuleFormat::Dsl => {
                // For now, DSL not supported in this in-memory impl
                Err(anyhow::anyhow!("DSL format not supported yet"))
            }
        }
    }

    /// validate semantics: e.g. non-empty id/name, conditions valid, supported ops
    fn validate_definition(def: &RuleDefinition) -> anyhow::Result<()> {
        if def.id.trim().is_empty() {
            return Err(anyhow::anyhow!("rule id must not be empty"));
        }
        if def.name.trim().is_empty() {
            return Err(anyhow::anyhow!("rule name must not be empty"));
        }
        for c in def.conditions.iter() {
            if c.path.trim().is_empty() {
                return Err(anyhow::anyhow!("condition path must not be empty"));
            }
            // Validate operator (already deserialized)
            // Additional checks: value type compatibility could be done here
            match c.op {
                Op::Eq | Op::Neq | Op::Gt | Op::Gte | Op::Lt | Op::Lte | Op::Contains | Op::Exists => {}
            }
        }
        Ok(())
    }

    /// Evaluate a single condition on a JSON value
    fn eval_condition_on_value(input: &Value, cond: &RuleCondition) -> anyhow::Result<bool> {
        // Navigate path
        let mut cur = input;
        for seg in cond.path.split('.') {
            if seg.is_empty() {
                continue;
            }
            match cur {
                Value::Object(map) => {
                    if let Some(next) = map.get(seg) {
                        cur = next;
                    } else {
                        // missing key
                        cur = &Value::Null;
                        break;
                    }
                }
                _ => {
                    // path cannot be followed
                    cur = &Value::Null;
                    break;
                }
            }
        }

        match cond.op {
            Op::Exists => Ok(!cur.is_null()),
            Op::Eq => Ok(cur == &cond.value),
            Op::Neq => Ok(cur != &cond.value),
            Op::Gt | Op::Gte | Op::Lt | Op::Lte => {
                // Try to compare as numbers or strings
                if let (Some(a), Some(b)) = (cur.as_f64(), cond.value.as_f64()) {
                    let res = match cond.op {
                        Op::Gt => a > b,
                        Op::Gte => a >= b,
                        Op::Lt => a < b,
                        Op::Lte => a <= b,
                        _ => false,
                    };
                    return Ok(res);
                }
                // fallback to string compare
                if let (Some(sa), Some(sb)) = (cur.as_str(), cond.value.as_str()) {
                    let res = match cond.op {
                        Op::Gt => sa > sb,
                        Op::Gte => sa >= sb,
                        Op::Lt => sa < sb,
                        Op::Lte => sa <= sb,
                        _ => false,
                    };
                    return Ok(res);
                }
                Ok(false)
            }
            Op::Contains => {
                // If cur is array, check membership; if string, substring
                if let Some(arr) = cur.as_array() {
                    for item in arr {
                        if item == &cond.value {
                            return Ok(true);
                        }
                    }
                    return Ok(false);
                }
                if let (Some(s), Some(substr)) = (cur.as_str(), cond.value.as_str()) {
                    return Ok(s.contains(substr));
                }
                Ok(false)
            }
        }
    }

    /// Evaluate rule definition against input; returns true if ALL conditions match (AND semantics)
    fn evaluate_definition(def: &RuleDefinition, input: &Value) -> anyhow::Result<bool> {
        for cond in def.conditions.iter() {
            let ok = Self::eval_condition_on_value(input, cond)?;
            if !ok {
                return Ok(false);
            }
        }
        // If no conditions, treat as match=false (or configurable). We choose false.
        Ok(!def.conditions.is_empty())
    }

    /// Rebuild active index for a tenant (should be called after changes)
    async fn rebuild_active_index_for_tenant(&self, tenant_id: &str) {
        let inner = self.inner.read().await;
        let mut index_vec: Vec<(i64, String, i64)> = Vec::new(); // (priority, rule_id, version)
        for ((tid, rid), versions) in inner.iter() {
            if tid != tenant_id {
                continue;
            }
            if let Some(v) = versions.iter().filter(|v| v.active).max_by_key(|v| v.version) {
                let priority = v.definition.priority.unwrap_or(0);
                index_vec.push((priority, rid.clone(), v.version));
            }
        }
        // sort by priority desc, then rule_id
        index_vec.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut idx = self.active_index.write().await;
        idx.insert(tenant_id.to_string(), index_vec);
    }
}

#[async_trait]
impl RuleStore for InMemoryRuleStore {
    async fn create_rule(&self, tenant_id: &str, created_by: &str, raw: &str, format: RuleFormat) -> anyhow::Result<RuleVersion> {
        let def = Self::parse_raw_rule(raw, format)?;
        Self::validate_definition(&def)?;

        let mut inner = self.inner.write().await;
        let key = (tenant_id.to_string(), def.id.clone());
        let versions = inner.entry(key.clone()).or_insert_with(Vec::new);
        let next_version = (versions.last().map(|v| v.version).unwrap_or(0)) + 1;

        let rv = RuleVersion {
            uid: Uuid::new_v4(),
            tenant_id: tenant_id.to_string(),
            rule_id: def.id.clone(),
            version: next_version,
            created_by: created_by.to_string(),
            created_at: Utc::now(),
            definition: def.clone(),
            active: true,
        };

        // Deactivate previous versions of this rule (if any)
        for v in versions.iter_mut() {
            v.active = false;
        }
        versions.push(rv.clone());

        // rebuild index for tenant
        drop(inner);
        self.rebuild_active_index_for_tenant(tenant_id).await;

        // audit
        self.audit.record_rule_change(&rv, "create").await;

        Ok(rv)
    }

    async fn update_rule(&self, tenant_id: &str, rule_id: &str, created_by: &str, raw: &str, format: RuleFormat) -> anyhow::Result<RuleVersion> {
        let def = Self::parse_raw_rule(raw, format)?;
        if def.id != rule_id {
            return Err(anyhow::anyhow!("rule id in payload '{}' does not match targeted '{}'", def.id, rule_id));
        }
        Self::validate_definition(&def)?;

        let mut inner = self.inner.write().await;
        let key = (tenant_id.to_string(), rule_id.to_string());
        let versions = inner.entry(key.clone()).or_insert_with(Vec::new);
        let next_version = (versions.last().map(|v| v.version).unwrap_or(0)) + 1;

        let rv = RuleVersion {
            uid: Uuid::new_v4(),
            tenant_id: tenant_id.to_string(),
            rule_id: rule_id.to_string(),
            version: next_version,
            created_by: created_by.to_string(),
            created_at: Utc::now(),
            definition: def.clone(),
            active: true,
        };

        // deactivate previous versions
        for v in versions.iter_mut() {
            v.active = false;
        }
        versions.push(rv.clone());

        drop(inner);
        self.rebuild_active_index_for_tenant(tenant_id).await;

        self.audit.record_rule_change(&rv, "update").await;

        Ok(rv)
    }

    async fn get_active_rules(&self, tenant_id: &str) -> anyhow::Result<Vec<RuleVersion>> {
        let idx = self.active_index.read().await;
        let keys = match idx.get(tenant_id) {
            Some(v) => v.clone(),
            None => Vec::new(),
        };
        let inner = self.inner.read().await;
        let mut out = Vec::new();
        for (_priority, rule_id, version) in keys.into_iter() {
            if let Some(versions) = inner.get(&(tenant_id.to_string(), rule_id.clone())) {
                if let Some(v) = versions.iter().find(|x| x.version == version) {
                    out.push(v.clone());
                }
            }
        }
        Ok(out)
    }

    async fn get_rule_versions(&self, tenant_id: &str, rule_id: &str) -> anyhow::Result<Vec<RuleVersion>> {
        let inner = self.inner.read().await;
        Ok(inner.get(&(tenant_id.to_string(), rule_id.to_string())).cloned().unwrap_or_default())
    }

    async fn rollback_rule(&self, tenant_id: &str, rule_id: &str, target_version: i64, performed_by: &str) -> anyhow::Result<RuleVersion> {
        let mut inner = self.inner.write().await;
        let key = (tenant_id.to_string(), rule_id.to_string());
        let versions = inner.get_mut(&key).ok_or_else(|| anyhow::anyhow!("rule not found"))?;

        // find the target
        let target = versions.iter_mut().find(|v| v.version == target_version).ok_or_else(|| anyhow::anyhow!("target version not found"))?;
        // mark all active false then activate target
        for v in versions.iter_mut() {
            v.active = false;
        }
        target.active = true;

        // create a new version representing the rollback event (copy target def)
        let next_version = (versions.last().map(|v| v.version).unwrap_or(0)) + 1;
        let rv = RuleVersion {
            uid: Uuid::new_v4(),
            tenant_id: tenant_id.to_string(),
            rule_id: rule_id.to_string(),
            version: next_version,
            created_by: performed_by.to_string(),
            created_at: Utc::now(),
            definition: target.definition.clone(),
            active: true,
        };
        versions.push(rv.clone());

        drop(inner);
        self.rebuild_active_index_for_tenant(tenant_id).await;

        self.audit.record_rule_change(&rv, "rollback").await;

        Ok(rv)
    }

    async fn deactivate_rule(&self, tenant_id: &str, rule_id: &str, performed_by: &str) -> anyhow::Result<()> {
        let mut inner = self.inner.write().await;
        let key = (tenant_id.to_string(), rule_id.to_string());
        if let Some(versions) = inner.get_mut(&key) {
            for v in versions.iter_mut() {
                v.active = false;
            }
            // record an audit entry representing deactivation
            let rv = RuleVersion {
                uid: Uuid::new_v4(),
                tenant_id: tenant_id.to_string(),
                rule_id: rule_id.to_string(),
                version: (versions.last().map(|v| v.version).unwrap_or(0)) + 1,
                created_by: performed_by.to_string(),
                created_at: Utc::now(),
                definition: versions.last().unwrap().definition.clone(),
                active: false,
            };
            versions.push(rv.clone());
            drop(inner);
            self.rebuild_active_index_for_tenant(tenant_id).await;
            self.audit.record_rule_change(&rv, "deactivate").await;
            Ok(())
        } else {
            Err(anyhow::anyhow!("rule not found"))
        }
    }

    async fn evaluate(&self, tenant_id: &str, input: &Value) -> anyhow::Result<Vec<RuleMatch>> {
        // get active rules for tenant (ordered by priority)
        let active = self.get_active_rules(tenant_id).await?;
        let mut matches = Vec::new();

        for v in active.iter() {
            match Self::evaluate_definition(&v.definition, input) {
                Ok(true) => {
                    matches.push(RuleMatch {
                        tenant_id: tenant_id.to_string(),
                        rule_id: v.rule_id.clone(),
                        version: v.version,
                        actions: v.definition.actions.clone(),
                    });
                }
                Ok(false) => {}
                Err(e) => {
                    warn!("Error evaluating rule {} v{}: {:?}", v.rule_id, v.version, e);
                }
            }
        }

        Ok(matches)
    }

    async fn validate_raw_rule(&self, raw: &str, format: RuleFormat) -> anyhow::Result<RuleDefinition> {
        let def = Self::parse_raw_rule(raw, format)?;
        Self::validate_definition(&def)?;
        Ok(def)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    struct TestAudit;
    #[async_trait]
    impl AuditSink for TestAudit {
        async fn record_rule_change(&self, entry: &RuleVersion, action: &str) {
            info!("AUDIT: {} rule {} v{} by {}", action, entry.rule_id, entry.version, entry.created_by);
        }
    }

    #[tokio::test]
    async fn test_create_and_evaluate_rule_json() {
        let store = InMemoryRuleStore::new(Arc::new(TestAudit));
        let raw = r#"
        {
            "id": "r1",
            "name": "high_error_count",
            "description": "error_count >= 10",
            "priority": 100,
            "conditions": [
                { "path": "payload.error_count", "op": ">=", "value": 10 }
            ],
            "actions": { "severity": "Critical", "notify": true }
        }
        "#;

        let rv = store.create_rule("tenant1", "alice", raw, RuleFormat::Json).await.expect("create");
        assert_eq!(rv.rule_id, "r1");
        assert!(rv.active);

        // prepare input
        let input = json!({
            "payload": { "error_count": 12 },
            "meta": {}
        });

        let matches = store.evaluate("tenant1", &input).await.expect("eval");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].actions.as_ref().unwrap().severity.as_ref().unwrap(), "Critical");
    }

    #[tokio::test]
    async fn test_yaml_rule_and_versioning_and_rollback() {
        let store = InMemoryRuleStore::new(Arc::new(TestAudit));

        let raw_v1 = r#"
id: r2
name: heartbeat
priority: 1
conditions:
  - path: payload.status
    op: eq
    value: "down"
actions:
  severity: Warning
  notify: true
"#;
        let v1 = store.create_rule("t2", "bob", raw_v1, RuleFormat::Yaml).await.expect("create v1");
        assert_eq!(v1.version, 1);

        let raw_v2 = r#"
id: r2
name: heartbeat
priority: 5
conditions:
  - path: payload.status
    op: eq
    value: "critical"
actions:
  severity: Critical
  notify: true
"#;
        let v2 = store.update_rule("t2", "r2", "bob", raw_v2, RuleFormat::Yaml).await.expect("update v2");
        assert_eq!(v2.version, 2);

        // evaluate: only v2 active (priority 5)
        let input = json!({"payload": {"status": "critical"}});
        let matches = store.evaluate("t2", &input).await.unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].actions.as_ref().unwrap().severity.as_ref().unwrap(), "Critical");

        // rollback to version 1
        let rb = store.rollback_rule("t2", "r2", 1, "admin").await.expect("rollback");
        assert_eq!(rb.version, 3); // new version created representing rollback
        // Now v3 should be active and equal to v1.definition
        let matches_after = store.evaluate("t2", &json!({"payload": {"status": "down"}})).await.unwrap();
        assert_eq!(matches_after.len(), 1);
        assert_eq!(matches_after[0].actions.as_ref().unwrap().severity.as_ref().unwrap(), "Warning");
    }

    #[tokio::test]
    async fn test_validate_bad_rule() {
        let store = InMemoryRuleStore::new(Arc::new(TestAudit));
        // invalid because empty id
        let bad = r#"
        {
            "id": "",
            "name": "bad",
            "conditions": []
        }
        "#;
        let res = store.validate_raw_rule(bad, RuleFormat::Json).await;
        assert!(res.is_err());
    }
}
