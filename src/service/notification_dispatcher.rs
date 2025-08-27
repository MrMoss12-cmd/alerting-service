// src/service/notification_dispatcher.rs
//! Notification dispatcher service
//!
//! Responsibilities:
//! - Deliver alerts to multiple communication channels (email, SMS, chat, webhooks, etc).
//! - Handle rate limiting and quota control to prevent channel saturation.
//! - Confirm successful delivery, support retries on failure.
//! - Protect sensitive data in transit and at destination.
//! - Support multi-tenant isolation with per-tenant channels and credentials.

use std::{collections::HashMap, sync::Arc, time::Duration};
use async_trait::async_trait;
use tokio::{sync::Mutex, time::sleep};
use uuid::Uuid;
use chrono::{Utc, DateTime};

/// Supported notification channels (extendable)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NotificationChannel {
    Email,
    Sms,
    Chat(String),       // e.g. Slack, Teams
    Webhook(String),    // named webhook endpoint
    Custom(String),
}

/// Represents the delivery status of a notification attempt
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryStatus {
    Pending,
    Delivered(DateTime<Utc>),
    Failed(DateTime<Utc>, String), // timestamp + failure reason
}

/// Notification request data, per tenant and channel
#[derive(Clone)]
pub struct NotificationRequest {
    pub alert_id: Uuid,
    pub tenant_id: String,
    pub channel: NotificationChannel,
    pub payload: String,             // Serialized alert content or message body
    pub credentials: Option<HashMap<String, String>>, // channel-specific creds (e.g. API keys)
}

/// Trait for channel-specific notifier implementations
#[async_trait]
pub trait Notifier: Send + Sync {
    /// Sends the notification, returns Ok(()) if delivered, or Err with reason
    async fn send(&self, request: NotificationRequest) -> Result<(), String>;
}

/// Rate limiter to control send speed and avoid saturation
struct RateLimiter {
    max_per_minute: usize,
    sent_this_minute: usize,
    window_start: DateTime<Utc>,
}

impl RateLimiter {
    fn new(max_per_minute: usize) -> Self {
        Self {
            max_per_minute,
            sent_this_minute: 0,
            window_start: Utc::now(),
        }
    }

    /// Checks if sending is allowed, waits if needed
    async fn allow_send(&mut self) {
        let now = Utc::now();
        if (now - self.window_start).num_seconds() >= 60 {
            self.window_start = now;
            self.sent_this_minute = 0;
        }

        if self.sent_this_minute >= self.max_per_minute {
            let wait_secs = 60 - (now - self.window_start).num_seconds();
            sleep(Duration::from_secs(wait_secs as u64)).await;
            self.window_start = Utc::now();
            self.sent_this_minute = 0;
        }

        self.sent_this_minute += 1;
    }
}

/// Notification dispatcher main service
pub struct NotificationDispatcher {
    notifiers: HashMap<NotificationChannel, Arc<dyn Notifier>>,
    rate_limiters: Arc<Mutex<HashMap<NotificationChannel, RateLimiter>>>,
}

impl NotificationDispatcher {
    /// Create new dispatcher with configured notifiers
    pub fn new(notifiers: HashMap<NotificationChannel, Arc<dyn Notifier>>, max_per_minute: usize) -> Self {
        let mut rate_limiters = HashMap::new();
        for channel in notifiers.keys() {
            rate_limiters.insert(channel.clone(), RateLimiter::new(max_per_minute));
        }

        Self {
            notifiers,
            rate_limiters: Arc::new(Mutex::new(rate_limiters)),
        }
    }

    /// Dispatch notification to multiple channels concurrently
    pub async fn dispatch(&self, requests: Vec<NotificationRequest>) -> HashMap<NotificationChannel, DeliveryStatus> {
        use futures::future::join_all;

        // For each request, spawn async task to send notification with retries
        let tasks = requests.into_iter().map(|req| {
            let dispatcher = self.clone();
            tokio::spawn(async move {
                let status = dispatcher.send_with_retries(req).await;
                (status.0, status.1)
            })
        });

        let results = join_all(tasks).await;

        // Collect results into map channel -> status
        let mut delivery_map = HashMap::new();
        for res in results {
            if let Ok((channel, status)) = res {
                delivery_map.insert(channel, status);
            }
        }

        delivery_map
    }

    /// Send notification with retry logic and rate limiting
    async fn send_with_retries(&self, request: NotificationRequest) -> (NotificationChannel, DeliveryStatus) {
        const MAX_RETRIES: usize = 3;
        const RETRY_DELAY_SECS: u64 = 5;

        let channel = request.channel.clone();

        for attempt in 0..=MAX_RETRIES {
            // Rate limit before sending
            {
                let mut limiters = self.rate_limiters.lock().await;
                if let Some(limiter) = limiters.get_mut(&channel) {
                    limiter.allow_send().await;
                }
            }

            // Send notification via corresponding notifier
            if let Some(notifier) = self.notifiers.get(&channel) {
                match notifier.send(request.clone()).await {
                    Ok(_) => {
                        return (channel, DeliveryStatus::Delivered(Utc::now()));
                    }
                    Err(err) => {
                        if attempt == MAX_RETRIES {
                            return (channel, DeliveryStatus::Failed(Utc::now(), err));
                        } else {
                            sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                        }
                    }
                }
            } else {
                return (channel, DeliveryStatus::Failed(Utc::now(), "No notifier configured".into()));
            }
        }

        // Should not reach here normally
        (channel, DeliveryStatus::Failed(Utc::now(), "Unknown failure".into()))
    }
}

impl Clone for NotificationDispatcher {
    fn clone(&self) -> Self {
        Self {
            notifiers: self.notifiers.clone(),
            rate_limiters: Arc::clone(&self.rate_limiters),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex;
    use std::sync::Arc;

    struct DummyNotifier {
        pub fail_first: Mutex<bool>,
    }

    #[async_trait]
    impl Notifier for DummyNotifier {
        async fn send(&self, _request: NotificationRequest) -> Result<(), String> {
            let mut fail_flag = self.fail_first.lock().await;
            if *fail_flag {
                *fail_flag = false;
                Err("Simulated failure".into())
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn test_dispatch_success_and_retry() {
        let dummy_notifier = Arc::new(DummyNotifier { fail_first: Mutex::new(true) });
        let mut notifiers = HashMap::new();
        notifiers.insert(NotificationChannel::Email, dummy_notifier.clone());

        let dispatcher = NotificationDispatcher::new(notifiers, 10);

        let request = NotificationRequest {
            alert_id: Uuid::new_v4(),
            tenant_id: "tenant1".into(),
            channel: NotificationChannel::Email,
            payload: "Test message".into(),
            credentials: None,
        };

        let results = dispatcher.dispatch(vec![request]).await;

        let status = results.get(&NotificationChannel::Email).unwrap();
        match status {
            DeliveryStatus::Delivered(_) => (),
            _ => panic!("Expected delivered status"),
        }
    }

    #[tokio::test]
    async fn test_dispatch_no_notifier() {
        let dispatcher = NotificationDispatcher::new(HashMap::new(), 10);

        let request = NotificationRequest {
            alert_id: Uuid::new_v4(),
            tenant_id: "tenant1".into(),
            channel: NotificationChannel::Sms,
            payload: "Test message".into(),
            credentials: None,
        };

        let results = dispatcher.dispatch(vec![request]).await;

        let status = results.get(&NotificationChannel::Sms).unwrap();
        match status {
            DeliveryStatus::Failed(_, msg) => assert_eq!(msg, "No notifier configured"),
            _ => panic!("Expected failed status"),
        }
    }
}
