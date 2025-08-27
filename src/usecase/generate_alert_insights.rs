// src/usecase/generate_alert_insights.rs
//! Generación de insights y análisis de alertas.
//!
//! Objetivos cubiertos:
//! - Agregación eficiente (batch + streaming) para métricas y tendencias.
//! - Arquitectura extensible para detección de anomalías / ML (pluggable).
//! - Reportes configurables por tenant y exportación (JSON).
//! - Soporte tanto real-time (stream) como batch processing.
//! - Anonimización y protección de datos sensibles en los reportes.
//!
//! Nota: este módulo proporciona un motor de ejemplo pensado para integrarse
//! en un sistema real. En producción reemplazar los componentes en memoria
//! por backends escalables (ClickHouse, Kafka Streams, Flink, etc.) y usar
//! modelos ML/feature store real.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fmt::Debug,
    sync::Arc,
};
use tokio::sync::{mpsc, RwLock};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

/// --- Tipos de dominio básicos usados por el motor de insights ---

/// Registro de alerta tal como llega o se lee del almacén histórico.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlertRecord {
    pub id: Uuid,
    pub tenant_id: String,
    pub severity: String,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub original_timestamp: DateTime<Utc>,
    pub delivered: bool,
    pub channel: Option<String>,
}

/// Resumen de métricas por ventana / agrupación
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct MetricsSummary {
    pub window_start: DateTime<Utc>,
    pub window_end: DateTime<Utc>,
    pub tenant_id: Option<String>, // None => global
    pub total_alerts: u64,
    pub by_severity: BTreeMap<String, u64>,
    pub by_event_type: BTreeMap<String, u64>,
    pub avg_time_to_delivery_ms: Option<f64>, // si aplica
}

/// Un "Insight" detectado por el motor: anomalías, tendencias, recomendaciones.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Insight {
    pub id: Uuid,
    pub tenant_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub kind: String,            // e.g. "anomaly", "trend", "sla_breach"
    pub description: String,
    pub severity_score: f64,     // 0..1 relative priority
    pub metadata: serde_json::Value,
}

/// Resultado de un job de reporte (exportable)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub id: Uuid,
    pub generated_at: DateTime<Utc>,
    pub tenant_id: Option<String>,
    pub metrics: MetricsSummary,
    pub insights: Vec<Insight>,
}

/// --- Interfaces pluggables / componentes extensibles ---

/// Interface para detectar anomalías (puede encapsular ML).
#[async_trait::async_trait]
pub trait AnomalyDetector: Send + Sync + 'static {
    /// Analiza una ventana de métricas y devuelve insights si detecta anomalías.
    async fn analyze_window(&self, metrics: &MetricsSummary) -> Vec<Insight>;
}

/// Detector trivial basado en z-score sobre counts (demo).
pub struct ZScoreAnomalyDetector {
    /// Lookback baseline window count map by tenant+severity or global.
    /// In-memory storage; in prod usar time-series DB or feature store.
    history_counts: Arc<RwLock<HashMap<String, Vec<u64>>>>, // key -> sliding history
    baseline_window: usize,
    threshold_z: f64,
}

impl ZScoreAnomalyDetector {
    pub fn new(baseline_window: usize, threshold_z: f64) -> Self {
        Self {
            history_counts: Arc::new(RwLock::new(HashMap::new())),
            baseline_window,
            threshold_z,
        }
    }

    fn calc_mean_std(values: &[u64]) -> Option<(f64, f64)> {
        if values.len() < 2 {
            return None;
        }
        let n = values.len() as f64;
        let sum: f64 = values.iter().map(|v| *v as f64).sum();
        let mean = sum / n;
        let var = values.iter().map(|v| {
            let d = *v as f64 - mean;
            d * d
        }).sum::<f64>() / (n - 1.0);
        let std = var.sqrt();
        Some((mean, std))
    }
}

