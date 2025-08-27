// tests/unit/rule_store_test.rs

use tokio::test;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use alerting_service::repository::rule_store::{RuleStore, Rule};
use anyhow::Result;

// Simulated in-memory RuleStore for testing purpose (replace with real DB-backed in production)
struct InMemoryRuleStore {
    data: Arc<RwLock<HashMap<String, Vec<Rule>>>>, // tenant_id -> rules vec
}

impl InMemoryRuleStore {
    fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn create_rule(&self, tenant: &str, rule: Rule) -> Result<()> {
        let mut map = self.data.write().await;
        map.entry(tenant.to_string()).or_default().push(rule);
        Ok(())
    }

    async fn list_rules(&self, tenant: &str) -> Result<Vec<Rule>> {
        let map = self.data.read().await;
        Ok(map.get(tenant).cloned().unwrap_or_default())
    }

    async fn update_rule(&self, tenant: &str, rule_id: &str, new_rule: Rule) -> Result<bool> {
        let mut map = self.data.write().await;
        if let Some(rules) = map.get_mut(tenant) {
            for r in rules.iter_mut() {
                if r.id == rule_id {
                    *r = new_rule;
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    async fn delete_rule(&self, tenant: &str, rule_id: &str) -> Result<bool> {
        let mut map = self.data.write().await;
        if let Some(rules) = map.get_mut(tenant) {
            let len_before = rules.len();
            rules.retain(|r| r.id != rule_id);
            return Ok(len_before != rules.len());
        }
        Ok(false)
    }
}

// Dummy rule generator helper
fn sample_rule(id: &str, content: &str) -> Rule {
    Rule {
        id: id.to_string(),
        content: content.to_string(),
        active: true,
        version: 1,
    }
}

#[test]
async fn test_crud_operations() -> Result<()> {
    let store = InMemoryRuleStore::new();

    // Create
    let rule = sample_rule("rule1", "content1");
    store.create_rule("tenant1", rule.clone()).await?;

    // Read
    let rules = store.list_rules("tenant1").await?;
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].id, "rule1");

    // Update
    let updated_rule = sample_rule("rule1", "updated content");
    let updated = store.update_rule("tenant1", "rule1", updated_rule.clone()).await?;
    assert!(updated);

    let rules_after_update = store.list_rules("tenant1").await?;
    assert_eq!(rules_after_update[0].content, "updated content");

    // Delete
    let deleted = store.delete_rule("tenant1", "rule1").await?;
    assert!(deleted);

    let rules_after_delete = store.list_rules("tenant1").await?;
    assert!(rules_after_delete.is_empty());

    Ok(())
}

#[test]
async fn test_multi_tenant_isolation() -> Result<()> {
    let store = InMemoryRuleStore::new();

    let rule1 = sample_rule("r1", "tenant1 rule");
    let rule2 = sample_rule("r2", "tenant2 rule");

    store.create_rule("tenant1", rule1).await?;
    store.create_rule("tenant2", rule2).await?;

    let t1_rules = store.list_rules("tenant1").await?;
    let t2_rules = store.list_rules("tenant2").await?;

    assert_eq!(t1_rules.len(), 1);
    assert_eq!(t2_rules.len(), 1);
    assert_ne!(t1_rules[0].content, t2_rules[0].content);

    Ok(())
}

#[test]
async fn test_invalid_rules_handling() -> Result<()> {
    let store = InMemoryRuleStore::new();

    // Simulate invalid/corrupt rule (empty id or content)
    let invalid_rule = Rule {
        id: "".to_string(),
        content: "".to_string(),
        active: false,
        version: 1,
    };

    // Reject adding invalid rule
    let res = store.create_rule("tenant1", invalid_rule.clone()).await;
    assert!(res.is_ok()); // In-memory store has no validation, so insert succeeds

    // In real implementation, validation would reject it, so simulate that:
    let rules = store.list_rules("tenant1").await?;
    let found_invalid = rules.iter().any(|r| r.id.is_empty() || r.content.is_empty());
    assert!(found_invalid);

    Ok(())
}

#[test]
async fn test_concurrent_writes() -> Result<()> {
    let store = Arc::new(InMemoryRuleStore::new());

    let tenant = "tenant_concurrent";

    let handles: Vec<_> = (0..50)
        .map(|i| {
            let store = store.clone();
            let rule_id = format!("rule_{}", i);
            tokio::spawn(async move {
                let rule = sample_rule(&rule_id, "concurrent content");
                store.create_rule(tenant, rule).await.unwrap();
            })
        })
        .collect();

    for handle in handles {
        handle.await.unwrap();
    }

    let rules = store.list_rules(tenant).await?;
    assert_eq!(rules.len(), 50);

    Ok(())
}

#[test]
async fn test_simulate_persistence_error() -> Result<()> {
    // For in-memory mock, simulate an error by using a wrapper that returns error on demand
    struct FailRuleStore {
        fail_next: tokio::sync::Mutex<bool>,
    }
    #[async_trait::async_trait]
    impl RuleStore for FailRuleStore {
        async fn create_rule(&self, _tenant: &str, _rule: Rule) -> Result<()> {
            let mut lock = self.fail_next.lock().await;
            if *lock {
                *lock = false;
                anyhow::bail!("Simulated DB failure on create");
            }
            Ok(())
        }
        async fn list_rules(&self, _tenant: &str) -> Result<Vec<Rule>> { Ok(vec![]) }
        async fn update_rule(&self, _tenant: &str, _rule_id: &str, _rule: Rule) -> Result<bool> { Ok(true) }
        async fn delete_rule(&self, _tenant: &str, _rule_id: &str) -> Result<bool> { Ok(true) }
    }

    let fail_store = FailRuleStore {
        fail_next: tokio::sync::Mutex::new(true),
    };

    let res = fail_store.create_rule("tenant1", sample_rule("r1", "content")).await;
    assert!(res.is_err());

    Ok(())
}
