// tests/integration/kafka_ingestion_test.rs

use anyhow::Result;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::consumer::{StreamConsumer, Consumer};
use rdkafka::message::Message;
use rdkafka::TopicPartitionList;
use rdkafka::util::Timeout;
use serde_json::json;
use tokio::time::{sleep, Duration};
use tracing::info;
use futures::StreamExt;
use std::collections::HashSet;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_kafka_ingestion_pipeline() -> Result<()> {
    // Setup tracing for the test (could be a minimal setup)
    tracing_subscriber::fmt::init();

    // Kafka config - adjust bootstrap servers to your test cluster
    let brokers = "localhost:9092";
    let topic = "alerts_test_topic";
    let group_id = "test_group_kafka_ingestion";

    // Create Kafka producer
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .set("message.timeout.ms", "5000")
        .create()
        .expect("Producer creation failed");

    // Create Kafka consumer
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .set("group.id", group_id)
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Consumer creation failed");

    // Assign topic partition list manually to ensure full consumption from beginning
    let mut tpl = TopicPartitionList::new();
    tpl.add_partition(topic, 0);
    consumer.assign(&tpl)?;

    // Prepare test messages: well-formed, incomplete, corrupt JSON, multi-tenant
    let valid_alert = json!({
        "tenant_id": "tenant1",
        "alert_id": "alert-001",
        "severity": "high",
        "message": "CPU usage above threshold",
        "timestamp": "2025-08-12T10:00:00Z"
    });

    let incomplete_alert = json!({
        "tenant_id": "tenant2",
        "alert_id": "alert-002"
        // Missing severity, message, timestamp
    });

    let corrupt_alert = "{ this is not json }";

    let tenant3_alerts = (1..=10).map(|i| {
        json!({
            "tenant_id": "tenant3",
            "alert_id": format!("alert-300{}", i),
            "severity": "medium",
            "message": format!("Alert number {}", i),
            "timestamp": "2025-08-12T10:05:00Z"
        })
    }).collect::<Vec<_>>();

    // Produce messages with concurrency simulating burst
    let mut produce_futures = vec![];
    produce_futures.push(producer.send(
        FutureRecord::to(topic)
            .payload(&valid_alert.to_string())
            .key("key1"),
        Timeout::Never,
    ));

    produce_futures.push(producer.send(
        FutureRecord::to(topic)
            .payload(&incomplete_alert.to_string())
            .key("key2"),
        Timeout::Never,
    ));

    produce_futures.push(producer.send(
        FutureRecord::to(topic)
            .payload(corrupt_alert)
            .key("key3"),
        Timeout::Never,
    ));

    for alert in tenant3_alerts.iter() {
        produce_futures.push(producer.send(
            FutureRecord::to(topic)
                .payload(&alert.to_string())
                .key(&alert["alert_id"].as_str().unwrap()),
            Timeout::Never,
        ));
    }

    // Await all sends to complete
    futures::future::join_all(produce_futures).await;

    // Wait a moment for messages to arrive at consumer
    sleep(Duration::from_secs(2)).await;

    // Track consumed alert_ids to verify no duplicates or missing
    let mut consumed_alerts = HashSet::new();
    let mut incomplete_found = false;
    let mut corrupt_found = false;

    // Consume messages - limit max to avoid infinite loop
    let mut stream = consumer.stream();
    let mut msg_count = 0;
    while let Some(result) = stream.next().await {
        let msg = match result {
            Ok(m) => m,
            Err(e) => {
                info!("Kafka consumer error: {:?}", e);
                continue;
            }
        };

        msg_count += 1;

        let payload = match msg.payload_view::<str>() {
            Some(Ok(s)) => s,
            _ => {
                info!("Received message with invalid UTF-8 payload");
                continue;
            }
        };

        // Try parse JSON to detect valid vs corrupt
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(payload);
        match parsed {
            Ok(val) => {
                // Validate required fields tenant_id and alert_id presence
                let tenant_id = val.get("tenant_id").and_then(|v| v.as_str());
                let alert_id = val.get("alert_id").and_then(|v| v.as_str());

                if tenant_id.is_none() || alert_id.is_none() {
                    incomplete_found = true;
                    // Commit offset to avoid reprocessing incomplete alerts endlessly
                    consumer.commit_message(&msg, rdkafka::consumer::CommitMode::Async)?;
                    continue;
                }

                // Insert alert id for uniqueness check
                if consumed_alerts.contains(alert_id.unwrap()) {
                    panic!("Duplicate alert id received: {}", alert_id.unwrap());
                }
                consumed_alerts.insert(alert_id.unwrap().to_string());

                // Simulate processing: classification, storing, metrics increment, etc.
                // e.g., assert severity field is valid if present
                if let Some(sev) = val.get("severity").and_then(|v| v.as_str()) {
                    assert!(["low", "medium", "high"].contains(&sev));
                }

                // Commit offset after successful processing
                consumer.commit_message(&msg, rdkafka::consumer::CommitMode::Async)?;
            }
            Err(_) => {
                corrupt_found = true;
                // Commit offset to skip corrupt messages
                consumer.commit_message(&msg, rdkafka::consumer::CommitMode::Async)?;
            }
        }

        // Break condition to avoid endless loop in test environment
        if msg_count >= 15 {
            break;
        }
    }

    // Assertions on expected conditions:
    // - Found corrupt and incomplete alerts processed and committed (no infinite reprocessing)
    assert!(corrupt_found, "Expected corrupt messages to be detected");
    assert!(incomplete_found, "Expected incomplete messages to be detected");

    // - All valid alerts from tenant3 received exactly once
    for i in 1..=10 {
        let alert_id = format!("alert-300{}", i);
        assert!(consumed_alerts.contains(&alert_id), "Missing alert {}", alert_id);
    }

    // - Valid alert from tenant1 present
    assert!(consumed_alerts.contains("alert-001"));

    // - Incomplete alert from tenant2 not in consumed_alerts set because missing alert_id
    assert!(!consumed_alerts.contains("alert-002"));

    Ok(())
}
