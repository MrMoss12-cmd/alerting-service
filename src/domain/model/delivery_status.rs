// src/domain/model/delivery_status.rs
//! Modelo de estado de entrega y registro de intentos.
//!
//! Este módulo modela el estado de entrega por canal para una alerta,
//! guardando historial de intentos, soportando políticas de reintento
//! (backoff exponencial), circuit-breaker temporal y facilitando
//! agregaciones útiles para métricas y diagnóstico.
//!
//! Está diseñado para usarse de forma asíncrona en un servicio basado en Tokio:
//! por ejemplo, `wait_backoff_for_next_attempt` usa `tokio::time::sleep`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::time::Duration;

/// Estado actual de la entrega para un canal concreto.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryState {
    /// Esperando primer intento o en cola.
    Pending,
    /// Entregado satisfactoriamente.
    Delivered,
    /// Falló y no se reintentará (por política o por error irreversible).
    Failed,
    /// En proceso de reintentos.
    Retrying,
    /// Saltado por política (ej. severidad, horario, tenant config).
    Skipped,
}

/// Registro de un intento individual de entrega.
/// Guardar detalles mínimos útiles para diagnóstico y auditoría.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    /// Número de intento (1..N).
    pub attempt_number: u32,
    /// Timestamp del intento.
    pub timestamp: DateTime<Utc>,
    /// Duración aproximada en milisegundos si está disponible.
    pub duration_ms: Option<u64>,
    /// Indica si fue exitoso.
    pub success: bool,
    /// Mensaje de error (redactado) si falló.
    pub error: Option<String>,
    /// Respuesta del proveedor / canal (estructura libre).
    pub provider_response: Option<JsonValue>,
}

/// Métricas resumidas por canal (útiles para exposición a Prometheus u OpenTelemetry).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelMetrics {
    pub success_count: u64,
    pub failure_count: u64,
    /// Latencia promedio en ms (estimada incrementálmente).
    pub avg_latency_ms: Option<f64>,
}

impl ChannelMetrics {
    fn record_attempt(&mut self, duration_ms: Option<u64>, success: bool) {
        if success {
            self.success_count = self.success_count.saturating_add(1);
        } else {
            self.failure_count = self.failure_count.saturating_add(1);
        }

        if let Some(d) = duration_ms {
            self.avg_latency_ms = match self.avg_latency_ms {
                Some(prev) => {
                    let total = prev * ((self.success_count + self.failure_count - 1) as f64);
                    let new_avg = (total + d as f64) / ((self.success_count + self.failure_count) as f64);
                    Some(new_avg)
                }
                None => Some(d as f64),
            };
        }
    }
}

/// Estado completo de entrega para un canal concreto.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelDelivery {
    /// Estado actual (Pending / Delivered / Failed / Retrying / Skipped).
    pub state: DeliveryState,

    /// Historial de intentos (ordenados cronológicamente).
    pub attempts: Vec<AttemptRecord>,

    /// Último intento (si existe).
    pub last_attempt: Option<DateTime<Utc>>,

    /// Máximo número de reintentos permitidos (política).
    pub max_retries: u32,

    /// Backoff base en milisegundos (usado en cálculo exponencial).
    pub backoff_base_ms: u64,

    /// Límite de backoff en ms (tope).
    pub backoff_max_ms: u64,

    /// Si el circuito está abierto, hasta cuándo (timestamp UTC). Mientras esté en el futuro,
    /// no se deberán realizar intentos — actúa como un circuit-breaker temporal.
    pub circuit_open_until: Option<DateTime<Utc>>,

    /// Metadata específica del canal (provider id, endpoint, etc.).
    pub channel_metadata: BTreeMap<String, JsonValue>,

    /// Métricas resumidas para este canal.
    pub metrics: ChannelMetrics,
}

impl ChannelDelivery {
    /// Crea un ChannelDelivery con valores por defecto razonables.
    pub fn new() -> Self {
        ChannelDelivery {
            state: DeliveryState::Pending,
            attempts: Vec::new(),
            last_attempt: None,
            max_retries: 5,
            backoff_base_ms: 500,   // 0.5s
            backoff_max_ms: 60_000, // 60s tope
            circuit_open_until: None,
            channel_metadata: BTreeMap::new(),
            metrics: ChannelMetrics::default(),
        }
    }

    /// Indica si actualmente el circuito está abierto (no se deben intentar entregas).
    pub fn is_circuit_open(&self) -> bool {
        match self.circuit_open_until {
            Some(t) => t > Utc::now(),
            None => false,
        }
    }

    /// Registra un intento — actualiza historial, métricas y estado.
    /// `duration_ms` y `provider_response` son opcionales y útiles para diagnóstico.
    pub fn record_attempt(&mut self, success: bool, duration_ms: Option<u64>, error: Option<String>, provider_response: Option<JsonValue>) {
        let attempt_number = (self.attempts.len() as u32) + 1;
        let now = Utc::now();

        let rec = AttemptRecord {
            attempt_number,
            timestamp: now,
            duration_ms,
            success,
            error: error.clone().map(Self::redact_error),
            provider_response,
        };

        // push historial
        self.attempts.push(rec);
        self.last_attempt = Some(now);

        // actualizar métricas
        self.metrics.record_attempt(duration_ms, success);

        // actualizar estado
        if success {
            self.state = DeliveryState::Delivered;
        } else {
            // si quedó en max retries -> marcar failed; si no -> retrying
            if attempt_number >= self.max_retries {
                self.state = DeliveryState::Failed;
            } else {
                self.state = DeliveryState::Retrying;
            }
        }
    }