#[async_trait::async_trait]
impl AnomalyDetector for ZScoreAnomalyDetector {
    async fn analyze_window(&self, metrics: &MetricsSummary) -> Vec<Insight> {
        // For demonstration: compute anomalies on total_alerts for tenant (or global)
        let key = format!(
            "{}::{}",
            metrics.tenant_id.clone().unwrap_or_else(|| "global".into()),
            metrics.window_start.timestamp()
        );

        // For baseline key, we use tenant + "total"
        let baseline_key = metrics.tenant_id.clone().unwrap_or_else(|| "global".into());

        // Update history and compute z-score
        let mut history = self.history_counts.write().await;
        let list = history.entry(baseline_key.clone()).or_insert_with(Vec::new);

        // keep sliding window
        list.push(metrics.total_alerts);
        if list.len() > self.baseline_window {
            list.remove(0);
        }

        let mut insights = Vec::new();
        if let Some((mean, std)) = Self::calc_mean_std(list) {
            if std > 0.0 {
                let z = (metrics.total_alerts as f64 - mean) / std;
                if z.abs() >= self.threshold_z {
                    insights.push(Insight {
                        id: Uuid::new_v4(),
                        tenant_id: metrics.tenant_id.clone(),
                        created_at: Utc::now(),
                        kind: "anomaly".into(),
                        description: format!("Alert volume anomaly: z={:.2}, value={}, mean={:.1}, std={:.1}", z, metrics.total_alerts, mean, std),
                        severity_score: (z.abs() / 10.0).min(1.0),
                        metadata: serde_json::json!({
                            "z_score": z,
                            "total": metrics.total_alerts,
                            "baseline_mean": mean,
                            "baseline_std": std,
                        }),
                    });
                }
            }
        }

        insights
    }
}

/// Interface para exporters (BI, SIEM, dashboards)
#[async_trait::async_trait]
pub trait InsightsExporter: Send + Sync + 'static {
    async fn export_report(&self, report: &Report) -> anyhow::Result<()>;
}

/// Simple stdout exporter for demo
pub struct StdoutExporter;

#[async_trait::async_trait]
impl InsightsExporter for StdoutExporter {
    async fn export_report(&self, report: &Report) -> anyhow::Result<()> {
        println!("--- Report {} (tenant {:?}) ---", report.id, report.tenant_id);
        println!("Generated at: {}", report.generated_at);
        println!("Metrics: {:?}", report.metrics);
        println!("Insights: {:?}", report.insights);
        Ok(())
    }
}

/// --- Core engine: Aggregator + Stream processor --- 

/// Configuration for the insights engine
#[derive(Clone)]
pub struct InsightsConfig {
    pub window_size_secs: i64,
    pub max_queue: usize,
    pub anonymize_fields: Vec<String>, // payload keys to redact
}

/// Engine that accepts incoming alerts (streaming) and also runs batch jobs.
pub struct InsightsEngine {
    cfg: InsightsConfig,
    detector: Arc<dyn AnomalyDetector>,
    exporter: Arc<dyn InsightsExporter>,

    // streaming channel where ingestion pushes AlertRecord
    sender: mpsc::Sender<AlertRecord>,

    // internal store for recent alerts per tenant (windowed)
    // in-memory store: tenant -> vec<AlertRecord> (append-only per window)
    store: Arc<RwLock<HashMap<String, Vec<AlertRecord>>>>,
}

impl InsightsEngine {
    /// Crear engine con detector y exporter inyectados.
    pub fn new(cfg: InsightsConfig, detector: Arc<dyn AnomalyDetector>, exporter: Arc<dyn InsightsExporter>) -> Self {
        let (tx, rx) = mpsc::channel(cfg.max_queue);
        let engine = Self {
            cfg,
            detector,
            exporter,
            sender: tx,
            store: Arc::new(RwLock::new(HashMap::new())),
        };
        engine.spawn_stream_worker(rx);
        engine
    }

    /// Public API: ingest an alert (streaming). Non-blocking.
    pub async fn ingest(&self, alert: AlertRecord) -> anyhow::Result<()> {
        // best-effort send; if queue full, drop or fallback (could push to durable queue)
        match self.sender.try_send(alert) {
            Ok(_) => Ok(()),
            Err(e) => {
                // Queue full -> record metric or return error. For now return Err.
                Err(anyhow::anyhow!("ingest queue full: {}", e))
            }
        }
    }

