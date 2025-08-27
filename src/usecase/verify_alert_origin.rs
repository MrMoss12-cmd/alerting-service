// src/usecase/verify_alert_origin.rs
//! Verificación del origen de una alerta (anti-spoofing).
//!
//! Este módulo ofrece un componente `OriginVerifier` que:
//! - valida la autenticidad del mensaje (HMAC-SHA256 o JWT token stub),
//! - aplica listas blancas/negras de orígenes,
//! - registra verificaciones en un `AuditSink` inyectable,
//! - usa `tokio::task::spawn_blocking` para las operaciones criptográficas costosas,
//! - mantiene una caché ligera de resultados para reducir latencia en re-verificaciones.
//!
//! Nota: para propósitos de demostración se usan stubs (p. ej. verificación JWT simple).
//! En producción sustituir por validadores JWT/RSA reales y conectar IAM.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::RwLock;
use tracing::{debug, error, info};
use uuid::Uuid;

/// Dependencias externas que la integración real debe proveer:
/// - Un audit sink para registrar verificaciones (trait AuditSink declarado abajo).
/// - Un IAM client para validar identidad/autorizaciones (se simula aquí).

/// Resultado de la verificación
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationResult {
    pub alert_id: Option<Uuid>,
    pub origin: String,
    pub verified: bool,
    pub method: VerificationMethod,
    pub reason: Option<String>,
    pub checked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VerificationMethod {
    Hmac,
    Jwt,
    Unknown,
}

/// Errores de verificación
#[derive(thiserror::Error, Debug)]
pub enum VerifyError {
    #[error("Origin blacklisted")]
    Blacklisted,
    #[error("Missing authentication material")]
    MissingAuth,
    #[error("Auth invalid: {0}")]
    InvalidAuth(String),
    #[error("IAM validation failed: {0}")]
    IAMError(String),
    #[error("Internal error: {0}")]
    Internal(String),
}

/// Interfaz de auditoría (inyectable)
#[async_trait::async_trait]
pub trait AuditSink: Send + Sync + 'static {
    async fn record_verify(&self, result: &VerificationResult);
}

/// Stub simple de AuditSink que escribe en tracing
pub struct TracingAuditSink;

#[async_trait::async_trait]
impl AuditSink for TracingAuditSink {
    async fn record_verify(&self, result: &VerificationResult) {
        info!(alert = ?result.alert_id, origin = %result.origin, verified = result.verified, method = ?result.method, reason = ?result.reason, "verify_record");
    }
}

/// Estructura que mantiene listas y claves, y ofrece verificación.
pub struct OriginVerifier {
    /// Lista blanca: sólo orígenes permitidos si no está vacía.
    whitelist: Arc<RwLock<HashSet<String>>>,
    /// Lista negra: orígenes explícitamente bloqueados.
    blacklist: Arc<RwLock<HashSet<String>>>,
    /// Map origin -> secret (para HMAC verification). En producción esto
    /// vendrá de un vault/secret store.
    hmac_secrets: Arc<RwLock<HashMap<String, String>>>,
    /// Caché simple de verificaciones para reducir latencia (ttl).
    cache: Arc<RwLock<HashMap<String, (VerificationResult, DateTime<Utc>)>>>,
    /// TTL de caché
    cache_ttl: Duration,
    /// Audit sink
    audit: Arc<dyn AuditSink>,
    /// IAM client stub toggle (in real world replace with client)
    iam_enabled: bool,
}

impl OriginVerifier {
    /// Crea un `OriginVerifier` con TTL de caché y audit sink inyectado.
    pub fn new(audit: Arc<dyn AuditSink>, cache_ttl_seconds: i64, iam_enabled: bool) -> Self {
        Self {
            whitelist: Arc::new(RwLock::new(HashSet::new())),
            blacklist: Arc::new(RwLock::new(HashSet::new())),
            hmac_secrets: Arc::new(RwLock::new(HashMap::new())),
            cache: Arc::new(RwLock::new(HashMap::new())),
            cache_ttl: Duration::seconds(cache_ttl_seconds),
            audit,
            iam_enabled,
        }
    }

    /// Añade origen a whitelist
    pub async fn add_to_whitelist(&self, origin: impl Into<String>) {
        let mut w = self.whitelist.write().await;
        w.insert(origin.into());
    }

    /// Añade origen a blacklist
    pub async fn add_to_blacklist(&self, origin: impl Into<String>) {
        let mut b = self.blacklist.write().await;
        b.insert(origin.into());
    }

    /// Asocia un secreto HMAC a un origen
    pub async fn set_hmac_secret(&self, origin: impl Into<String>, secret: impl Into<String>) {
        let mut m = self.hmac_secrets.write().await;
        m.insert(origin.into(), secret.into());
    }