    /// Marca la entrega como `Delivered` explícitamente.
    pub fn mark_delivered(&mut self, duration_ms: Option<u64>, provider_response: Option<JsonValue>) {
        self.record_attempt(true, duration_ms, None, provider_response);
        self.state = DeliveryState::Delivered;
    }

    /// Marca la entrega como `Failed` sin reintentos.
    pub fn mark_failed_permanent(&mut self, error: Option<String>, provider_response: Option<JsonValue>) {
        // registra intento final como fallido
        self.record_attempt(false, None, error, provider_response);
        self.state = DeliveryState::Failed;
        // abrir circuito por defecto un pequeño periodo para evitar cascada
        self.open_circuit_for_secs(30);
    }

    /// Abre el circuito por N segundos (evita nuevos intentos hasta esa hora).
    pub fn open_circuit_for_secs(&mut self, secs: i64) {
        self.circuit_open_until = Some(Utc::now() + chrono::Duration::seconds(secs));
    }

    /// Cierra el circuito (permitir intentos).
    pub fn close_circuit(&mut self) {
        self.circuit_open_until = None;
    }

    /// Indica si se debería reintentar ahora según políticas básicas (reintentos restantes, circuit).
    pub fn should_retry_now(&self) -> bool {
        if self.is_circuit_open() {
            return false;
        }
        let attempts_done = self.attempts.len() as u32;
        attempts_done < self.max_retries && self.state != DeliveryState::Delivered && self.state != DeliveryState::Failed && self.state != DeliveryState::Skipped
    }

    /// Calcula el backoff en milisegundos para el próximo intento exponencial con tope.
    /// Fórmula: min(backoff_base_ms * 2^(attempt-1), backoff_max_ms)
    /// `attempt` es 1-based (1 = primer reintento after initial attempt)
    pub fn compute_backoff_ms(&self) -> u64 {
        let attempts_done = self.attempts.len() as u32;
        // próximo intento será attempts_done + 1
        let exp = attempts_done.saturating_add(1).saturating_sub(1); // 0-based exponent
        // Use u128 to avoid overflow for large exponents (though max_retries limits it)
        let mult = (self.backoff_base_ms as u128).saturating_shl(exp as u32); // base * 2^exp
        let ms = std::cmp::min(mult as u64, self.backoff_max_ms);
        // minimal floor
        std::cmp::max(ms, self.backoff_base_ms)
    }

    /// Espera asíncronamente el backoff calculado antes del próximo intento.
    /// Usa `tokio::time::sleep` para integrarse con Tokio.
    pub async fn wait_backoff_for_next_attempt(&self) {
        if self.is_circuit_open() {
            // si circuito abierto, esperar hasta que se cierre
            if let Some(until) = self.circuit_open_until {
                let dur = until.signed_duration_since(Utc::now()).to_std().unwrap_or_else(|_| Duration::from_secs(0));
                tokio::time::sleep(dur).await;
            }
            return;
        }
        let ms = self.compute_backoff_ms();
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    /// Resumen corto orientado a métricas/diagnóstico.
    pub fn metrics_summary(&self) -> ChannelMetricsSummary {
        ChannelMetricsSummary {
            state: self.state.clone(),
            attempts: self.attempts.len() as u32,
            success_count: self.metrics.success_count,
            failure_count: self.metrics.failure_count,
            avg_latency_ms: self.metrics.avg_latency_ms,
        }
    }

    /// Redacta errores muy largos o sensibles antes de almacenar.
    fn redact_error(err: &str) -> String {
        if err.len() > 256 {
            format!("{}...[truncated]", &err[..256])
        } else {
            err.to_string()
        }
    }
}

/// Estructura de salida simplificada útil para exposición en métricas o APIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMetricsSummary {
    pub state: DeliveryState,
    pub attempts: u32,
    pub success_count: u64,
    pub failure_count: u64,
    pub avg_latency_ms: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn test_backoff_and_attempts() {
        let mut cd = ChannelDelivery::new();
        cd.max_retries = 3;
        cd.backoff_base_ms = 10;
        cd.backoff_max_ms = 1000;

        assert!(cd.should_retry_now());

        // first failed attempt
        cd.record_attempt(false, Some(15), Some("network".into()), Some(json!({"code": 502})));
        assert_eq!(cd.attempts.len(), 1);
        assert_eq!(cd.state, DeliveryState::Retrying);

        // compute backoff for next attempt (attempts_done =1 -> exponent 1 -> base*2^1)
        let b1 = cd.compute_backoff_ms();
        assert!(b1 >= 10);

        // wait (should not panic)
        cd.wait_backoff_for_next_attempt().await;

        // second failed
        cd.record_attempt(false, Some(20), Some("timeout".into()), None);
        assert_eq!(cd.attempts.len(), 2);
        assert_eq!(cd.state, DeliveryState::Retrying);

        // third failed -> reaches max_retries -> Failed
        cd.record_attempt(false, None, Some("permanent".into()), None);
        assert_eq!(cd.attempts.len(), 3);
        assert_eq!(cd.state, DeliveryState::Failed);
        assert!(!cd.should_retry_now());
    }

    #[tokio::test]
    async fn test_mark_delivered() {
        let mut cd = ChannelDelivery::new();
        cd.mark_delivered(Some(5), Some(json!({"ok": true})));
        assert_eq!(cd.state, DeliveryState::Delivered);
        assert_eq!(cd.attempts.len(), 1);
        assert_eq!(cd.metrics.success_count, 1);
    }
}
