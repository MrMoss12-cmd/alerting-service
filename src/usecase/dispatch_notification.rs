use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{sync::RwLock, time::sleep};
use uuid::Uuid;
use chrono::{DateTime, Utc};
use rand::Rng;

// Estados posibles de entrega
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryState {
    Pending,
    Delivered,
    Failed(String), // Motivo del fallo (sin datos sensibles)
    Retrying(u32),  // Número de intento
}

impl DeliveryState {
    pub fn is_final(&self) -> bool {
        matches!(self, DeliveryState::Delivered | DeliveryState::Failed(_))
    }
}

// Canales soportados (simplificado)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
            NotificationChannel::Email => "Email".to_string(),
            NotificationChannel::Sms => "SMS".to_string(),
            NotificationChannel::Slack => "Slack".to_string(),
            NotificationChannel::PagerDuty => "PagerDuty".to_string(),
            NotificationChannel::Custom(name) => name.clone(),
        }
    }
}

// Registro de intento por canal
#[derive(Debug, Clone)]
pub struct DeliveryAttempt {
    pub timestamp: DateTime<Utc>,
    pub state: DeliveryState,
    pub latency_ms: Option<u128>, // Latencia en ms si se puede medir
}

// Estado de entrega por canal
#[derive(Debug, Clone)]
pub struct ChannelDeliveryStatus {
    pub channel: NotificationChannel,
    pub current_state: DeliveryState,
    pub attempts: Vec<DeliveryAttempt>,
}

impl ChannelDeliveryStatus {
    pub fn new(channel: NotificationChannel) -> Self {
        Self {
            channel,
            current_state: DeliveryState::Pending,
            attempts: Vec::new(),
        }
    }

    pub fn record_attempt(&mut self, state: DeliveryState, latency_ms: Option<u128>) {
        self.current_state = state.clone();
        self.attempts.push(DeliveryAttempt {
            timestamp: Utc::now(),
            state,
            latency_ms,
        });
    }
}

// Evento alerta básico (solo lo necesario)
#[derive(Debug, Clone)]
pub struct AlertEvent {
    pub id: Uuid,
    pub tenant_id: String,
    pub payload: serde_json::Value,
}

// Dispatcher con estado concurrente por canal
pub struct NotificationDispatcher {
    // Estado por alerta y canal
    delivery_status: Arc<RwLock<HashMap<(Uuid, NotificationChannel), ChannelDeliveryStatus>>>,
    // Configurables de backoff
    max_retries: u32,
    base_backoff_ms: u64,
}

impl NotificationDispatcher {
    pub fn new(max_retries: u32, base_backoff_ms: u64) -> Self {
        Self {
            delivery_status: Arc::new(RwLock::new(HashMap::new())),
            max_retries,
            base_backoff_ms,
        }
    }

    /// Ejecuta el despacho concurrente a múltiples canales con reintentos y fallback
    pub async fn dispatch(&self, alert: AlertEvent, channels: Vec<NotificationChannel>) {
        let mut handles = Vec::new();

        for channel in channels {
            let alert_clone = alert.clone();
            let dispatcher_clone = self.clone();
            handles.push(tokio::spawn(async move {
                dispatcher_clone
                    .dispatch_to_channel(alert_clone, channel)
                    .await;
            }));
        }

        // Espera que todos terminen
        for handle in handles {
            let _ = handle.await;
        }
    }

    /// Despacho con reintentos y backoff exponencial a un canal específico
    async fn dispatch_to_channel(&self, alert: AlertEvent, channel: NotificationChannel) {
        let key = (alert.id, channel.clone());
        let mut retries = 0;

        loop {
            let start = std::time::Instant::now();

            // Simula envío real, aquí reemplazar por integración real
            let result = Self::simulate_send(&alert, &channel).await;

            let latency_ms = start.elapsed().as_millis();

            // Actualizar estado
            {
                let mut status_map = self.delivery_status.write().await;
                let status = status_map
                    .entry(key.clone())
                    .or_insert_with(|| ChannelDeliveryStatus::new(channel.clone()));

                match &result {
                    Ok(_) => {
                        status.record_attempt(DeliveryState::Delivered, Some(latency_ms));
                        println!(
                            "Alerta {} entregada en canal {} en {} ms",
                            alert.id,
                            channel.to_string(),
                            latency_ms
                        );
                        break;
                    }
                    Err(err_msg) => {
                        if retries >= self.max_retries {
                            status.record_attempt(
                                DeliveryState::Failed(err_msg.clone()),
                                Some(latency_ms),
                            );
                            println!(
                                "Alerta {} falló en canal {} tras {} intentos: {}",
                                alert.id,
                                channel.to_string(),
                                retries,
                                err_msg
                            );
                            break;
                        } else {
                            retries += 1;
                            status.record_attempt(DeliveryState::Retrying(retries), Some(latency_ms));
                            println!(
                                "Intento {} para alerta {} en canal {} falló: {}. Reintentando...",
                                retries,
                                alert.id,
                                channel.to_string(),
                                err_msg
                            );
                            // Backoff exponencial con jitter
                            let backoff = self.base_backoff_ms * 2u64.pow(retries - 1);
                            let jitter: u64 = rand::thread_rng().gen_range(0..100);
                            sleep(Duration::from_millis(backoff + jitter)).await;
                        }
                    }
                }
            }
        }
    }

    /// Simulación de envío: éxito o fallo aleatorio para demo
    async fn simulate_send(
        _alert: &AlertEvent,
        channel: &NotificationChannel,
    ) -> Result<(), String> {
        // Simula latencia y fallo aleatorio
        let simulated_latency = rand::thread_rng().gen_range(50..200);
        sleep(Duration::from_millis(simulated_latency)).await;

        // Falla 20% de las veces para demo
        if rand::thread_rng().gen_bool(0.8) {
            Ok(())
        } else {
            Err(format!("Error simulado en canal {}", channel.to_string()))
        }
    }

    /// Consulta el estado de entrega de un alerta y canal
    pub async fn get_delivery_status(
        &self,
        alert_id: Uuid,
        channel: NotificationChannel,
    ) -> Option<ChannelDeliveryStatus> {
        let map = self.delivery_status.read().await;
        map.get(&(alert_id, channel)).cloned()
    }
}

impl Clone for NotificationDispatcher {
    fn clone(&self) -> Self {
        NotificationDispatcher {
            delivery_status: Arc::clone(&self.delivery_status),
            max_retries: self.max_retries,
            base_backoff_ms: self.base_backoff_ms,
        }
    }
}

// Ejemplo de uso
#[tokio::main]
async fn main() {
    let dispatcher = NotificationDispatcher::new(3, 500);

    let alert = AlertEvent {
        id: Uuid::new_v4(),
        tenant_id: "tenant_xyz".to_string(),
        payload: serde_json::json!({
            "message": "Critical alert triggered",
            "details": "Disk space low"
        }),
    };

    let channels = vec![
        NotificationChannel::Email,
        NotificationChannel::Slack,
        NotificationChannel::Sms,
    ];

    dispatcher.dispatch(alert, channels).await;
}