    /// Spawn background worker that consumes alerts and aggregates into windows.
    fn spawn_stream_worker(&self, mut rx: mpsc::Receiver<AlertRecord>) {
        let cfg = self.cfg.clone();
        let detector = Arc::clone(&self.detector);
        let exporter = Arc::clone(&self.exporter);
        let store = Arc::clone(&self.store);

        tokio::spawn(async move {
            // sliding window tick
            let mut window_start = Utc::now();
            let mut buffer: Vec<AlertRecord> = Vec::new();
            let window_dur = chrono::Duration::seconds(cfg.window_size_secs);

            loop {
                // wait for either new alert or window timeout
                tokio::select! {
                    maybe = rx.recv() => {
                        if let Some(alert) = maybe {
                            buffer.push(alert);
                            // if buffer grows too large, flush partial or drop; here we cap
                            if buffer.len() > 10_000 {
                                // flush early to avoid memory explosion
                                let batch = std::mem::take(&mut buffer);
                                process_window_batch(batch, &store, &detector, &exporter, cfg.anonymize_fields.clone(), window_start, window_start + window_dur).await;
                            }
                        } else {
                            // channel closed -> drain and exit
                            if !buffer.is_empty() {
                                let batch = std::mem::take(&mut buffer);
                                process_window_batch(batch, &store, &detector, &exporter, cfg.anonymize_fields.clone(), window_start, window_start + window_dur).await;
                            }
                            break;
                        }
                    }
                    _ = sleep(std::time::Duration::from_secs(cfg.window_size_secs as u64)) => {
                        // time to flush current window
                        let batch = std::mem::take(&mut buffer);
                        let window_end = window_start + window_dur;
                        process_window_batch(batch, &store, &detector, &exporter, cfg.anonymize_fields.clone(), window_start, window_end).await;
                        // advance window
                        window_start = Utc::now();
                    }
                }
            }
        });
    }

    /// Ejecuta job batch on-demand: procesa un conjunto de alertas (e.g. historical)
    /// y genera un exportable Report. Esta función respeta anonymize_fields.
    pub async fn run_batch_report(&self, alerts: Vec<AlertRecord>, tenant_id: Option<String>) -> anyhow::Result<Report> {
        // filter by tenant if provided
        let filtered: Vec<_> = if let Some(tid) = tenant_id.clone() {
            alerts.into_iter().filter(|a| a.tenant_id == tid).collect()
        } else {
            alerts
        };

        // aggregate metrics
        let now = Utc::now();
        let metrics = aggregate_metrics_window(&filtered, now - chrono::Duration::minutes(5), now).await;

        // anonymize payloads (don't modify originals)
        let _ = anonymize_alerts(&filtered, &self.cfg.anonymize_fields).await;

        // detect insights (sync call to detector)
        let insights = self.detector.analyze_window(&metrics).await;

        let report = Report {
            id: Uuid::new_v4(),
            generated_at: Utc::now(),
            tenant_id,
            metrics,
            insights,
        };

        // export
        let _ = self.exporter.export_report(&report).await;

        Ok(report)
    }

    /// Obtain a snapshot of recent aggregated metrics per tenant
    pub async fn metrics_snapshot(&self) -> HashMap<String, MetricsSummary> {
        let s = self.store.read().await;
        let mut out = HashMap::new();
        for (tenant, alerts) in s.iter() {
            // aggregate last window sized period
            let end = Utc::now();
            let start = end - chrono::Duration::seconds(self.cfg.window_size_secs);
            // basic aggregation
            let mut summary = MetricsSummary {
                window_start: start,
                window_end: end,
                tenant_id: Some(tenant.clone()),
                total_alerts: 0,
                by_severity: BTreeMap::new(),
                by_event_type: BTreeMap::new(),
                avg_time_to_delivery_ms: None,
            };
            for a in alerts.iter().filter(|a| a.original_timestamp >= start && a.original_timestamp <= end) {
                summary.total_alerts += 1;
                *summary.by_severity.entry(a.severity.clone()).or_insert(0) += 1;
                *summary.by_event_type.entry(a.event_type.clone()).or_insert(0) += 1;
            }
            out.insert(tenant.clone(), summary);
        }
        out
    }
}

