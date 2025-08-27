// src/telemetry/metrics.rs
//! Metrics definitions and registration for alerting-service.
//!
//! Responsibilities:
//! - Define key performance, error, latency, saturation and usage metrics.
//! - Attach contextual labels (tenant, channel, alert_type) for fine granularity.
//! - Compatible with Prometheus scrape (using opentelemetry-prometheus).
//! - Control cardinality by limiting label values and dynamic labels.
//! - Support tenant-specific custom metrics for SLAs.
//! - Minimize overhead in collection and updates.
//! - Designed for integration with alerting systems consuming these metrics.
//!
//! Usage:
//! - Call `init_metrics()` at startup to create and register metrics.
//! - Use provided handles or helper functions to update metrics in your code.

use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;

use opentelemetry::metrics::{Meter, Counter, Histogram, ObservableGauge};
use opentelemetry::KeyValue;
use opentelemetry::global;

use once_cell::sync::OnceCell;
use tracing::warn;

/// Max allowed distinct tenants and alert types to avoid cardinality explosion.
/// This is a safeguard; exceeding this logs a warning and skips metric update.
const MAX_TENANTS: usize = 1000;
const MAX_ALERT_TYPES: usize = 100;

/// Metrics container to hold all the metrics instruments.
pub struct AlertingMetrics {
    meter: Meter,

    // Performance counters
    pub alerts_received: Counter<u64>,
    pub alerts_processed: Counter<u64>,
    pub alerts_dropped: Counter<u64>,

    // Histograms for latencies in ms
    pub processing_latency_ms: Histogram<f64>,
    pub delivery_latency_ms: Histogram<f64>,

    // Queue saturation gauge (observable)
    pub retry_queue_size: ObservableGauge<u64>,

    // Error counters by type
    pub error_count: Counter<u64>,

    // Sets for cardinality control (protected with RwLock)
    tenants_seen: Arc<RwLock<HashSet<String>>>,
    alert_types_seen: Arc<RwLock<HashSet<String>>>,
}

/// Static global metrics instance
static GLOBAL_METRICS: OnceCell<AlertingMetrics> = OnceCell::new();

impl AlertingMetrics {
    /// Initialize and register all metrics, returns Arc to metrics container.
    pub fn init() -> Arc<Self> {
        let meter = global::meter("alerting_service");

        // Create counters
        let alerts_received = meter
            .u64_counter("alerts_received_total")
            .with_description("Total number of alerts received")
            .init();

        let alerts_processed = meter
            .u64_counter("alerts_processed_total")
            .with_description("Total number of alerts processed successfully")
            .init();

        let alerts_dropped = meter
            .u64_counter("alerts_dropped_total")
            .with_description("Total number of alerts dropped due to errors or duplicates")
            .init();

        // Create histograms (latencies in milliseconds)
        let processing_latency_ms = meter
            .f64_histogram("processing_latency_ms")
            .with_description("Histogram of alert processing latency in milliseconds")
            .init();

        let delivery_latency_ms = meter
            .f64_histogram("delivery_latency_ms")
            .with_description("Histogram of alert delivery latency in milliseconds")
            .init();

        // Observable gauge for retry queue size
        let retry_queue_size = meter
            .u64_observable_gauge("retry_queue_size")
            .with_description("Current size of retry queue")
            .init();

        // Error counter (aggregate, can be extended with labels)
        let error_count = meter
            .u64_counter("error_count_total")
            .with_description("Total number of errors encountered in processing")
            .init();

        let metrics = AlertingMetrics {
            meter,
            alerts_received,
            alerts_processed,
            alerts_dropped,
            processing_latency_ms,
            delivery_latency_ms,
            retry_queue_size,
            error_count,
            tenants_seen: Arc::new(RwLock::new(HashSet::new())),
            alert_types_seen: Arc::new(RwLock::new(HashSet::new())),
        };

        // Register observable callback for retry queue size
        // In actual code, replace with integration from retry_queue module.
        let retry_queue_size_ref = metrics.retry_queue_size.clone();
        let tenants_clone = metrics.tenants_seen.clone();
        metrics.meter.register_callback(move |cx| {
            // Example: set a dummy value, replace with actual queue size from repository
            // This is a stub implementation, real logic must query queue size.
            retry_queue_size_ref.observe(cx, 42, &[]);
        }).expect("Failed to register retry_queue_size callback");

        Arc::new(metrics)
    }