    /// Verifica el origen usando el siguiente orden:
    /// 1. blacklist check -> fail fast
    /// 2. whitelist check (if whitelist non-empty) -> only allow if present
    /// 3. cache lookup -> return cached if fresh
    /// 4. attempt HMAC verification (spawn_blocking)
    /// 5. attempt JWT/IAM verification (async)
    /// 6. if none -> fail
    ///
    /// `origin` es el identificador del emisor (hostname, service-name, etc.)
    /// `payload` es la representación canónica del mensaje (JSON string usually)
    /// `hmac_signature_b64` es la firma HMAC-SHA256 en base64, si disponible
    /// `jwt` es token JWT si disponible
    pub async fn verify_origin(
        &self,
        origin: &str,
        alert_id: Option<Uuid>,
        payload_canonical: &str,
        hmac_signature_b64: Option<&str>,
        jwt: Option<&str>,
    ) -> Result<VerificationResult, VerifyError> {
        let now = Utc::now();

        // 1) blacklist
        {
            let b = self.blacklist.read().await;
            if b.contains(origin) {
                let r = VerificationResult {
                    alert_id,
                    origin: origin.to_string(),
                    verified: false,
                    method: VerificationMethod::Unknown,
                    reason: Some("origin_blacklisted".to_string()),
                    checked_at: now,
                };
                // audit and return
                self.audit.record_verify(&r).await;
                return Err(VerifyError::Blacklisted);
            }
        }

        // 2) whitelist (if non-empty)
        {
            let w = self.whitelist.read().await;
            if !w.is_empty() && !w.contains(origin) {
                let r = VerificationResult {
                    alert_id,
                    origin: origin.to_string(),
                    verified: false,
                    method: VerificationMethod::Unknown,
                    reason: Some("origin_not_in_whitelist".to_string()),
                    checked_at: now,
                };
                self.audit.record_verify(&r).await;
                return Err(VerifyError::InvalidAuth("Origin not whitelisted".into()));
            }
        }

        // 3) cache lookup
        {
            let mut cache = self.cache.write().await;
            if let Some((cached, ts)) = cache.get(origin) {
                if *ts + self.cache_ttl > now {
                    debug!(origin = %origin, "cache hit for origin verification");
                    // return cached result (but update alert_id & checked_at)
                    let mut out = cached.clone();
                    out.checked_at = now;
                    self.audit.record_verify(&out).await;
                    return Ok(out);
                } else {
                    // stale -> remove
                    cache.remove(origin);
                }
            }
        }

        // 4) Try HMAC verification if secret exists and signature provided
        if let Some(sig_b64) = hmac_signature_b64 {
            let secrets = self.hmac_secrets.read().await;
            if let Some(secret) = secrets.get(origin) {
                // compute HMAC in blocking task to avoid stalling reactor
                let payload = payload_canonical.to_string();
                let secret_clone = secret.clone();
                let sig_b64 = sig_b64.to_string();
                let verified = tokio::task::spawn_blocking(move || {
                    verify_hmac_sha256_base64(&payload, &secret_clone, &sig_b64)
                })
                .await
                .map_err(|e| VerifyError::Internal(format!("spawn error: {}", e)))??;

                let res = VerificationResult {
                    alert_id,
                    origin: origin.to_string(),
                    verified,
                    method: VerificationMethod::Hmac,
                    reason: if verified { None } else { Some("hmac_mismatch".into()) },
                    checked_at: now,
                };

                // cache positive results only (to reduce exposure)
                if verified {
                    let mut cache = self.cache.write().await;
                    cache.insert(origin.to_string(), (res.clone(), now));
                }

                self.audit.record_verify(&res).await;

                if verified {
                    return Ok(res);
                } else {
                    return Err(VerifyError::InvalidAuth("HMAC signature invalid".into()));
                }
            }
        }

        // 5) Try JWT / IAM validation if enabled and token provided
        if self.iam_enabled {
            if let Some(token) = jwt {
                // Async IAM validation (simulated)
                match self.verify_jwt_via_iam(token).await {
                    Ok(valid) => {
                        let res = VerificationResult {
                            alert_id,
                            origin: origin.to_string(),
                            verified: valid,
                            method: VerificationMethod::Jwt,
                            reason: if valid { None } else { Some("jwt_invalid".into()) },
                            checked_at: now,
                        };

                        if valid {
                            let mut cache = self.cache.write().await;
                            cache.insert(origin.to_string(), (res.clone(), now));
                            self.audit.record_verify(&res).await;
                            return Ok(res);
                        } else {
                            self.audit.record_verify(&res).await;
                            return Err(VerifyError::InvalidAuth("JWT invalid".into()));
                        }
                    }
                    Err(e) => {
                        // IAM validation failed unexpectedly
                        let err = VerifyError::IAMError(format!("{:?}", e));
                        error!(?err, "IAM validation failed");
                        return Err(err);
                    }
                }
            }
        }

        // 6) Fallback: no auth materials or unable to verify
        let r = VerificationResult {
            alert_id,
            origin: origin.to_string(),
            verified: false,
            method: VerificationMethod::Unknown,
            reason: Some("no_auth_provided_or_no_verifier".into()),
            checked_at: now,
        };
        self.audit.record_verify(&r).await;
        Err(VerifyError::MissingAuth)
    }