/// --- Helper functions used by the engine --- 

/// Process a batch that corresponds to a window: persist to store and call detector/exporter
async fn process_window_batch(
    batch: Vec<AlertRecord>,
    store: &Arc<RwLock<HashMap<String, Vec<AlertRecord>>>>,
    detector: &Arc<dyn AnomalyDetector>,
    exporter: &Arc<dyn InsightsExporter>,
    anonymize_fields: Vec<String>,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) {
    if batch.is_empty() {
        return;
    }
    // group by tenant and persist to in-memory store (append)
    let mut grouped: HashMap<String, Vec<AlertRecord>> = HashMap::new();
    for mut a in batch.into_iter() {
        // redact sensitive fields on payload copy
        redact_payload_inplace(&mut a.payload, &anonymize_fields);
        grouped.entry(a.tenant_id.clone()).or_default().push(a);
    }

    // persist grouped
    {
        let mut s = store.write().await;
        let now = Utc::now();
        for (tenant, alerts) in grouped.into_iter() {
            let vec = s.entry(tenant.clone()).or_insert_with(Vec::new);
            // retain only recent N to avoid unbounded growth in this demo
            vec.extend(alerts.clone());
            // optional: truncate to last 10k items
            if vec.len() > 10_000 {
                let remove = vec.len() - 10_000;
                vec.drain(0..remove);
            }

            // aggregate metrics for this tenant and window
            let metrics = aggregate_metrics_window(&alerts, window_start, window_end).await;

            // detect anomalies
            let insights = detector.analyze_window(&metrics).await;

            // produce a report object and export
            let report = Report {
                id: Uuid::new_v4(),
                generated_at: now,
                tenant_id: Some(tenant.clone()),
                metrics,
                insights,
            };

            // fire-and-forget export
            let exporter = Arc::clone(exporter);
            tokio::spawn(async move {
                if let Err(e) = exporter.export_report(&report).await {
                    tracing::error!("export report failed: {:?}", e);
                }
            });
        }
    }
}

/// Aggregate a collection of alerts into MetricsSummary for the given window.
async fn aggregate_metrics_window(alerts: &[AlertRecord], window_start: DateTime<Utc>, window_end: DateTime<Utc>) -> MetricsSummary {
    let mut summary = MetricsSummary {
        window_start,
        window_end,
        tenant_id: alerts.first().map(|a| a.tenant_id.clone()),
        total_alerts: 0,
        by_severity: BTreeMap::new(),
        by_event_type: BTreeMap::new(),
        avg_time_to_delivery_ms: None,
    };

    let mut total_latency_ms: u64 = 0;
    let mut latency_count: u64 = 0;

    for a in alerts.iter().filter(|a| a.original_timestamp >= window_start && a.original_timestamp <= window_end) {
        summary.total_alerts += 1;
        *summary.by_severity.entry(a.severity.clone()).or_insert(0) += 1;
        *summary.by_event_type.entry(a.event_type.clone()).or_insert(0) += 1;
        // if payload has delivery_time_ms, accumulate
        if let Some(d) = a.payload.get("delivery_time_ms").and_then(|v| v.as_u64()) {
            total_latency_ms += d;
            latency_count += 1;
        }
    }

    if latency_count > 0 {
        summary.avg_time_to_delivery_ms = Some(total_latency_ms as f64 / latency_count as f64);
    }

    summary
}

/// Simple anonymization: remove specific keys from JSON payload (in place).
async fn anonymize_alerts(alerts: &[AlertRecord], fields: &[String]) -> Vec<AlertRecord> {
    let mut out = Vec::with_capacity(alerts.len());
    for mut a in alerts.iter().cloned() {
        redact_payload_inplace(&mut a.payload, fields);
        out.push(a);
    }
    out
}

