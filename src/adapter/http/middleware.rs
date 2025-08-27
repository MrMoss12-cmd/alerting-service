use axum::{
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    middleware::Next,
};
use jsonwebtoken::{decode, DecodingKey, Validation, Algorithm};
use serde::{Deserialize};
use tracing::warn;

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    exp: usize,
    tenant_id: String,
    roles: Vec<String>,
}

/// Middleware para verificar JWT en Authorization Bearer header y validar mTLS si aplica
pub async fn auth_middleware<B>(
    req: Request<B>,
    next: Next<B>,
) -> Result<Response, StatusCode> {
    // Verificar header Authorization
    let auth_header = req.headers().get("authorization").and_then(|h| h.to_str().ok());
    let token = match auth_header {
        Some(header) if header.starts_with("Bearer ") => Some(header.trim_start_matches("Bearer ").to_string()),
        _ => None,
    };

    if token.is_none() {
        warn!("No Authorization token found");
        return Err(StatusCode::UNAUTHORIZED);
    }
    let token = token.unwrap();

    // Verificar JWT (clave ejemplo, en producción usar config)
    let secret = b"your_jwt_secret";
    let validation = Validation::new(Algorithm::HS256);

    let token_data = decode::<Claims>(&token, &DecodingKey::from_secret(secret), &validation);
    match token_data {
        Ok(data) => {
            // Podrías almacenar data en extensions para que el controlador lo use
            // TODO: Validar scopes, tenant_id, roles para autorización granular
            Ok(next.run(req).await)
        }
        Err(e) => {
            warn!("JWT validation error: {:?}", e);
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

// Aquí podría implementarse también validación de certificados cliente para mTLS,
// con acceso a req.extensions() o una capa adicional de hyper / tokio-tls
