// src/usecase/retry_failed_deliveries.rs
//! Reintento de entregas fallidas (Retry Manager)
//!
//! Este módulo proporciona un componente que programa y ejecuta reintentos
//! para entregas fallidas por canal, respetando políticas configurables
//! (backoff exponencial, max_retries, cooldown), implementando circuit-breakers
//! por canal, manteniendo historial de intentos y exponiendo métricas simples.
//!
//! Está diseñado para integrarse en un servicio Tokio: el `RetryManager` corre
//! un worker asincrónico que procesa entradas pendientes y ejecuta reintentos
//! aislados por canal.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::RwLock, task::JoinHandle, time::sleep};
use uuid::Uuid;

/// Canales soportados (extensible)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NotificationChannel {
    Email,
    Sms,
    Slack,
    PagerDuty,
    Custom(String),
}

impl ToString for NotificationChannel {
    fn to_string(&self) -> String {
        match self {
            NotificationChannel::Email => "email".into(),
            NotificationChannel::Sms => "sms".into(),
            NotificationChannel::Slack => "slack".into(),
            NotificationChannel::PagerDuty => "pagerduty".into(),
            NotificationChannel::Custom(n) => n.clone(),
        }
    }
}

/// Resultado de intento de entrega
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub attempt_number: u32,
    pub timestamp: DateTime<Utc>,
    pub success: bool,
    pub duration_ms: Option<u64>,
    pub error: Option<String>, // redacted / summarized
}

