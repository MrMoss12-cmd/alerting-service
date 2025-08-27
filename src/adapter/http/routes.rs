use axum::{
    routing::{get, post},
    Router,
    middleware::from_fn,
};

use crate::adapter::http::{controller::HttpController, middleware::auth_middleware};
use std::sync::Arc;

/// Construye el router HTTP con rutas, middlewares y controladores
pub fn build_router(controller: Arc<HttpController>) -> Router {
    // Middlewares globales (ejemplo, autenticación)
    let auth_layer = from_fn(auth_middleware);

    Router::new()
        // Ruta para recibir alertas (POST /alerts)
        .route("/alerts", post({
            let ctrl = controller.clone();
            move |payload| ctrl.receive_alert(payload)
        }))
        // Ruta para listar alertas (GET /alerts)
        .route("/alerts", get({
            let ctrl = controller.clone();
            move |query| ctrl.list_alerts(query)
        }))
        // Agregar middleware de autenticación a las rutas
        .layer(auth_layer)
        // Aquí podrían agregarse rutas agrupadas por versión o área
}
