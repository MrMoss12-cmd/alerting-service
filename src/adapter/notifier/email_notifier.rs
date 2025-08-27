// adapter/notifier/email_notifier.rs

use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::Deserialize;
use tokio::sync::mpsc::{self, Sender, Receiver};
use tokio::task;
use tracing::{info, warn, error};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct EmailConfig {
    pub smtp_server: String,
    pub smtp_port: u16,
    pub api_token: String,     // Token API para autenticación
    pub from_address: String,  // Dirección remitente
    pub tenant_id: String,     // Para multi-tenant
}

#[derive(Debug, Clone)]
pub struct EmailNotification {
    pub tenant_id: String,
    pub to_address: String,
    pub subject: String,
    pub body_html: Option<String>,
    pub body_text: Option<String>,
}

pub struct EmailNotifier {
    sender: Sender<EmailNotification>,
    configs: HashMap<String, EmailConfig>, // tenant_id => config
}

impl EmailNotifier {
    pub fn new(configs: HashMap<String, EmailConfig>) -> Self {
        // Canal para cola de envíos (por ejemplo, buffer 100)
        let (sender, receiver) = mpsc::channel(100);
        let mut notifier = Self { sender, configs };

        // Lanzar el worker de envío en background
        notifier.spawn_worker(receiver);

        notifier
    }

    pub async fn send(&self, notification: EmailNotification) -> Result<(), String> {
        // Enviar a la cola async, sin bloquear el caller
        self.sender.send(notification).await.map_err(|e| {
            error!("EmailNotifier queue full or closed: {}", e);
            "Email queue full or closed".to_string()
        })
    }

    fn spawn_worker(&mut self, mut receiver: Receiver<EmailNotification>) {
        let configs = self.configs.clone();

        task::spawn(async move {
            while let Some(notification) = receiver.recv().await {
                let tenant_config = match configs.get(&notification.tenant_id) {
                    Some(cfg) => cfg.clone(),
                    None => {
                        warn!("No SMTP config for tenant {}", notification.tenant_id);
                        continue; // Saltar
                    }
                };

                // Construir mensaje con texto y html opcionales
                let mut email_builder = Message::builder()
                    .from(tenant_config.from_address.parse().unwrap_or_else(|_| {
                        // fallback para errores
                        "no-reply@example.com".parse().unwrap()
                    }))
                    .to(notification.to_address.parse().unwrap());

                email_builder = email_builder.subject(notification.subject.clone());

                let email = if let Some(html) = notification.body_html.clone() {
                    email_builder.multipart(
                        lettre::message::MultiPart::alternative_plain_html(
                            notification.body_text.clone().unwrap_or_default(),
                            html,
                        )
                    )
                } else if let Some(text) = notification.body_text.clone() {
                    email_builder.body(text)
                } else {
                    warn!("Empty email body for notification {:?}", notification);
                    continue;
                };

                let email = match email {
                    Ok(m) => m,
                    Err(e) => {
                        error!("Error building email: {}", e);
                        continue;
                    }
                };

                // Transport SMTP con token API (usamos token como password)
                let creds = Credentials::new("apikey".to_string(), tenant_config.api_token.clone());

                let mailer = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&tenant_config.smtp_server)
                    .and_then(|transport| Ok(transport.port(tenant_config.smtp_port).credentials(creds).build()));

                let mailer = match mailer {
                    Ok(m) => m,
                    Err(e) => {
                        error!("Failed to create SMTP transport: {}", e);
                        continue;
                    }
                };

                // Enviar email, con manejo básico de retry
                let send_result = mailer.send(email).await;

                match send_result {
                    Ok(_) => info!("Email sent to {} for tenant {}", notification.to_address, notification.tenant_id),
                    Err(e) => error!("Failed to send email to {}: {}", notification.to_address, e),
                }

                // En producción se puede agregar reintentos, backoff, fallback a otros canales, métricas, etc.

                // Para no saturar el SMTP server, un pequeño delay (ajustable)
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_email_notifier_queue() {
        let mut configs = HashMap::new();
        configs.insert("tenant1".into(), EmailConfig {
            smtp_server: "smtp.example.com".into(),
            smtp_port: 587,
            api_token: "fake-token".into(),
            from_address: "alerts@example.com".into(),
            tenant_id: "tenant1".into(),
        });

        let notifier = EmailNotifier::new(configs);

        let notification = EmailNotification {
            tenant_id: "tenant1".into(),
            to_address: "user@example.com".into(),
            subject: "Test Email".into(),
            body_html: Some("<h1>Hello</h1>".into()),
            body_text: Some("Hello".into()),
        };

        let res = notifier.send(notification).await;
        assert!(res.is_ok());
    }
}
