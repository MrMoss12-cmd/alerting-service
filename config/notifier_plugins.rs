// src/config/notifier_plugins.rs

use serde::{Deserialize};
use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime};

#[derive(Debug, Clone, Deserialize)]
pub struct NotifierPluginsConfig {
    pub plugins: HashMap<String, PluginMetadata>, // key is plugin name
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginMetadata {
    pub name: String,
    pub version: String,
    pub enabled: bool,
    pub tenant_id: Option<String>, // None means global

    #[serde(default)]
    pub config: HashMap<String, String>, // Plugin-specific config key-value

    #[serde(default)]
    pub integrity_hash: Option<String>, // e.g. SHA256 hash for plugin binary

    #[serde(default)]
    pub last_updated: Option<SystemTime>,

    #[serde(default)]
    pub dependencies: Vec<String>, // Other plugin names this plugin depends on
}

impl NotifierPluginsConfig {
    pub fn validate(&self) -> Result<()> {
        // Validate no conflicting versions enabled for same tenant
        let mut tenant_plugin_versions: HashMap<(Option<String>, String), HashSet<String>> = HashMap::new();
        for plugin in self.plugins.values() {
            let key = (plugin.tenant_id.clone(), plugin.name.clone());
            let versions = tenant_plugin_versions.entry(key).or_default();
            versions.insert(plugin.version.clone());
            if versions.len() > 1 {
                bail!("Multiple versions of plugin '{}' enabled for tenant {:?}: {:?}", plugin.name, plugin.tenant_id, versions);
            }
        }

        // Validate integrity hashes format (hex length 64 for sha256)
        for plugin in self.plugins.values() {
            if let Some(hash) = &plugin.integrity_hash {
                if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
                    bail!("Invalid integrity_hash format for plugin '{}'", plugin.name);
                }
            }
        }

        Ok(())
    }

    pub fn get_enabled_plugins_for_tenant(&self, tenant_id: &str) -> Vec<&PluginMetadata> {
        self.plugins.values()
            .filter(|p| p.enabled && (p.tenant_id.as_deref() == Some(tenant_id) || p.tenant_id.is_none()))
            .collect()
    }
}
