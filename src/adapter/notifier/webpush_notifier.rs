// adapter/notifier/webpush_notifier.rs

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};
use web_push::{ContentEncoding, SubscriptionInfo, WebPushClient, WebPushMessageBuilder, VapidSignatureBuilder};
use serde::{Deserialize, Serialize};
use url::Url;
use futures::stream::{FuturesUnordered, StreamExt};
use std::time::{Duration, SystemTime};

/// Representa la suscripción web push de un cliente
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebPushSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub tenant_id: String,
    pub created_at: SystemTime,
}

/// Configuración VAPID por tenant
#[derive(Clone, Debug)]
pub struct VapidConfig {
    pub subject: String,            // mailto: o URL para contacto
    pub public_key: String,
    pub private_key: String,
}

/// Notificación web push
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebPushNotification {
    pub tenant_id: String,
    pub title: String,
    pub body: String,
    pub icon: Option<String>,
    pub url: Option<String>,
    pub ttl: u32, // seconds
    pub data: Option<serde_json::Value>, // Payload custom
}

/// Dispatcher para notificaciones webpush multi-tenant
pub struct WebPushNotifier {
    client: WebPushClient,
    vapid_configs: HashMap<String, VapidConfig>,
    subscriptions: Arc<Mutex<HashMap<String, Vec<WebPushSubscription>>>>, // tenant_id -> subscripciones
}

impl WebPushNotifier {
    pub fn new(
        vapid_configs: HashMap<String, VapidConfig>,
        subscriptions: Arc<Mutex<HashMap<String, Vec<WebPushSubscription>>>>,
    ) -> Self {
        WebPushNotifier {
            client: WebPushClient::new().expect("Failed to create WebPushClient"),
            vapid_configs,
            subscriptions,
        }
    }

    /// Registrar suscripción de usuario
    pub async fn register_subscription(&self, sub: WebPushSubscription) {
        let mut subs = self.subscriptions.lock().await;
        let tenant_subs = subs.entry(sub.tenant_id.clone()).or_insert_with(Vec::new);
        // Evitar duplicados por endpoint
        if !tenant_subs.iter().any(|s| s.endpoint == sub.endpoint) {
            tenant_subs.push(sub);
        }
    }

    /// Enviar notificación push a todos suscriptores de tenant (con control de tasa simplificado)
    pub async fn send_notification(&self, notification: WebPushNotification) {
        let subs_map = self.subscriptions.lock().await;
        let tenant_subs = match subs_map.get(&notification.tenant_id) {
            Some(s) => s.clone(),
            None => {
                warn!("No subscriptions for tenant {}", &notification.tenant_id);
                return;
            }
        };
        drop(subs_map); // liberar lock antes del envío async

        let vapid = match self.vapid_configs.get(&notification.tenant_id) {
            Some(v) => v,
            None => {
                warn!("No VAPID config for tenant {}", &notification.tenant_id);
                return;
            }
        };

        // Construir signature VAPID para cada mensaje
        let sig_builder = VapidSignatureBuilder::from_pem(
            vapid.private_key.as_bytes(),
            &vapid.subject,
        ).expect("Invalid VAPID private key");

        let mut futures = FuturesUnordered::new();

        for sub in tenant_subs.into_iter() {
            let client = self.client.clone();
            let sig_builder = sig_builder.clone();
            let notif = notification.clone();

            futures.push(tokio::spawn(async move {
                // Construir SubscriptionInfo
                let subscription_info = SubscriptionInfo {
                    endpoint: sub.endpoint.clone(),
                    keys: web_push::SubscriptionKeys {
                        p256dh: sub.p256dh.clone(),
                        auth: sub.auth.clone(),
                    },
                };

                let mut builder = WebPushMessageBuilder::new(&subscription_info).unwrap();

                // Set VAPID signature
                builder.set_vapid_signature(sig_builder.clone());

                builder.set_ttl(notif.ttl);

                // Payload JSON
                let payload = serde_json::json!({
                    "title": notif.title,
                    "body": notif.body,
                    "icon": notif.icon,
                    "url": notif.url,
                    "data": notif.data,
                });

                let payload_bytes = serde_json::to_vec(&payload).unwrap();

                builder.set_content_encoding(ContentEncoding::AesGcm);
                builder.set_payload(payload_bytes);

                match client.send(builder.build().unwrap()).await {
                    Ok(_) => {
                        info!("WebPush sent to endpoint {} tenant {}", sub.endpoint, notif.tenant_id);
                        Ok(())
                    }
                    Err(e) => {
                        error!("Failed to send WebPush to {}: {}", sub.endpoint, e);
                        Err(e)
                    }
                }
            }));
        }

        // Ejecutar envíos concurrentes con un límite (p. ej. 10)
        let concurrency_limit = 10;
        let mut in_progress = FuturesUnordered::new();

        for fut in futures {
            in_progress.push(fut);

            if in_progress.len() >= concurrency_limit {
                if let Some(res) = in_progress.next().await {
                    match res {
                        Ok(Ok(())) => (),
                        Ok(Err(e)) => warn!("Send error: {:?}", e),
                        Err(e) => warn!("Task join error: {:?}", e),
                    }
                }
            }
        }

        // Esperar restantes
        while let Some(res) = in_progress.next().await {
            match res {
                Ok(Ok(())) => (),
                Ok(Err(e)) => warn!("Send error: {:?}", e),
                Err(e) => warn!("Task join error: {:?}", e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    #[tokio::test]
    async fn test_register_and_send() {
        let tenant_id = "tenant_test".to_string();

        let vapid_cfg = VapidConfig {
            subject: "mailto:admin@example.com".into(),
            public_key: "FAKE_PUBLIC_KEY".into(),
            private_key: "-----BEGIN PRIVATE KEY-----\nFAKE_PRIVATE_KEY\n-----END PRIVATE KEY-----".into(),
        };

        let subscriptions = Arc::new(Mutex::new(HashMap::new()));

        let notifier = WebPushNotifier::new(
            vec![(tenant_id.clone(), vapid_cfg)].into_iter().collect(),
            subscriptions.clone(),
        );

        let subscription = WebPushSubscription {
            endpoint: "https://example.com/endpoint".into(),
            p256dh: "fake_p256dh_key".into(),
            auth: "fake_auth_key".into(),
            tenant_id: tenant_id.clone(),
            created_at: SystemTime::now(),
        };

        notifier.register_subscription(subscription).await;

        let notification = WebPushNotification {
            tenant_id,
            title: "Test Title".into(),
            body: "Test Body".into(),
            icon: None,
            url: Some("https://example.com".into()),
            ttl: 3600,
            data: None,
        };

        notifier.send_notification(notification).await;
    }
}