    /// Install global static metrics instance (call once at startup)
    pub fn install() -> Arc<Self> {
        GLOBAL_METRICS.get_or_init(|| Self::init()).clone()
    }

    /// Helper to update alerts_received metric with tenant and alert_type labels.
    /// Respects cardinality limits.
    pub async fn record_alert_received(&self, tenant: &str, alert_type: &str) {
        if !self.check_cardinality(tenant, alert_type).await {
            return; // skip metric update to avoid explosion
        }
        self.alerts_received.add(1, &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("alert_type", alert_type.to_string()),
        ]);
    }

    /// Helper to update alerts_processed metric with labels.
    pub async fn record_alert_processed(&self, tenant: &str, alert_type: &str) {
        if !self.check_cardinality(tenant, alert_type).await {
            return;
        }
        self.alerts_processed.add(1, &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("alert_type", alert_type.to_string()),
        ]);
    }

    /// Helper to update alerts_dropped metric with labels.
    pub async fn record_alert_dropped(&self, tenant: &str, alert_type: &str) {
        if !self.check_cardinality(tenant, alert_type).await {
            return;
        }
        self.alerts_dropped.add(1, &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("alert_type", alert_type.to_string()),
        ]);
    }

    /// Helper to record processing latency in milliseconds.
    pub fn record_processing_latency(&self, latency_ms: f64) {
        self.processing_latency_ms.record(latency_ms, &[]);
    }

    /// Helper to record delivery latency in milliseconds.
    pub fn record_delivery_latency(&self, latency_ms: f64) {
        self.delivery_latency_ms.record(latency_ms, &[]);
    }

    /// Helper to record an error occurrence with optional tenant label.
    pub async fn record_error(&self, tenant: Option<&str>) {
        let labels = if let Some(t) = tenant {
            if !self.tenants_seen.read().await.contains(t) && self.tenants_seen.read().await.len() > MAX_TENANTS {
                warn!("Max tenants cardinality exceeded, skipping error metric update");
                return;
            }
            vec![KeyValue::new("tenant", t.to_string())]
        } else {
            vec![]
        };
        self.error_count.add(1, &labels);
    }

    /// Cardinality control: track tenant and alert_type, return false if limits exceeded.
    async fn check_cardinality(&self, tenant: &str, alert_type: &str) -> bool {
        let mut tenants = self.tenants_seen.write().await;
        let mut alert_types = self.alert_types_seen.write().await;

        if !tenants.contains(tenant) {
            if tenants.len() >= MAX_TENANTS {
                warn!("Max tenants cardinality exceeded, skipping metric update");
                return false;
            }
            tenants.insert(tenant.to_string());
        }
        if !alert_types.contains(alert_type) {
            if alert_types.len() >= MAX_ALERT_TYPES {
                warn!("Max alert_types cardinality exceeded, skipping metric update");
                return false;
            }
            alert_types.insert(alert_type.to_string());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::runtime::Runtime;

    #[tokio::test]
    async fn test_cardinality_control() {
        let metrics = AlertingMetrics::init();
        // Insert MAX_TENANTS + 1 tenants to exceed limit
        for i in 0..(MAX_TENANTS + 1) {
            let tenant = format!("tenant{}", i);
            let alert_type = "test_alert";
            let allowed = metrics.check_cardinality(&tenant, alert_type).await;
            if i < MAX_TENANTS {
                assert!(allowed, "Should allow tenant {}", i);
            } else {
                assert!(!allowed, "Should reject tenant {}", i);
            }
        }
    }

    #[tokio::test]
    async fn record_and_check_metrics() {
        let metrics = AlertingMetrics::init();
        metrics.record_alert_received("tenant1", "typeA").await;
        metrics.record_alert_processed("tenant1", "typeA").await;
        metrics.record_alert_dropped("tenant1", "typeA").await;
        metrics.record_processing_latency(12.3);
        metrics.record_delivery_latency(45.6);
        metrics.record_error(Some("tenant1")).await;
        metrics.record_error(None).await;
    }
}
