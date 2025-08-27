// src/config/kafka_config.rs

use serde::{Deserialize};
use anyhow::{Result, bail};
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct KafkaConfig {
    pub brokers: Vec<String>,
    pub group_id: String,
    pub topics: Vec<String>,
    #[serde(default)]
    pub enable_auto_commit: bool,
    #[serde(default)]
    pub auto_commit_interval_ms: Option<u64>,
    #[serde(default)]
    pub session_timeout_ms: Option<u64>,
    #[serde(default)]
    pub retry_backoff_ms: Option<u64>,
    #[serde(default)]
    pub max_poll_records: Option<usize>,

    #[serde(default)]
    pub sasl: Option<SaslConfig>,
    #[serde(default)]
    pub tls: Option<TlsConfig>,

    #[serde(default)]
    pub tenant_overrides: HashMap<String, TenantKafkaConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SaslConfig {
    pub mechanism: String, // e.g. "PLAIN", "SCRAM-SHA-256"
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    pub ca_cert_path: Option<String>,
    pub client_cert_path: Option<String>,
    pub client_key_path: Option<String>,
    pub insecure_skip_verify: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TenantKafkaConfig {
    pub brokers: Option<Vec<String>>,
    pub group_id: Option<String>,
    pub topics: Option<Vec<String>>,
    #[serde(default)]
    pub enable_auto_commit: Option<bool>,
    #[serde(default)]
    pub sasl: Option<SaslConfig>,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

impl KafkaConfig {
    pub fn validate(&self) -> Result<()> {
        if self.brokers.is_empty() {
            bail!("KafkaConfig validation error: brokers list cannot be empty");
        }
        if self.group_id.trim().is_empty() {
            bail!("KafkaConfig validation error: group_id cannot be empty");
        }
        if self.topics.is_empty() {
            bail!("KafkaConfig validation error: topics list cannot be empty");
        }
        // Validate SASL config if present
        if let Some(sasl) = &self.sasl {
            let allowed_mechs = ["PLAIN", "SCRAM-SHA-256", "SCRAM-SHA-512"];
            if !allowed_mechs.contains(&sasl.mechanism.as_str()) {
                bail!("KafkaConfig validation error: unsupported SASL mechanism '{}'", sasl.mechanism);
            }
            if sasl.username.trim().is_empty() {
                bail!("KafkaConfig validation error: SASL username cannot be empty");
            }
            if sasl.password.trim().is_empty() {
                bail!("KafkaConfig validation error: SASL password cannot be empty");
            }
        }
        // TLS validation could be added here if needed

        Ok(())
    }
}