/// Redact helper (mutates JSON payload)
fn redact_payload_inplace(payload: &mut serde_json::Value, fields: &[String]) {
    if let serde_json::Value::Object(map) = payload {
        for f in fields {
            if map.contains_key(f) {
                map.insert(f.clone(), serde_json::Value::String("[REDACTED]".into()));
            }
        }
    }
}

/// --------------------
/// Example usage (demo)
/// --------------------
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Build engine with config
    let cfg = InsightsConfig {
        window_size_secs: 10, // short window for demo
        max_queue: 10_000,
        anonymize_fields: vec!["ssn".into(), "email".into()],
    };

    let detector = Arc::new(ZScoreAnomalyDetector::new(5, 2.5));
    let exporter = Arc::new(StdoutExporter);

    let engine = InsightsEngine::new(cfg, detector, exporter);

    // Simulate streaming ingestion
    for i in 0..50 {
        let alert = AlertRecord {
            id: Uuid::new_v4(),
            tenant_id: if i % 2 == 0 { "tenant-a".into() } else { "tenant-b".into() },
            severity: if i % 10 == 0 { "critical".into() } else { "info".into() },
            event_type: if i % 5 == 0 { "payment.failure".into() } else { "heartbeat".into() },
            payload: serde_json::json!({
                "x": i,
                "delivery_time_ms": (i % 7) * 10,
                "email": "user@example.com",
                "ssn": "123-45-6789"
            }),
            original_timestamp: Utc::now(),
            delivered: i % 3 == 0,
            channel: Some("email".into()),
        };

        // best-effort ingestion
        let _ = engine.ingest(alert).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Run a batch report for tenant-a (dry-run)
    let sample_alerts = vec![]; // in real use-case, fetch from alert store
    let _ = engine.run_batch_report(sample_alerts, Some("tenant-a".into())).await?;

    // Give time for background worker to process windows in demo
    tokio::time::sleep(std::time::Duration::from_secs(15)).await;

    // Snapshot metrics
    let snap = engine.metrics_snapshot().await;
    println!("Metrics snapshot: {:?}", snap);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDur;

    #[tokio::test]
    async fn test_aggregation_basic() {
        let now = Utc::now();
        let a1 = AlertRecord {
            id: Uuid::new_v4(),
            tenant_id: "t1".into(),
            severity: "critical".into(),
            event_type: "payment.failure".into(),
            payload: serde_json::json!({"delivery_time_ms": 200}),
            original_timestamp: now - ChronoDur::seconds(5),
            delivered: true,
            channel: Some("email".into()),
        };
        let a2 = AlertRecord {
            id: Uuid::new_v4(),
            tenant_id: "t1".into(),
            severity: "info".into(),
            event_type: "heartbeat".into(),
            payload: serde_json::json!({}),
            original_timestamp: now - ChronoDur::seconds(4),
            delivered: false,
            channel: Some("slack".into()),
        };

        let metrics = aggregate_metrics_window(&[a1.clone(), a2.clone()], now - ChronoDur::seconds(10), now).await;
        assert_eq!(metrics.total_alerts, 2);
        assert_eq!(*metrics.by_severity.get("critical").unwrap(), 1);
        assert!(metrics.avg_time_to_delivery_ms.is_some());
    }

    #[tokio::test]
    async fn test_anomaly_detector_zscore() {
        let detector = ZScoreAnomalyDetector::new(3, 1.5);
        // create a sequence of windows to populate baseline
        for i in 0..3 {
            let m = MetricsSummary {
                window_start: Utc::now(),
                window_end: Utc::now(),
                tenant_id: Some("t".into()),
                total_alerts: 10 + i,
                by_severity: BTreeMap::new(),
                by_event_type: BTreeMap::new(),
                avg_time_to_delivery_ms: None,
            };
            let _ = detector.analyze_window(&m).await;
        }
        // now a spike
        let spike = MetricsSummary {
            window_start: Utc::now(),
            window_end: Utc::now(),
            tenant_id: Some("t".into()),
            total_alerts: 100,
            by_severity: BTreeMap::new(),
            by_event_type: BTreeMap::new(),
            avg_time_to_delivery_ms: None,
        };
        let insights = detector.analyze_window(&spike).await;
        assert!(!insights.is_empty());
    }
}
