// tests/integration/notify_flow_test.rs

use anyhow::Result;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep, Duration};
use tracing::{info, error};
use std::collections::HashMap;

// Mocks / stubs for components involved in the flow
// In real tests, these could be replaced by integration points or test doubles.

#[derive(Debug, Clone)]
struct Alert {
    tenant_id: String,
    alert_id: String,
    severity: String,
    message: String,
    enriched: bool,
}

#[derive(Debug)]
enum NotificationChannel {
    Email,
    Slack,
    Webhook,
}

#[derive(Debug)]
struct Notification {
    alert: Alert,
    channel: NotificationChannel,
    success: bool,
}

struct MockKafkaProducer {
    sender: mpsc::Sender<Alert>,
}

impl MockKafkaProducer {
    async fn send(&self, alert: Alert) -> Result<()> {
        self.sender.send(alert).await.map_err(|e| anyhow::anyhow!(e.to_string()))
    }
}

struct MockAlertClassifier;

impl MockAlertClassifier {
    async fn classify(&self, alert: &Alert) -> Alert {
        // Simulate enrichment
        let mut enriched_alert = alert.clone();
        enriched_alert.enriched = true;
        enriched_alert
    }
}

struct MockNotifier {
    // channels to simulate notifications
    results_tx: mpsc::Sender<Notification>,
}

impl MockNotifier {
    async fn notify(&self, alert: Alert, channel: NotificationChannel) -> bool {
        // Simulate delivery success/failure (random or deterministic)
        let success = match channel {
            NotificationChannel::Email => true,
            NotificationChannel::Slack => true,
            NotificationChannel::Webhook => true,
        };

        let _ = self.results_tx.send(Notification { alert, channel, success }).await;
        success
    }
}

// Main test flow simulating end-to-end pipeline
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_notify_flow_end_to_end() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Channel for simulating Kafka ingestion queue
    let (kafka_tx, mut kafka_rx) = mpsc::channel::<Alert>(32);
    // Channel for collecting notification results
    let (notif_tx, mut notif_rx) = mpsc::channel::<Notification>(32);

    let kafka_producer = MockKafkaProducer { sender: kafka_tx.clone() };
    let classifier = MockAlertClassifier;
    let notifier = MockNotifier { results_tx: notif_tx.clone() };

    // Spawn a task simulating consumer + classifier + notifier flow
    tokio::spawn(async move {
        while let Some(mut alert) = kafka_rx.recv().await {
            info!("Received alert for classification: {:?}", alert);
            // Classification / enrichment
            alert = classifier.classify(&alert).await;
            assert!(alert.enriched);

            // Simulate multi-channel notification
            let channels = vec![
                NotificationChannel::Email,
                NotificationChannel::Slack,
                NotificationChannel::Webhook,
            ];

            for ch in channels {
                // Simulate notification send with retry logic placeholder
                let mut attempts = 0;
                let max_attempts = 3;
                let mut success = false;
                while attempts < max_attempts && !success {
                    attempts += 1;
                    success = notifier.notify(alert.clone(), ch.clone()).await;
                    if !success {
                        info!("Retrying notification for alert {} on channel {:?}", alert.alert_id, ch);
                        sleep(Duration::from_millis(100)).await;
                    }
                }
                assert!(success, "Notification failed after retries");
            }
        }
    });

    // Test data with multiple tenants and alerts
    let test_alerts = vec![
        Alert { tenant_id: "tenant1".to_string(), alert_id: "alert-01".to_string(), severity: "high".to_string(), message: "CPU overload".to_string(), enriched: false },
        Alert { tenant_id: "tenant2".to_string(), alert_id: "alert-02".to_string(), severity: "medium".to_string(), message: "Disk space low".to_string(), enriched: false },
        Alert { tenant_id: "tenant1".to_string(), alert_id: "alert-03".to_string(), severity: "low".to_string(), message: "Service restart".to_string(), enriched: false },
    ];

    // Send alerts simulating Kafka ingestion
    for alert in test_alerts.iter() {
        kafka_producer.send(alert.clone()).await?;
    }

    // Collect notifications with timeout guard
    let mut received_notifications: HashMap<(String, NotificationChannel), usize> = HashMap::new();
    let expected_notifications = test_alerts.len() * 3; // 3 channels per alert

    let mut total_received = 0;
    let timeout = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            maybe_notif = notif_rx.recv() => {
                if let Some(notif) = maybe_notif {
                    info!("Notification sent: alert_id={} channel={:?} success={}", notif.alert.alert_id, notif.channel, notif.success);
                    assert!(notif.success, "Notification should succeed");
                    *received_notifications.entry((notif.alert.alert_id.clone(), notif.channel)).or_insert(0) += 1;
                    total_received += 1;
                    if total_received >= expected_notifications {
                        break;
                    }
                } else {
                    break; // channel closed
                }
            }
            _ = &mut timeout => {
                error!("Timeout waiting for notifications");
                break;
            }
        }
    }

    // Validate all expected notifications received exactly once per channel per alert
    for alert in test_alerts.iter() {
        for ch in &[NotificationChannel::Email, NotificationChannel::Slack, NotificationChannel::Webhook] {
            let count = received_notifications.get(&(alert.alert_id.clone(), ch.clone())).unwrap_or(&0);
            assert_eq!(*count, 1, "Expected exactly one notification for alert {} on channel {:?}", alert.alert_id, ch);
        }
    }

    Ok(())
}
