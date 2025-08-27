// src/service/plugin_loader.rs
//! Plugin loader service
//!
//! Responsibilities:
//! - Dynamically load/unload plugins at runtime to extend capabilities without recompilation.
//! - Ensure modular architecture with well-defined plugin APIs.
//! - Validate plugin integrity and sandbox for security before activation.
//! - Support hot-reload: load, update, remove plugins without downtime.
//! - Manage version compatibility and support safe rollback.
//! - Audit all plugin lifecycle changes with user/context info.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::RwLock;
use tracing::{info, error};
use uuid::Uuid;
use anyhow::{Result, Context};

/// Trait every plugin must implement
#[async_trait::async_trait]
pub trait Plugin: Send + Sync {
    /// Unique plugin identifier
    fn id(&self) -> Uuid;

    /// Plugin version string
    fn version(&self) -> &str;

    /// Plugin name (human-readable)
    fn name(&self) -> &str;

    /// Initialize plugin (load resources, validate config)
    async fn initialize(&self) -> Result<()>;

    /// Shutdown plugin (release resources)
    async fn shutdown(&self) -> Result<()>;
}

/// Plugin metadata for auditing and version control
#[derive(Clone, Debug)]
pub struct PluginMetadata {
    pub id: Uuid,
    pub name: String,
    pub version: String,
    pub loaded_at: chrono::DateTime<chrono::Utc>,
    pub loaded_by: String, // user or service who triggered load/unload
}

/// Manages plugin lifecycle, storage and auditing
pub struct PluginLoader {
    plugins: Arc<RwLock<HashMap<Uuid, Arc<dyn Plugin>>>>,
    metadata: Arc<RwLock<HashMap<Uuid, PluginMetadata>>>,
}

impl PluginLoader {
    /// Create new empty plugin loader
    pub fn new() -> Self {
        Self {
            plugins: Arc::new(RwLock::new(HashMap::new())),
            metadata: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Load a plugin from file path with audit info
    pub async fn load_plugin<P: AsRef<Path>>(&self, path: P, loaded_by: &str) -> Result<()> {
        let path = path.as_ref();

        // Validate plugin file exists
        if !path.exists() {
            anyhow::bail!("Plugin file {:?} does not exist", path);
        }

        // Validate plugin integrity (e.g. hash check or signature)
        self.validate_plugin(path).await
            .context("Plugin validation failed")?;

        // Load plugin dynamic library or instantiate (simulate here)
        let plugin = self.instantiate_plugin(path).await
            .context("Plugin instantiation failed")?;

        // Initialize plugin (sandboxing, resource allocation)
        plugin.initialize().await.context("Plugin initialization failed")?;

        // Insert plugin and metadata
        {
            let mut plugins_guard = self.plugins.write().await;
            let mut meta_guard = self.metadata.write().await;

            plugins_guard.insert(plugin.id(), Arc::clone(&plugin));
            meta_guard.insert(plugin.id(), PluginMetadata {
                id: plugin.id(),
                name: plugin.name().to_string(),
                version: plugin.version().to_string(),
                loaded_at: chrono::Utc::now(),
                loaded_by: loaded_by.to_string(),
            });
        }

        info!("Loaded plugin '{}' version '{}' by {}", plugin.name(), plugin.version(), loaded_by);

        Ok(())
    }

    /// Unload a plugin by id with audit
    pub async fn unload_plugin(&self, plugin_id: Uuid, unloaded_by: &str) -> Result<()> {
        let plugin_opt = {
            let mut plugins_guard = self.plugins.write().await;
            plugins_guard.remove(&plugin_id)
        };

        if let Some(plugin) = plugin_opt {
            plugin.shutdown().await.context("Plugin shutdown failed")?;

            // Remove metadata and log
            {
                let mut meta_guard = self.metadata.write().await;
                meta_guard.remove(&plugin_id);
            }

            info!("Unloaded plugin '{}' by {}", plugin.name(), unloaded_by);

            Ok(())
        } else {
            anyhow::bail!("Plugin with id {} not found", plugin_id);
        }
    }

    /// List all loaded plugins with metadata
    pub async fn list_plugins(&self) -> Vec<PluginMetadata> {
        let meta_guard = self.metadata.read().await;
        meta_guard.values().cloned().collect()
    }

    /// Validate plugin integrity before loading
    async fn validate_plugin(&self, path: &Path) -> Result<()> {
        // Here: checksum, signature verification, sandboxing
        // Placeholder: always valid
        Ok(())
    }

    /// Instantiate plugin from file path (simulate dynamic loading)
    async fn instantiate_plugin(&self, path: &Path) -> Result<Arc<dyn Plugin>> {
        // Placeholder: simulate a dummy plugin instance
        Ok(Arc::new(DummyPlugin::new(path.to_string_lossy().to_string())))
    }
}

/// Dummy plugin implementation for simulation
struct DummyPlugin {
    id: Uuid,
    name: String,
    version: String,
}

impl DummyPlugin {
    fn new(name: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            name,
            version: "1.0.0".to_string(),
        }
    }
}

#[async_trait::async_trait]
impl Plugin for DummyPlugin {
    fn id(&self) -> Uuid { self.id }
    fn version(&self) -> &str { &self.version }
    fn name(&self) -> &str { &self.name }

    async fn initialize(&self) -> Result<()> {
        Ok(())
    }
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_load_unload_plugin() {
        let loader = PluginLoader::new();
        let dummy_path = PathBuf::from("dummy_plugin.so");

        // Load plugin (simulate success)
        let res = loader.load_plugin(&dummy_path, "tester").await;
        assert!(res.is_ok());

        // List plugins should contain one plugin
        let plugins = loader.list_plugins().await;
        assert_eq!(plugins.len(), 1);

        // Unload plugin by id
        let plugin_id = plugins[0].id;
        let res = loader.unload_plugin(plugin_id, "tester").await;
        assert!(res.is_ok());

        // List should be empty now
        let plugins = loader.list_plugins().await;
        assert!(plugins.is_empty());
    }
}
