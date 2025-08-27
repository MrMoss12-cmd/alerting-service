// adapter/notifier/notifier_registry.rs

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use async_trait::async_trait;

/// Interfaz común para cualquier notifier
#[async_trait]
pub trait Notifier: Send + Sync {
    /// Identificador único del canal (email, sms, telegram, webpush, etc)
    fn channel(&self) -> &'static str;

    /// Versión del notifier, para control de compatibilidad
    fn version(&self) -> &'static str;

    /// Enviar notificación (payload genérico)
    async fn notify(&self, tenant_id: &str, payload: &NotificationPayload) -> Result<(), String>;
}

/// Representación genérica de payload de notificación
#[derive(Debug, Clone)]
pub struct NotificationPayload {
    pub title: String,
    pub body: String,
    pub metadata: HashMap<String, String>, // campos opcionales por notifier
}

/// Configuración específica para un tenant sobre un notifier
#[derive(Debug, Clone)]
pub struct NotifierConfig {
    pub tenant_id: String,
    pub enabled: bool,
    pub config_version: u32,
    pub settings: HashMap<String, String>, // Configuración específica (ej. API keys)
}

/// Registro interno por canal y tenant
struct NotifierEntry {
    notifier: Arc<dyn Notifier>,
    configs: HashMap<String, NotifierConfig>, // tenant_id -> config
}

/// Registry para múltiples notifiers y tenants
pub struct NotifierRegistry {
    notifiers: RwLock<HashMap<&'static str, NotifierEntry>>,
}

impl NotifierRegistry {
    pub fn new() -> Self {
        NotifierRegistry {
            notifiers: RwLock::new(HashMap::new()),
        }
    }

    /// Registrar un nuevo notifier para un canal
    /// Reemplaza si la versión es más reciente o igual (version string lex orden)
    pub async fn register_notifier(&self, notifier: Arc<dyn Notifier>) -> Result<(), String> {
        let mut notifiers = self.notifiers.write().await;
        let channel = notifier.channel();

        if let Some(existing) = notifiers.get(channel) {
            if existing.notifier.version() > notifier.version() {
                let msg = format!(
                    "Notifier version {} older than existing {} for channel {}",
                    notifier.version(),
                    existing.notifier.version(),
                    channel
                );
                warn!("{}", msg);
                return Err(msg);
            }
        }

        info!("Registering notifier for channel {} version {}", channel, notifier.version());

        notifiers.insert(channel, NotifierEntry {
            notifier,
            configs: HashMap::new(),
        });

        Ok(())
    }

    /// Configurar notifier para un tenant
    pub async fn configure_tenant_notifier(
        &self,
        channel: &'static str,
        tenant_config: NotifierConfig,
    ) -> Result<(), String> {
        let mut notifiers = self.notifiers.write().await;

        if let Some(entry) = notifiers.get_mut(channel) {
            entry.configs.insert(tenant_config.tenant_id.clone(), tenant_config);
            info!("Configured notifier {} for tenant", channel);
            Ok(())
        } else {
            let msg = format!("Notifier channel {} not registered", channel);
            error!("{}", msg);
            Err(msg)
        }
    }

    /// Obtener notifier activo para un tenant y canal
    /// Retorna None si no existe o no está habilitado
    pub async fn get_notifier_for_tenant(
        &self,
        channel: &str,
        tenant_id: &str,
    ) -> Option<Arc<dyn Notifier>> {
        let notifiers = self.notifiers.read().await;

        notifiers.get(channel).and_then(|entry| {
            entry.configs.get(tenant_id).and_then(|conf| {
                if conf.enabled {
                    Some(entry.notifier.clone())
                } else {
                    None
                }
            })
        })
    }

    /// Enviar notificación con fallback automático entre canales preferidos
    /// channels_order: lista de canales en orden de preferencia
    pub async fn notify_with_fallback(
        &self,
        tenant_id: &str,
        payload: &NotificationPayload,
        channels_order: &[&str],
    ) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        for &channel in channels_order {
            if let Some(notifier) = self.get_notifier_for_tenant(channel, tenant_id).await {
                match notifier.notify(tenant_id, payload).await {
                    Ok(_) => {
                        info!("Notification sent via channel {} for tenant {}", channel, tenant_id);
                        return Ok(());
                    }
                    Err(e) => {
                        warn!("Notification failed on channel {}: {}", channel, e);
                        errors.push(format!("{}: {}", channel, e));
                    }
                }
            } else {
                warn!("No enabled notifier for channel {} tenant {}", channel, tenant_id);
            }
        }

        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio;

    struct DummyNotifier {
        channel_name: &'static str,
        version_str: &'static str,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl Notifier for DummyNotifier {
        fn channel(&self) -> &'static str {
            self.channel_name
        }

        fn version(&self) -> &'static str {
            self.version_str
        }

        async fn notify(&self, _tenant_id: &str, _payload: &NotificationPayload) -> Result<(), String> {
            if self.fail {
                Err("forced failure".to_string())
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn test_register_and_notify() {
        let registry = NotifierRegistry::new();

        let email_notifier = Arc::new(DummyNotifier {
            channel_name: "email",
            version_str: "1.0.0",
            fail: false,
        });

        registry.register_notifier(email_notifier.clone()).await.unwrap();

        let tenant_config = NotifierConfig {
            tenant_id: "tenant1".to_string(),
            enabled: true,
            config_version: 1,
            settings: HashMap::new(),
        };

        registry.configure_tenant_notifier("email", tenant_config).await.unwrap();

        let payload = NotificationPayload {
            title: "Test".into(),
            body: "Body".into(),
            metadata: HashMap::new(),
        };

        let res = registry.notify_with_fallback("tenant1", &payload, &["email"]).await;
        assert!(res.is_ok());
    }
}
