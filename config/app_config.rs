// src/config/app_config.rs

use serde::{Deserialize};
use tokio::sync::watch;
use tokio::sync::RwLock;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use config::{Config, ConfigError, Environment, File};
use anyhow::{Result, Context};
use tracing::info;

/// Estructura principal que representa la configuración de la aplicación.
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub environment: EnvironmentConfig,
    pub tenants: HashMap<String, TenantConfig>,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub service: ServiceConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnvironmentConfig {
    pub profile: String, // e.g. "development", "staging", "production"
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LoggingConfig {
    pub level: Option<String>,
    pub file: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TenantConfig {
    pub id: String,
    pub database_url: String,
    #[serde(default)]
    pub alert_retention_days: Option<u32>,
    #[serde(default)]
    pub notifier_configs: Option<HashMap<String, serde_json::Value>>, // extensible config by notifier
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServiceConfig {
    pub max_concurrency: Option<usize>,
    pub retry_backoff_seconds: Option<u64>,
}

/// Wrapper para manejar configuración compartida y recargable.
#[derive(Clone)]
pub struct SharedAppConfig {
    inner: Arc<RwLock<AppConfig>>,
    pub watcher: watch::Receiver<AppConfig>,
}

impl SharedAppConfig {
    /// Carga inicial de configuración, desde archivo, entorno, y opcionalmente consul/vault.
    pub async fn load_from_sources(
        config_paths: &[PathBuf],
    ) -> Result<Self> {
        let mut cfg = Config::builder();

        // Cargar archivos en orden (último puede sobrescribir)
        for path in config_paths {
            if path.exists() {
                cfg = cfg.add_source(File::from(path.clone()));
                info!("Loaded config file: {:?}", path);
            } else {
                info!("Config file not found, skipping: {:?}", path);
            }
        }

        // Cargar variables de entorno con prefijo APP_
        cfg = cfg.add_source(Environment::with_prefix("APP").separator("__"));

        // Aquí se podría agregar integración con consul, vault, etc.
        // Ejemplo: cfg = cfg.add_source(ConsulSource::new(...));

        let built = cfg.build().context("Failed to build configuration")?;
        let app_config: AppConfig = built.try_deserialize().context("Failed to deserialize configuration")?;

        Self::validate(&app_config)?;

        // Inicializar canal para hot-reload (watch)
        let (tx, rx) = watch::channel(app_config.clone());

        // En un entorno real, acá se puede lanzar un watcher de archivos o config server para reload dinámico.

        Ok(Self {
            inner: Arc::new(RwLock::new(app_config)),
            watcher: rx,
        })
    }

    /// Validar configuración con reglas personalizadas y valores por defecto.
    fn validate(cfg: &AppConfig) -> Result<()> {
        // Validar profile
        let valid_profiles = ["development", "staging", "production"];
        if !valid_profiles.contains(&cfg.environment.profile.as_str()) {
            anyhow::bail!("Invalid environment profile '{}', must be one of {:?}", cfg.environment.profile, valid_profiles);
        }

        // Validar tenants
        if cfg.tenants.is_empty() {
            anyhow::bail!("At least one tenant must be defined");
        }

        // Validar URLs (simplificado)
        for (id, tenant) in &cfg.tenants {
            if tenant.database_url.trim().is_empty() {
                anyhow::bail!("Tenant '{}' must have a non-empty database_url", id);
            }
        }

        // Aquí se pueden agregar validaciones adicionales por campo...

        Ok(())
    }

    /// Obtener la configuración actual (snapshot)
    pub async fn get_config(&self) -> AppConfig {
        self.inner.read().await.clone()
    }

    /// Actualizar configuración internamente (solo para hot-reload)
    pub async fn update_config(&self, new_cfg: AppConfig) -> Result<()> {
        Self::validate(&new_cfg)?;
        {
            let mut writable = self.inner.write().await;
            *writable = new_cfg.clone();
        }
        // Notificar a watchers
        self.watcher.borrow_and_update();
        Ok(())
    }

    /// Obtener configuración de un tenant específico
    pub async fn get_tenant_config(&self, tenant_id: &str) -> Option<TenantConfig> {
        let cfg = self.inner.read().await;
        cfg.tenants.get(tenant_id).cloned()
    }
}

// Opcional: Implementar algún watcher para recarga dinámica (ejemplo simple con notify crate)
// pub async fn watch_config_files(...) { ... }
