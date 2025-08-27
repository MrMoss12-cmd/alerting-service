// src/config/tls_config.rs

use serde::{Deserialize};
use anyhow::{Result, bail};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    #[serde(default)]
    pub ca_cert_path: Option<PathBuf>,

    #[serde(default)]
    pub enable_mtls: bool,

    #[serde(default)]
    pub allowed_protocols: Option<Vec<String>>, // e.g. ["TLSv1.2", "TLSv1.3"]

    #[serde(default)]
    pub allowed_cipher_suites: Option<Vec<String>>, // e.g. ["TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"]

    #[serde(default)]
    pub tenant_overrides: HashMap<String, TenantTlsConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TenantTlsConfig {
    pub cert_path: Option<PathBuf>,
    pub key_path: Option<PathBuf>,
    #[serde(default)]
    pub ca_cert_path: Option<PathBuf>,
    #[serde(default)]
    pub enable_mtls: Option<bool>,
    #[serde(default)]
    pub allowed_protocols: Option<Vec<String>>,
    #[serde(default)]
    pub allowed_cipher_suites: Option<Vec<String>>,
}

impl TlsConfig {
    pub fn validate(&self) -> Result<()> {
        if !self.cert_path.exists() {
            bail!("TLS config error: cert_path does not exist: {:?}", self.cert_path);
        }
        if !self.key_path.exists() {
            bail!("TLS config error: key_path does not exist: {:?}", self.key_path);
        }
        if let Some(ca_path) = &self.ca_cert_path {
            if !ca_path.exists() {
                bail!("TLS config error: ca_cert_path does not exist: {:?}", ca_path);
            }
        }
        // Additional validations can be added here, like protocols and cipher suites allowed

        Ok(())
    }
}
