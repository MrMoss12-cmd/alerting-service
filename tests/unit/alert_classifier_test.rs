// tests/unit/alert_classifier_test.rs

use tokio::test;
use std::collections::HashMap;
use alerting_service::service::alert_classifier::{AlertClassifier, Alert, ClassificationResult};

// Simulated alert types for testing
fn sample_alert(tenant: &str, alert_type: &str, severity: &str, extra: Option<HashMap<&str, &str>>) -> Alert {
    Alert {
        tenant_id: tenant.to_string(),
        alert_type: alert_type.to_string(),
        severity: severity.to_string(),
        payload: extra.unwrap_or_default().into_iter().map(|(k,v)| (k.to_string(), v.to_string())).collect(),
    }
}

// Fixture for multi-tenant classifier with different rules (simulated)
fn setup_classifier() -> AlertClassifier {
    let mut classifier = AlertClassifier::new();

    // Add tenant-specific rules
    classifier.add_rule("tenant_a", |alert| {
        if alert.alert_type == "cpu" && alert.severity == "high" {
            ClassificationResult {
                category: "performance".to_string(),
                level: "critical".to_string(),
                tags: vec!["tenant_a_rule".to_string()],
            }
        } else {
            ClassificationResult::default()
        }
    });

    classifier.add_rule("tenant_b", |alert| {
        if alert.alert_type == "disk" {
            ClassificationResult {
                category: "storage".to_string(),
                level: "warning".to_string(),
                tags: vec!["tenant_b_rule".to_string()],
            }
        } else {
            ClassificationResult::default()
        }
    });

    classifier
}

#[test]
async fn test_basic_classification() {
    let classifier = setup_classifier();

    // Valid alert for tenant_a
    let alert = sample_alert("tenant_a", "cpu", "high", None);
    let classification = classifier.classify(&alert).await.unwrap();
    assert_eq!(classification.category, "performance");
    assert_eq!(classification.level, "critical");
    assert!(classification.tags.contains(&"tenant_a_rule".to_string()));

    // Valid alert for tenant_b
    let alert2 = sample_alert("tenant_b", "disk", "medium", None);
    let classification2 = classifier.classify(&alert2).await.unwrap();
    assert_eq!(classification2.category, "storage");
    assert_eq!(classification2.level, "warning");
    assert!(classification2.tags.contains(&"tenant_b_rule".to_string()));

    // Alert type not matching any rule
    let alert3 = sample_alert("tenant_b", "network", "low", None);
    let classification3 = classifier.classify(&alert3).await.unwrap();
    assert_eq!(classification3.category, "uncategorized");
}

#[test]
async fn test_invalid_alerts() {
    let classifier = setup_classifier();

    // Missing tenant
    let alert = Alert {
        tenant_id: "".to_string(),
        alert_type: "cpu".to_string(),
        severity: "high".to_string(),
        payload: HashMap::new(),
    };
    let res = classifier.classify(&alert).await;
    assert!(res.is_err());

    // Missing alert_type
    let alert2 = Alert {
        tenant_id: "tenant_a".to_string(),
        alert_type: "".to_string(),
        severity: "high".to_string(),
        payload: HashMap::new(),
    };
    let res2 = classifier.classify(&alert2).await;
    assert!(res2.is_err());
}

#[test]
async fn test_extra_payload_data() {
    let classifier = setup_classifier();

    let mut extra = HashMap::new();
    extra.insert("user", "admin");
    extra.insert("ip", "192.168.1.1");

    let alert = sample_alert("tenant_a", "cpu", "high", Some(extra));
    let classification = classifier.classify(&alert).await.unwrap();

    assert_eq!(classification.category, "performance");
    // Extra payload should not affect classification
}

#[test]
async fn test_no_data_contamination_between_tenants() {
    let classifier = setup_classifier();

    // tenant_a alert with tenant_b type
    let alert = sample_alert("tenant_a", "disk", "medium", None);
    let classification = classifier.classify(&alert).await.unwrap();
    // tenant_a rule does not match disk alert
    assert_eq!(classification.category, "uncategorized");

    // tenant_b alert with tenant_a type
    let alert2 = sample_alert("tenant_b", "cpu", "high", None);
    let classification2 = classifier.classify(&alert2).await.unwrap();
    // tenant_b rule matches only disk alerts
    assert_eq!(classification2.category, "uncategorized");
}

#[test]
async fn test_simulated_high_volume_performance() {
    let classifier = setup_classifier();

    // Simulate 10_000 alerts classification
    let mut handles = Vec::new();
    for i in 0..10_000 {
        let alert = sample_alert("tenant_a", "cpu", "high", None);
        let classifier_ref = &classifier;
        handles.push(tokio::spawn(async move {
            classifier_ref.classify(&alert).await.unwrap()
        }));
    }

    for handle in handles {
        let res = handle.await.unwrap();
        assert_eq!(res.category, "performance");
    }
}

#[test]
async fn test_multiple_input_formats() {
    // Simulate different input formats by directly creating alerts as if parsed from Kafka, HTTP, gRPC

    let classifier = setup_classifier();

    // Kafka format (simulate deserialized alert)
    let kafka_alert = sample_alert("tenant_a", "cpu", "high", None);
    let classification = classifier.classify(&kafka_alert).await.unwrap();
    assert_eq!(classification.category, "performance");

    // HTTP format (simulate deserialized JSON)
    let http_alert = sample_alert("tenant_b", "disk", "medium", None);
    let classification2 = classifier.classify(&http_alert).await.unwrap();
    assert_eq!(classification2.category, "storage");

    // gRPC format (simulate deserialized proto)
    let grpc_alert = sample_alert("tenant_a", "cpu", "high", None);
    let classification3 = classifier.classify(&grpc_alert).await.unwrap();
    assert_eq!(classification3.category, "performance");
}