/// Estado por canal para una alerta
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelDeliveryState {
    pub alert_id: Uuid,
    pub channel: NotificationChannel,
    pub attempts: Vec<AttemptRecord>,
    pub state: DeliveryState,
    pub max_retries: u32,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
    pub circuit_failures: u32,            // contador de fallos recientes (ventana)
    pub circuit_open_until: Option<DateTime<Utc>>,
    pub last_scheduled_at: Option<DateTime<Utc>>,
    pub cooldown_until: Option<DateTime<Utc>>, // período después de fallos para pausar reintentos
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryState {
    Pending,
    Retrying,
    Delivered,
    FailedPermanent,
    Skipped,
}

impl ChannelDeliveryState {
    pub fn new(alert_id: Uuid, channel: NotificationChannel) -> Self {
        Self {
            alert_id,
            channel,
            attempts: Vec::new(),
            state: DeliveryState::Pending,
            max_retries: 5,
            backoff_base_ms: 500,
            backoff_max_ms: 60_000,
            circuit_failures: 0,
            circuit_open_until: None,
            last_scheduled_at: None,
            cooldown_until: None,
        }
    }

    /// Computa backoff exponencial con tope (ms)
    pub fn compute_backoff_ms(&self) -> u64 {
        let attempts_done = self.attempts.len() as u32;
        // próximo intento exponencial: base * 2^(attempts_done)
        let exp = attempts_done.saturating_sub(0); // 0-based exponent
        let base = self.backoff_base_ms as u128;
        let mult = base.saturating_shl(exp as u32); // base * 2^exp
        let ms = std::cmp::min(mult as u64, self.backoff_max_ms);
        std::cmp::max(ms, self.backoff_base_ms)
    }

    pub fn attempts_done(&self) -> u32 {
        self.attempts.len() as u32
    }

    pub fn should_retry(&self) -> bool {
        if self.state == DeliveryState::Delivered || self.state == DeliveryState::FailedPermanent || self.state == DeliveryState::Skipped {
            return false;
        }

        if let Some(until) = self.circuit_open_until {
            if until > Utc::now() {
                return false;
            }
        }

        if let Some(cd) = self.cooldown_until {
            if cd > Utc::now() {
                return false;
            }
        }

        self.attempts_done() < self.max_retries
    }

    /// Marca intento y actualiza estado
    pub fn record_attempt(&mut self, success: bool, duration_ms: Option<u64>, error: Option<String>) {
        let attempt = AttemptRecord {
            attempt_number: (self.attempts.len() as u32) + 1,
            timestamp: Utc::now(),
            success,
            duration_ms,
            error,
        };
        self.attempts.push(attempt);
        if success {
            self.state = DeliveryState::Delivered;
            self.circuit_failures = 0;
            self.circuit_open_until = None;
            self.cooldown_until = None;
        } else {
            if self.attempts_done() >= self.max_retries {
                self.state = DeliveryState::FailedPermanent;
            } else {
                self.state = DeliveryState::Retrying;
            }
            // Incrementar contador de fallos para circuito
            self.circuit_failures = self.circuit_failures.saturating_add(1);
        }
        self.last_scheduled_at = Some(Utc::now());
    }

    /// Abrir circuito por N segundos
    pub fn open_circuit_for(&mut self, seconds: i64) {
        self.circuit_open_until = Some(Utc::now() + chrono::Duration::seconds(seconds));
    }

    /// Establecer cooldown (pausa) por N segundos
    pub fn set_cooldown_for(&mut self, seconds: i64) {
        self.cooldown_until = Some(Utc::now() + chrono::Duration::seconds(seconds));
    }
}

/// Métricas básicas atómicas para exponer
#[derive(Debug, Default)]
pub struct RetryMetrics {
    pub total_retries: AtomicU64,
    pub total_successful_retries: AtomicU64,
    pub total_failed_retries: AtomicU64,
    pub total_permanent_failures: AtomicU64,
}

impl RetryMetrics {
    pub fn inc_retries(&self) {
        self.total_retries.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_success(&self) {
        self.total_successful_retries.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_failed(&self) {
        self.total_failed_retries.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_permanent_fail(&self) {
        self.total_permanent_failures.fetch_add(1, Ordering::Relaxed);
    }
}

/// Interfaz abstracta para ejecutar el envío real a un canal.
/// En integración real esto sería un trait con implementaciones para Email/SMS/Slack.
/// Aquí la definimos como función asíncrona que el caller inyecta.
pub type SendFn = Arc<dyn Fn(Uuid, NotificationChannel, serde_json::Value) -> BoxFutureSendResult + Send + Sync>;
pub type BoxFutureSendResult = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;

/// RetryManager mantiene los items pendientes y un worker que intenta reintentos.
pub struct RetryManager {
    /// Mapa (alert_id, channel) -> ChannelDeliveryState
    store: Arc<RwLock<HashMap<(Uuid, NotificationChannel), ChannelDeliveryState>>>,
    metrics: Arc<RetryMetrics>,
    send_fn: SendFn,
    worker_handle: Option<JoinHandle<()>>,
    stop_signal: Arc<RwLock<bool>>,
}

impl RetryManager {
    pub fn new(send_fn: SendFn) -> Self {
        Self {
            store: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(RetryMetrics::default()),
            send_fn,
            worker_handle: None,
            stop_signal: Arc::new(RwLock::new(false)),
        }
    }

    /// Inserta un fallo inicial en la cola de reintentos (por ejemplo, desde dispatcher).
    pub async fn enqueue_failure(
        &self,
        alert_id: Uuid,
        channel: NotificationChannel,
        max_retries: Option<u32>,
        backoff_base_ms: Option<u64>,
        backoff_max_ms: Option<u64>,
        initial_error: Option<String>,
        payload: serde_json::Value,
    ) {
        let key = (alert_id, channel.clone());
        let mut store = self.store.write().await;
        let mut state = store
            .entry(key.clone())
            .or_insert_with(|| ChannelDeliveryState::new(alert_id, channel.clone()));

        if let Some(mr) = max_retries {
            state.max_retries = mr;
        }
        if let Some(b) = backoff_base_ms {
            state.backoff_base_ms = b;
        }
        if let Some(m) = backoff_max_ms {
            state.backoff_max_ms = m;
        }

        // registrar intento fallido inicial
        state.record_attempt(false, None, initial_error);

        // schedule immediate next attempt (worker will see it)
        state.last_scheduled_at = Some(Utc::now());

        // store the payload in a separate map if needed. For simplicity, we store inline metadata in attempts
        // but real impl should persist payload in durable storage or reference id.

        // ensure the entry exists
        store.insert(key, state.clone());

        // increment metric
        self.metrics.inc_retries();
    }

    /// Marca éxito externo (por ejemplo dispatcher) para limpiar el item.
    pub async fn mark_delivered(&self, alert_id: Uuid, channel: NotificationChannel) {
        let key = (alert_id, channel.clone());
        let mut store = self.store.write().await;
        if let Some(mut state) = store.remove(&key) {
            state.state = DeliveryState::Delivered;
            self.metrics.inc_success();
        }
    }

    /// Inicia worker background que procesa reintentos hasta que se solicite detener.
    pub fn start(&mut self) {
        let store = Arc::clone(&self.store);
        let metrics = Arc::clone(&self.metrics);
        let send_fn = Arc::clone(&self.send_fn);
        let stop_signal = Arc::clone(&self.stop_signal);

        let handle = tokio::spawn(async move {
            loop {
                // check stop
                if *stop_signal.read().await {
                    tracing::info!("RetryManager worker received stop signal");
                    break;
                }

                // snapshot keys to avoid holding write lock long
                let keys: Vec<(Uuid, NotificationChannel)> = {
                    let s = store.read().await;
                    s.keys().cloned().collect()
                };

                for key in keys {
                    // check stop mid-loop
                    if *stop_signal.read().await {
                        break;
                    }

                    // scope to reduce lock time
                    let mut maybe_state = {
                        let s = store.read().await;
                        s.get(&key).cloned()
                    };

                    if maybe_state.is_none() {
                        continue;
                    }
                    let mut state = maybe_state.take().unwrap();

                    // skip if not eligible
                    if !state.should_retry() {
                        // if permanent failed, increment metric & possibly cleanup
                        if state.state == DeliveryState::FailedPermanent {
                            metrics.inc_permanent_fail();
                            // optionally remove permanent failures after retention policy; keep here for auditing
                        }
                        continue;
                    }

                    // if circuit open or cooldown, skip
                    if state.circuit_open_until.map_or(false, |t| t > Utc::now()) {
                        continue;
                    }
                    if state.cooldown_until.map_or(false, |t| t > Utc::now()) {
                        continue;
                    }

                    // compute backoff and possibly wait before trying
                    let backoff_ms = state.compute_backoff_ms();
                    // if last_scheduled_at exists and not enough time passed, skip for now
                    if let Some(last) = state.last_scheduled_at {
                        let elapsed = (Utc::now() - last).to_std().unwrap_or(Duration::from_secs(0));
                        if elapsed < Duration::from_millis(backoff_ms) {
                            continue;
                        }
                    }

                    // Attempt send: in a real system we'd fetch payload from durable store.
                    // Here we call the injected send_fn and pass a minimal payload (empty object).
                    metrics.inc_retries();
                    let send_future = (send_fn)(state.alert_id, state.channel.clone(), serde_json::json!({}));
                    let res = send_future.await;

                    match res {
                        Ok(_) => {
                            // success: record, remove state
                            state.record_attempt(true, None, None);
                            metrics.inc_success();

                            // persist updated state and remove from store
                            let mut s = store.write().await;
                            s.remove(&key);
                        }
                        Err(err_msg) => {
                            // failure: record attempt, update circuit behavior
                            state.record_attempt(false, None, Some(err_msg.clone()));
                            metrics.inc_failed();

                            // simple circuit-breaker logic: if failures exceed threshold, open circuit
                            if state.circuit_failures >= 3 {
                                state.open_circuit_for(30); // open for 30s
                                state.set_cooldown_for(60); // cooldown 60s
                            }

                            // if reached max_retries, mark permanent fail
                            if state.attempts_done() >= state.max_retries {
                                state.state = DeliveryState::FailedPermanent;
                                metrics.inc_permanent_fail();
                            }

                            // persist updated state
                            let mut s = store.write().await;
                            s.insert(key.clone(), state.clone());
                        }
                    } // match send result
                } // for each key

                // periodic sleep to avoid busy loop; tune as needed
                sleep(Duration::from_millis(500)).await;
            } // loop
        });

        self.worker_handle = Some(handle);
    }

    /// Solicita al worker detenerse y espera su finalización.
    pub async fn stop(&mut self) {
        {
            let mut sig = self.stop_signal.write().await;
            *sig = true;
        }
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.await;
        }
    }

    /// Recuperar métricas en snapshot (útil para exportar)
    pub fn metrics_snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.metrics.total_retries.load(Ordering::Relaxed),
            self.metrics.total_successful_retries.load(Ordering::Relaxed),
            self.metrics.total_failed_retries.load(Ordering::Relaxed),
            self.metrics.total_permanent_failures.load(Ordering::Relaxed),
        )
    }

    /// Obtener estado para un alert/channel (útil para API)
    pub async fn get_state(&self, alert_id: Uuid, channel: &NotificationChannel) -> Option<ChannelDeliveryState> {
        let s = self.store.read().await;
        s.get(&(alert_id, channel.clone())).cloned()
    }

    /// Listar todos los pendientes ( snapshot )
    pub async fn list_pending(&self) -> Vec<ChannelDeliveryState> {
        let s = self.store.read().await;
        s.values().cloned().collect()
    }
}

impl Drop for RetryManager {
    fn drop(&mut self) {
        // best-effort: signal worker to stop; worker will check stop_signal periodically
        // but we cannot await here since Drop is sync; rely on tokio runtime shutdown.
        if let Some(handle) = &self.worker_handle {
            handle.abort();
        }
    }
}

// ---------------------------
// Ejemplo de uso (main)
// ---------------------------

#[tokio::main]
async fn main() {
    // Simple send_fn: simulate send with random success/failure
    let send_fn: SendFn = Arc::new(move |alert_id, channel, _payload| {
        Box::pin(async move {
            // simulate latency and probabilistic failure
            let latency = rand::random::<u64>() % 200 + 20;
            sleep(Duration::from_millis(latency)).await;
            if rand::random::<f64>() < 0.75 {
                tracing::info!("Simulated send success for {} -> {}", alert_id, channel.to_string());
                Ok(())
            } else {
                tracing::warn!("Simulated send failure for {} -> {}", alert_id, channel.to_string());
                Err(format!("simulated_failure_{}", channel.to_string()))
            }
        })
    });

    let mut manager = RetryManager::new(send_fn);

    // Start background worker
    manager.start();

    // Enqueue a few simulated failures
    let alert1 = Uuid::new_v4();
    manager
        .enqueue_failure(
            alert1,
            NotificationChannel::Email,
            Some(4),
            Some(200),
            Some(5_000),
            Some("initial_timeout".into()),
            serde_json::json!({}),
        )
        .await;

    let alert2 = Uuid::new_v4();
    manager
        .enqueue_failure(
            alert2,
            NotificationChannel::Slack,
            Some(3),
            Some(500),
            Some(30_000),
            Some("initial_502".into()),
            serde_json::json!({}),
        )
        .await;

    // Let it run for a short while to demonstrate retries
    sleep(Duration::from_secs(10)).await;

    // Inspect metrics
    let m = manager.metrics_snapshot();
    println!(
        "Metrics: total_retries={}, success={}, failed={}, permanent_fail={}",
        m.0, m.1, m.2, m.3
    );

    // Stop manager gracefully
    manager.stop().await;
}
