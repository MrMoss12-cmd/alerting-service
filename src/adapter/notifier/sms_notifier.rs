// adapter/notifier/sms_notifier.rs

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{sleep, Duration, Instant};
use tracing::{error, info, warn};
use serde::Serialize;
use reqwest::Client;

#[derive(Debug, Clone)]
pub struct SmsConfig {
    pub api_token: String,
    pub tenant_id: String,
    pub sender_id: String,  // Remitente configurado en proveedor
    pub max_per_minute: u32, // Control cuota (mensajes/minuto)
    pub api_url: String,     // URL base proveedor SMS
}

#[derive(Debug, Clone)]
pub struct SmsMessage {
    pub tenant_id: String,
    pub to_number: String,
    pub body: String,
}

pub struct SmsNotifier {
    sender: mpsc::Sender<SmsMessage>,
    configs: HashMap<String, SmsConfig>,
    client: Client,
    // Rate limit por tenant: contador de mensajes y timestamp del periodo
    rate_limits: Arc<Mutex<HashMap<String, (u32, Instant)>>>,
}

impl SmsNotifier {
    pub fn new(configs: HashMap<String, SmsConfig>) -> Self {
        let (sender, receiver) = mpsc::channel(100);
        let notifier = SmsNotifier {
            sender,
            configs,
            client: Client::new(),
            rate_limits: Arc::new(Mutex::new(HashMap::new())),
        };
        notifier.spawn_worker(receiver);
        notifier
    }

    pub async fn send(&self, msg: SmsMessage) -> Result<(), String> {
        self.sender.send(msg).await.map_err(|e| {
            error!("SmsNotifier queue send error: {}", e);
            "SmsNotifier queue closed or full".to_string()
        })
    }

    fn spawn_worker(&self, mut receiver: mpsc::Receiver<SmsMessage>) {
        let configs = self.configs.clone();
        let client = self.client.clone();
        let rate_limits = self.rate_limits.clone();

        tokio::spawn(async move {
            while let Some(msg) = receiver.recv().await {
                let config = match configs.get(&msg.tenant_id) {
                    Some(c) => c.clone(),
                    None => {
                        warn!("No SMS config found for tenant {}", msg.tenant_id);
                        continue;
                    }
                };

                // Control simple de cuota: max_per_minute mensajes/minuto
                {
                    let mut limits = rate_limits.lock().await;
                    let now = Instant::now();

                    let (count, last_reset) = limits.entry(msg.tenant_id.clone())
                        .or_insert((0, now));

                    // Resetear contador si pasó más de un minuto
                    if now.duration_since(*last_reset) > Duration::from_secs(60) {
                        *count = 0;
                        *last_reset = now;
                    }

                    if *count >= config.max_per_minute {
                        warn!("Rate limit exceeded for tenant {}", msg.tenant_id);
                        // Esperar un poco antes de procesar siguiente mensaje
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }

                    *count += 1;
                }

                // Preparar payload para proveedor (simulación)
                #[derive(Serialize)]
                struct SmsRequest<'a> {
                    to: &'a str,
                    from: &'a str,
                    body: &'a str,
                    // token no se expone aquí, va en header auth
                }

                let sms_payload = SmsRequest {
                    to: &msg.to_number,
                    from: &config.sender_id,
                    body: &msg.body,
                };

                let mut attempts = 0;
                let max_attempts = 3;

                loop {
                    attempts += 1;
                    let res = client
                        .post(&config.api_url)
                        .bearer_auth(&config.api_token)
                        .json(&sms_payload)
                        .send()
                        .await;

                    match res {
                        Ok(resp) => {
                            if resp.status().is_success() {
                                info!("SMS sent to {} for tenant {}", msg.to_number, msg.tenant_id);
                                break;
                            } else {
                                let status = resp.status();
                                let body = resp.text().await.unwrap_or_default();
                                error!("SMS API error status: {}, body: {}", status, body);
                                if attempts >= max_attempts {
                                    error!("Max retry attempts reached for SMS to {}", msg.to_number);
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            error!("SMS send error: {}", e);
                            if attempts >= max_attempts {
                                error!("Max retry attempts reached for SMS to {}", msg.to_number);
                                break;
                            }
                        }
                    }

                    // Backoff exponencial
                    let backoff = Duration::from_secs(2u64.pow(attempts));
                    sleep(backoff).await;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;

    #[tokio::test]
    async fn test_sms_notifier_send() {
        let mut configs = HashMap::new();
        configs.insert("tenant1".into(), SmsConfig {
            api_token: "FAKE_API_TOKEN".into(),
            tenant_id: "tenant1".into(),
            sender_id: "TestSender".into(),
            max_per_minute: 5,
            api_url: "https://api.smsprovider.example/send".into(),
        });

        let notifier = SmsNotifier::new(configs);

        let msg = SmsMessage {
            tenant_id: "tenant1".into(),
            to_number: "+1234567890".into(),
            body: "Test SMS message".into(),
        };

        // Solo test que la cola acepte el mensaje (sin enviar real)
        let res = timeout(std::time::Duration::from_secs(1), notifier.send(msg)).await;
        assert!(res.is_ok());
    }
}
