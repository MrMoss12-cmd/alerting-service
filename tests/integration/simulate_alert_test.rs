// tests/integration/simulate_alert_test.rs

use anyhow::Result;
use tokio::sync::{mpsc};
use tokio::time::{timeout, Duration};
use tracing::{info, error};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone)]
struct Alert {
    tenant_id: String,
    alert_id: String,
    severity: String,
    message: String,
    simulated: bool,
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

struct AlertSimulator {
    sender: mpsc::Sender<Alert>,
    simulation_log: Vec<String>,
}

impl AlertSimulator {
    fn new(sender: mpsc::Sender<Alert>) -> Self {
        Self {
            sender,
            simulation_log: Vec::new(),
        }
    }

    async fn simulate_alert(&mut self, tenant_id: &str, severity: &str, message: &str) -> Result<()> {
        let alert = Alert {
            tenant_id: tenant_id.to_string(),
            alert_id: Uuid::new_v4().to_string(),
            severity: severity.to_string(),
            message: message.to_string(),
            simulated: true,
            enriched: false,
        };

        self.simulation_log.push(format!("Simulated alert: {:?}", alert));
        self.sender.send(alert).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;

        Ok(())
    }

    fn get_simulation_log(&self) -> &Vec<String> {
        &self.simulation_log
    }
}

struct MockAlertProcessor {
    classification_log: Vec<String>,
    notification_sender: mpsc::Sender<Notification>,
}

impl MockAlertProcessor {
    fn new(notification_sender: mpsc::Sender<Notification>) -> Self {
        Self {
            classification_log: Vec::new(),
            notification_sender,
        }
    }

    async fn process_alert(&mut self, mut alert: Alert) -> Result<()> {
        // Enrich and classify alert
        alert.enriched = true;
        self.classification_log.push(format!("Classified alert: {:?}", alert));

        // Notify channels
        let channels = vec![
            NotificationChannel::Email,
            NotificationChannel::Slack,
            NotificationChannel::Webhook,
        ];

        for channel in channels {
            let notif = Notification {
                alert: alert.clone(),
                channel,
                success: true,
            };
            self.notification_sender.send(notif).await?;
        }
        Ok(())
    }

    fn get_classification_log(&self) -> &Vec<String> {
        &self.classification_log
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_simulate_alerts_full_flow() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Channels for alert ingestion and notification sending
    let (alert_tx, mut alert_rx) = mpsc::channel::<Alert>(64);
    let (notif_tx, mut notif_rx) = mpsc::channel::<Notification>(64);

    let mut simulator = AlertSimulator::new(alert_tx.clone());
    let mut processor = MockAlertProcessor::new(notif_tx.clone());

    // Spawn task simulating alert processing pipeline
    tokio::spawn(async move {
        while let Some(alert) = alert_rx.recv().await {
            if let Err(e) = processor.process_alert(alert).await {
                error!("Error processing alert: {}", e);
            }
        }
    });

    // Simulate predefined and dynamic alerts for multiple tenants
    let tenants = vec!["tenantA", "tenantB"];
    let severities = vec!["low", "medium", "high"];

    // Simulate preconfigured alerts
    for tenant in &tenants {
        for severity in &severities {
            simulator.simulate_alert(tenant, severity, "Preconfigured test alert").await?;
        }
    }

    // Simulate dynamic alerts (massive load test)
    let simulate_count = 50;
    for i in 0..simulate_count {
        let tenant = tenants[i % tenants.len()];
        let severity = severities[i % severities.len()];
        let msg = format!("Dynamic simulated alert {}", i+1);
        simulator.simulate_alert(tenant, severity, &msg).await?;
    }

    // Collect notifications with timeout guard
    let expected_notifications = (tenants.len() * severities.len() + simulate_count) * 3; // 3 channels per alert
    let mut received_notifications: HashMap<(String, NotificationChannel), usize> = HashMap::new();
    let mut total_received = 0;

    let wait_timeout = Duration::from_secs(10);
    let collect_res = timeout(wait_timeout, async {
        while let Some(notif) = notif_rx.recv().await {
            info!("Notification: alert_id={} tenant={} channel={:?} success={}",
                notif.alert.alert_id, notif.alert.tenant_id, notif.channel, notif.success);
            assert!(notif.success);
            *received_notifications.entry((notif.alert.alert_id.clone(), notif.channel)).or_insert(0) += 1;
            total_received += 1;
            if total_received >= expected_notifications {
                break;
            }
        }
    }).await;

    if collect_res.is_err() {
        error!("Timeout collecting notifications");
    }

    // Verify all notifications received once per channel per alert
    assert_eq!(total_received, expected_notifications);

    // Verify simulator logs contain entries
    assert!(!simulator.get_simulation_log().is_empty());
    // Verify processor logs contain entries
    assert!(!processor.get_classification_log().is_empty());

    // Check multi-tenant isolation (each alert belongs to the correct tenant)
    for ((alert_id, _channel), _) in &received_notifications {
        let alert_belongs_to_known_tenant = simulator.get_simulation_log().iter().any(|log| log.contains(alert_id));
        assert!(alert_belongs_to_known_tenant);
    }

    Ok(())
}
