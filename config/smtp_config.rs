// src/config/smtp_config.rs

use serde::{Deserialize};
use anyhow::{Result, bail};
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default)]
    pub use_tls: bool,
    #[serde(default)]
    pub tls_verify_cert: bool,
    #[serde(default)]
    pub auth_mechanism: Option<String>, // e.g. "LOGIN", "PLAIN", "XOAUTH2"
    #[serde(default)]
    pub connection_timeout_secs: Option<u64>,
    #[serde(default)]
    pub read_timeout_secs: Option<u64>,
    #[serde(default)]
    pub write_timeout_secs: Option<u64>,

    #[serde(default)]
    pub tenant_overrides: HashMap<String, TenantSmtpConfig>,

    #[serde(default)]
    pub email_templates_path: Option<String>, // Path to templates folder
}

#[derive(Debug, Clone, Deserialize)]
pub struct TenantSmtpConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default)]
    pub use_tls: Option<bool>,
    #[serde(default)]
    pub tls_verify_cert: Option<bool>,
    #[serde(default)]
    pub auth_mechanism: Option<String>,
    #[serde(default)]
    pub connection_timeout_secs: Option<u64>,
}

impl SmtpConfig {
    pub fn validate(&self) -> Result<()> {
        if self.host.trim().is_empty() {
            bail!("SmtpConfig validation error: host cannot be empty");
        }
        if !(1..=65535).contains(&self.port) {
            bail!("SmtpConfig validation error: port must be between 1 and 65535");
        }
        if let Some(username) = &self.username {
            if username.trim().is_empty() {
                bail!("SmtpConfig validation error: username cannot be empty if provided");
            }
        }
        if self.use_tls && !self.tls_verify_cert {
            // Warn or error depending on policy
            // Here just a note: skipping cert verification may be unsafe
        }

        Ok(())
    }
}
