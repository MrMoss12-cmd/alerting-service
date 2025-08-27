// adapter/notifier/telegram_notifier.rs

use reqwest::Client;
use serde::Serialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tokio::task;
use tokio::time::sleep;
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub tenant_id: String,
    pub chat_id: String, // ID o username de chat/usuario/grupo
}

#[derive(Debug, Clone)]
pub struct TelegramMessage {
    pub tenant_id: String,
    pub chat_id: String,
    pub text: String,
    // Enriquecible con multimedia, markdown, etc.
}

pub struct TelegramNotifier {
    sender: mpsc::Sender<TelegramMessage>,
    configs: HashMap<String, TelegramConfig>,
    client: Client,
    // Rate limiting simple: últimos timestamp por tenant
    last_sent: Mutex<HashMap<String, Instant>>,
}

impl TelegramNotifier {
    pub fn new(configs: HashMap<String, TelegramConfig>) -> Self {
        let (sender, receiver) = mpsc::channel(100);
        let notifier = Self {
            sender,
            configs,
            client: Client::new(),
            last_sent: Mutex::new(HashMap::new()),
        };

        notifier.spawn_worker(receiver);

        notifier
    }

    pub async fn send(&self, message: TelegramMessage) -> Result<(), String> {
        self.sender.send(message).await.map_err(|e| {
            error!("TelegramNotifier queue error: {}", e);
            "Telegram notifier queue closed or full".to_string()
        })
    }

    fn spawn_worker(&self, mut receiver: mpsc::Receiver<TelegramMessage>) {
        let configs = self.configs.clone();
        let client = self.client.clone();
        let last_sent = self.last_sent.clone();

        task::spawn(async move {
            while let Some(msg) = receiver.recv().await {
                let config = match configs.get(&msg.tenant_id) {
                    Some(c) => c.clone(),
                    None => {
                        warn!("No Telegram config for tenant {}", msg.tenant_id);
                        continue;
                    }
                };

                // Rate limiting: mínimo 1 mensaje cada 1.5 segundos por tenant
                {
                    let mut last_sent_map = last_sent.lock().await;
                    if let Some(last_time) = last_sent_map.get(&msg.tenant_id) {
                        let elapsed = last_time.elapsed();
                        if elapsed < Duration::from_millis(1500) {
                            let wait = Duration::from_millis(1500) - elapsed;
                            sleep(wait).await;
                        }
                    }
                    last_sent_map.insert(msg.tenant_id.clone(), Instant::now());
                }

                // Construir URL API Telegram para sendMessage
                let url = format!(
                    "https://api.telegram.org/bot{}/sendMessage",
                    config.bot_token
                );

                #[derive(Serialize)]
                struct TelegramSendRequest<'a> {
                    chat_id: &'a str,
                    text: &'a str,
                    parse_mode: &'a str,
                    disable_web_page_preview: bool,
                }

                let req = TelegramSendRequest {
                    chat_id: &msg.chat_id,
                    text: &msg.text,
                    parse_mode: "MarkdownV2",
                    disable_web_page_preview: true,
                };

                let mut attempts = 0;
                let max_attempts = 3;

                loop {
                    attempts += 1;
                    let res = client.post(&url).json(&req).send().await;

                    match res {
                        Ok(resp) => {
                            if resp.status().is_success() {
                                info!("Telegram message sent to {} tenant {}", msg.chat_id, msg.tenant_id);
                                break;
                            } else {
                                let status = resp.status();
                                let body = resp.text().await.unwrap_or_default();
                                error!("Telegram API error status: {}, body: {}", status, body);
                                if attempts >= max_attempts {
                                    error!("Max retry attempts reached for Telegram message");
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            error!("Telegram send error: {}", e);
                            if attempts >= max_attempts {
                                error!("Max retry attempts reached for Telegram message");
                                break;
                            }
                        }
                    }

                    // Backoff exponencial simple antes de reintentar
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
    async fn test_telegram_notifier_queue() {
        let mut configs = HashMap::new();
        configs.insert("tenant1".into(), TelegramConfig {
            bot_token: "FAKE_BOT_TOKEN".into(),
            tenant_id: "tenant1".into(),
            chat_id: "@somechannel".into(),
        });

        let notifier = TelegramNotifier::new(configs);

        let msg = TelegramMessage {
            tenant_id: "tenant1".into(),
            chat_id: "@somechannel".into(),
            text: "Test message from notifier".into(),
        };

        // Enviar y no esperar respuesta real (token falso)
        let result = timeout(std::time::Duration::from_secs(1), notifier.send(msg)).await;
        assert!(result.is_ok());
    }
}