    /// Simple wrapper to clear cache (for tests or manual invalidation).
    pub async fn clear_cache(&self) {
        let mut c = self.cache.write().await;
        c.clear();
    }

    /// Simula verificación JWT vía IAM (en producción llamar al servicio real).
    async fn verify_jwt_via_iam(&self, token: &str) -> Result<bool, String> {
        // Simulación ligera: token containing "valid" => ok; "slow" -> delay to simulate remote call
        if token.contains("slow") {
            // demonstrate spawn_blocking not needed, but emulate network latency
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        if token.contains("valid") {
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// Verifica HMAC-SHA256 del payload con el secreto y compara con firma en base64.
/// Retorna Ok(true) si coincide. Utiliza `sha2` y `hmac`-style manual composition.
/// En producción usa crates auditadas como `hmac` y `subtle` para comparación constante.
fn verify_hmac_sha256_base64(payload: &str, secret: &str, signature_b64: &str) -> Result<bool, VerifyError> {
    use hmac::{Hmac, Mac};
    use base64::engine::general_purpose;
    use base64::Engine as _;
    type HmacSha256 = Hmac<Sha256>;

    // decode provided signature
    let provided = general_purpose::STANDARD.decode(signature_b64).map_err(|e| VerifyError::InvalidAuth(format!("bad base64: {}", e)))?;

    // compute HMAC
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|e| VerifyError::Internal(format!("hmac init error: {:?}", e)))?;
    mac.update(payload.as_bytes());
    let computed = mac.finalize().into_bytes();

    // constant-time comparison
    if computed.len() != provided.len() {
        return Ok(false);
    }
    let mut eq = 0u8;
    for (a, b) in computed.iter().zip(provided.iter()) {
        eq |= a ^ b;
    }
    Ok(eq == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose;
    use base64::Engine as _;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    #[tokio::test]
    async fn test_hmac_verify_success_and_cache() {
        // setup
        let audit = Arc::new(TracingAuditSink);
        let v = OriginVerifier::new(audit, 60, false);
        let origin = "service-A";
        let secret = "supersecret";
        v.set_hmac_secret(origin, secret).await;

        // compute signature
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        let payload = r#"{"x":1}"#;
        mac.update(payload.as_bytes());
        let sig = mac.finalize().into_bytes();
        let sig_b64 = general_purpose::STANDARD.encode(sig);

        // first verification should compute and cache
        let res = v.verify_origin(origin, None, payload, Some(&sig_b64), None).await;
        assert!(res.is_ok());
        let out = res.unwrap();
        assert!(out.verified);
        assert_eq!(out.method, VerificationMethod::Hmac);

        // second verification should hit cache (we assert Ok)
        let res2 = v.verify_origin(origin, None, payload, Some(&sig_b64), None).await;
        assert!(res2.is_ok());
        let out2 = res2.unwrap();
        assert!(out2.verified);
    }

    #[tokio::test]
    async fn test_whitelist_blacklist_behavior() {
        let audit = Arc::new(TracingAuditSink);
        let v = OriginVerifier::new(audit, 60, false);

        v.add_to_blacklist("bad-service").await;
        let r = v.verify_origin("bad-service", None, "{}", None, None).await;
        assert!(matches!(r.unwrap_err(), VerifyError::Blacklisted));

        v.add_to_whitelist("only-one").await;
        // origin not in whitelist should fail
        let r2 = v.verify_origin("not-in-list", None, "{}", None, None).await;
        assert!(matches!(r2.unwrap_err(), VerifyError::InvalidAuth(_)));

        // origin in whitelist but no auth -> MissingAuth
        let r3 = v.verify_origin("only-one", None, "{}", None, None).await;
        assert!(matches!(r3.unwrap_err(), VerifyError::MissingAuth));
    }

    #[tokio::test]
    async fn test_jwt_iam_validation() {
        let audit = Arc::new(TracingAuditSink);
        let v = OriginVerifier::new(audit, 60, true); // iam enabled

        // valid token
        let r = v.verify_origin("svc", None, "{}", None, Some("token-valid")).await;
        assert!(r.is_ok());
        assert!(r.unwrap().verified);

        // invalid token
        let r2 = v.verify_origin("svc", None, "{}", None, Some("token-bad")).await;
        assert!(matches!(r2.unwrap_err(), VerifyError::InvalidAuth(_)));
    }
}
