use axum::{
    extract::{Path, Query, Json, Extension},
    http::StatusCode,
    response::{IntoResponse, Json as JsonResponse},
    Json as AxumJson,
};
use serde::{Deserialize, Serialize};
use tracing::{info, error};
use std::sync::Arc;

use crate::usecase::receive_and_validate_event::ReceiveAndValidateEventUseCase;

#[derive(Deserialize)]
pub struct AlertFilter {
    pub tenant_id: Option<String>,
    pub severity: Option<String>,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    pub page: Option<u32>,
    pub per_page: Option<u32>,
}

// Respuesta genérica con posible error
#[derive(Serialize)]
struct ApiResponse<T> {
    success: bool,
    message: Option<String>,
    data: Option<T>,
}

impl<T> ApiResponse<T> {
    fn success(data: T) -> Self {
        ApiResponse {
            success: true,
            message: None,
            data: Some(data),
        }
    }

    fn error(msg: &str) -> Self {
        ApiResponse {
            success: false,
            message: Some(msg.to_string()),
            data: None,
        }
    }
}

pub struct HttpController {
    // Dependencias inyectadas (ejemplo: casos de uso)
    receive_usecase: Arc<ReceiveAndValidateEventUseCase>,
}

impl HttpController {
    pub fn new(receive_usecase: Arc<ReceiveAndValidateEventUseCase>) -> Self {
        Self { receive_usecase }
    }

    /// Handler para recibir una alerta vía POST /alerts
    pub async fn receive_alert(
        &self,
        Json(payload): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        // Log contextual
        info!("Received alert payload: {:?}", payload);

        // Validar y procesar alerta (delegar a caso de uso)
        match self.receive_usecase.execute(payload).await {
            Ok(alert_id) => {
                info!("Alert processed with id {}", alert_id);
                (StatusCode::OK, AxumJson(ApiResponse::<String>::success(alert_id)))
            }
            Err(e) => {
                error!("Error processing alert: {:?}", e);
                (
                    StatusCode::BAD_REQUEST,
                    AxumJson(ApiResponse::<String>::error("Invalid alert data")),
                )
            }
        }
    }

    /// Handler para consultar alertas GET /alerts?tenant_id=...&severity=...
    pub async fn list_alerts(
        &self,
        Query(params): Query<AlertFilter>,
    ) -> impl IntoResponse {
        info!("Listing alerts with filters: {:?}", params);

        // Aquí delegar a un caso de uso que devuelva alertas paginadas y filtradas.
        // Simulación de respuesta vacía para ejemplo
        let empty_result: Vec<String> = vec![];

        (StatusCode::OK, AxumJson(ApiResponse::success(empty_result)))
    }
}
