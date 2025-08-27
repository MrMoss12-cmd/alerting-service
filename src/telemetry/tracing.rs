// src/telemetry/tracing.rs
//! Distributed tracing setup for alerting-service.
//!
//! Responsibilities:
//! - Enable context propagation across microservices using W3C Trace Context.
//! - Provide detailed annotations for critical operations (notifications, retries, purges).
//! - Integrate tightly with metrics and logging for fast diagnostics.
//! - Enrich spans with attributes like tenant, alert_id, channel, enqueue_time.
//! - Support dynamic sampling to control trace volume in production.
//! - Fail gracefully if tracing backend is down, avoiding blocking critical ops.
//! - Facilitate filtering by tenant, channel, or event for troubleshooting.

use opentelemetry::{
    global,
    sdk::{
        trace as sdktrace,
        Resource,
    },
    trace::{Tracer, TraceError},
    KeyValue,
};
use tracing_subscriber::{prelude::*, EnvFilter, Registry};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing::{info, instrument};
use std::sync::Arc;

/// Initializes and returns a configured Tracer and tracing subscriber layer.
///
/// # Arguments
/// * `service_name` - The name of this microservice (e.g., "alerting_service").
/// * `service_version` - Current version of the service.
/// * `otel_collector_endpoint` - OTLP/gRPC endpoint URL for exporting traces.
/// * `enable_debug` - Enable verbose tracing for development/testing.
/// * `sampling_ratio` - Between 0.0 and 1.0, fraction of traces to sample.
///
/// # Returns
/// On success, returns a `Tracer` instance for manual span creation.
pub async fn init_tracing(
    service_name: &str,
    service_version: &str,
    otel_collector_endpoint: &str,
    enable_debug: bool,
    sampling_ratio: f64,
) -> Result<Arc<sdktrace::Tracer>, TraceError> {
    // Build resource describing this service (service.name, service.version)
    let resource = Resource::new(vec![
        KeyValue::new("service.name", service_name.to_string()),
        KeyValue::new("service.version", service_version.to_string()),
    ]);

    // Setup sampler (parent based + probabilistic)
    let sampler = sdktrace::Sampler::ParentBased(Box::new(sdktrace::Sampler::TraceIdRatioBased(sampling_ratio)));

    // Configure OTLP exporter with TLS (mTLS can be added here if needed)
    let exporter = opentelemetry_otlp::new_exporter()
        .grpc_endpoint(otel_collector_endpoint)
        .with_tls_config(
            // Add TLS config for mTLS here if needed
            opentelemetry_otlp::TlsConfig::default()
        );

    // Configure tracer pipeline
    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(exporter)
        .with_trace_config(sdktrace::Config::default().with_resource(resource).with_sampler(sampler))
        .install_batch(opentelemetry::runtime::Tokio)?;

    // Create tracing subscriber layer for tracing integration with tokio
    let otel_layer = OpenTelemetryLayer::new(tracer.clone());

    // Setup environment filter from RUST_LOG or default level
    let env_filter = if enable_debug {
        EnvFilter::new("debug")
    } else {
        EnvFilter::new("info")
    };

    // Compose subscriber
    let subscriber = Registry::default()
        .with(env_filter)
        .with(otel_layer);

    tracing::subscriber::set_global_default(subscriber)
        .expect("Failed to set global tracing subscriber");

    Ok(Arc::new(tracer))
}

/// Helper function to enrich spans with common attributes related to alerts.
///
/// Typical usage:
/// ```
/// #[instrument(fields(tenant = %tenant, alert_id = %alert_id, channel = %channel))]
/// async fn process_alert(...) { ... }
/// ```
///
/// You can also add these attributes programmatically:
///
/// ```
/// let span = tracing::info_span!("alert_processing");
/// span.record("tenant", &tenant);
/// span.record("alert_id", &alert_id);
/// span.record("channel", &channel);
/// span.record("enqueue_time", &enqueue_time.to_string());
/// ```
pub fn enrich_span(
    tenant: &str,
    alert_id: &str,
    channel: &str,
    enqueue_time: &str,
) {
    let span = tracing::Span::current();
    span.record("tenant", &tenant);
    span.record("alert_id", &alert_id);
    span.record("channel", &channel);
    span.record("enqueue_time", &enqueue_time);
}

/// Gracefully shutdown the tracer provider.
///
/// Ensures all pending traces are flushed before shutdown.
pub async fn shutdown_tracer() {
    global::shutdown_tracer_provider();
}
