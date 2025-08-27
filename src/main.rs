// src/main.rs
//! Punto de entrada del microservicio `alerting-service`.
//!
//! Objetivos principales:
//! - Cargar configuración, tracing, métricas y autenticación.
//! - Arrancar servidores HTTP (Axum), gRPC (Tonic) y workers (Kafka, schedulers).
//! - Gestionar apagado ordenado (graceful shutdown) y liberación de recursos.
//! - Exponer salud del servicio y métricas para monitoreo.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use tokio::{select, signal, sync::broadcast, task::JoinSet};
use tracing::{error, info, warn};

/// ---- Imports internos (respetando la estructura propuesta) -----------------
mod config;
mod telemetry;

use config::app_config::AppConfig;

// HTTP (Axum)
#[cfg(feature = "http")]
mod adapter_http {
    pub use crate::adapter::http::routes::build_http_router;
    pub use crate::adapter::http::{controller, middleware, routes};
}
#[cfg(feature = "http")]
mod adapter;

// gRPC (Tonic)
#[cfg(feature = "grpc")]
mod adapter_grpc {
    pub use crate::adapter::grpc::grpc_server;
}
#[cfg(feature = "grpc")]
mod adapter;

// Kafka
#[cfg(feature = "kafka")]
mod adapter_kafka {
    pub use crate::adapter::kafka::{kafka_consumer, kafka_producer};
}
#[cfg(feature = "kafka")]
mod adapter;

// Schedulers
#[cfg(feature = "schedulers")]
mod scheduler;

// Repositorios / servicios (inyectar estados compartidos)
mod repository;
mod service;

// --------------------------------------------------------------------------------

/// Estado compartido de la app inyectado en HTTP/gRPC/Workers.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<AppConfig>,
    // Ejemplos de dependencias compartidas:
    // pub alert_store: Arc<repository::alert_store::AlertStore>,
    // pub rule_store: Arc<repository::rule_store::RuleStore>,
    // pub notifier_registry: Arc<adapter::notifier::notifier_registry::NotifierRegistry>,
}

/// Señales de shutdown compartidas entre tareas.
#[derive(Clone)]
struct Shutdown {
    tx: broadcast::Sender<()>,
}
impl Shutdown {
    fn new() -> Self {
        let (tx, _rx) = broadcast::channel(8);
        Self { tx }
    }
    fn subscribe(&self) -> broadcast::Receiver<()> {
        self.tx.subscribe()
    }
    fn trigger(&self) {
        let _ = self.tx.send(());
    }
}

/// Punto de entrada Tokio.
#[tokio::main]
async fn main() {
    // 1) Carga de configuración
    let cfg = match AppConfig::load() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("❌ No se pudo cargar la configuración: {e}");
            std::process::exit(1);
        }
    };

    // 2) Tracing + Métricas + OpenTelemetry
    if let Err(e) = telemetry::tracing::init_tracing(&cfg) {
        eprintln!("⚠️  Tracing parcial: {e}");
    }
    let _otel_guard = telemetry::opentelemetry_setup::init_otel(&cfg)
        .await
        .unwrap_or_else(|e| {
            warn!(error = %e, "OpenTelemetry no inicializado; continuando sin exportador.");
            telemetry::opentelemetry_setup::NoopOtelGuard::new()
        });
    telemetry::metrics::register_core_metrics();

    // 3) Construcción de estado compartido
    let state = AppState { cfg: cfg.clone() /*, alert_store, rule_store, notifier_registry */ };

    // 4) Disparadores de apagado
    let shutdown = Shutdown::new();

    // 5) Conjunto de tareas concurrentes
    let mut tasks = JoinSet::new();

    // 5.a) Servidor HTTP (REST)
    #[cfg(feature = "http")]
    {
        let http_addr: SocketAddr = cfg.http.bind.parse().expect("http.bind inválido");
        let app_state = state.clone();
        let mut http_shutdown_rx = shutdown.subscribe();

        tasks.spawn(async move {
            let app = adapter_http::build_http_router(app_state);
            info!(%http_addr, "🌐 HTTP server escuchando");
            let server = axum::Server::bind(&http_addr).serve(app.into_make_service());

            select! {
                res = server => {
                    if let Err(e) = res {
                        error!(error=?e, "HTTP server finalizó con error");
                        return Err(e);
                    }
                    Ok::<(), hyper::Error>(())
                }
                _ = http_shutdown_rx.recv() => {
                    info!("🔻 Recibida señal de shutdown para HTTP");
                    // Axum/hyper graceful shutdown requiere with_graceful_shutdown; recreamos para ejemplo sencillo.
                    Ok(())
                }
            }
        });
    }

    // 5.b) Servidor gRPC
    #[cfg(feature = "grpc")]
    {
        use tonic::transport::Server;
        use adapter_grpc::grpc_server::{make_grpc_service, HealthReporter};

        let grpc_addr: SocketAddr = cfg.grpc.bind.parse().expect("grpc.bind inválido");
        let app_state = state.clone();
        let mut grpc_shutdown_rx = shutdown.subscribe();

        tasks.spawn(async move {
            let (svc, health_reporter): (_, HealthReporter) = make_grpc_service(app_state);
            health_reporter.set_serving().await;

            info!(%grpc_addr, "🛰️  gRPC server escuchando");
            let server = Server::builder().add_service(svc).serve_with_shutdown(
                grpc_addr,
                async move {
                    let _ = grpc_shutdown_rx.recv().await;
                    info!("🔻 Recibida señal de shutdown para gRPC");
                },
            );

            if let Err(e) = server.await {
                error!(error=?e, "gRPC server finalizó con error");
            }
        });
    }

    // 5.c) Consumidor Kafka
    #[cfg(feature = "kafka")]
    {
        let app_state = state.clone();
        let mut kafka_shutdown_rx = shutdown.subscribe();

        tasks.spawn(async move {
            use adapter_kafka::kafka_consumer::KafkaConsumer;
            let consumer = KafkaConsumer::new(app_state.cfg.clone());
            info!("📥 Kafka consumer iniciando");
            let run = consumer.run(async move {
                let _ = kafka_shutdown_rx.recv().await;
                info!("🔻 Recibida señal de shutdown para Kafka consumer");
            });
            if let Err(e) = run.await {
                error!(error=?e, "Kafka consumer terminó con error");
            }
        });
    }

    // 5.d) Productor Kafka opcional (para eventos internos)
    #[cfg(feature = "kafka")]
    {
        let app_state = state.clone();
        tasks.spawn(async move {
            use adapter_kafka::kafka_producer::KafkaProducer;
            // Ejemplo de inicialización temprana para calentar conexiones
            match KafkaProducer::new(app_state.cfg.clone()).await {
                Ok(_producer) => {
                    info!("📤 Kafka producer inicializado");
                    // Mantener vivo si se desea un loop de flush/healthcheck, o compartir por AppState.
                }
                Err(e) => warn!(error=?e, "Kafka producer no disponible al inicio"),
            }
        });
    }

    // 5.e) Servidor de métricas (Prometheus) — independiente del HTTP público
    #[cfg(feature = "metrics")]
    {
        use axum::{routing::get, Router};
        use telemetry::metrics::metrics_handler;

        let metrics_addr: SocketAddr = cfg.metrics.bind.parse().expect("metrics.bind inválido");
        let mut metrics_shutdown_rx = shutdown.subscribe();

        tasks.spawn(async move {
            let router = Router::new().route("/metrics", get(metrics_handler));
            info!(%metrics_addr, "📈 Metrics server escuchando");
            let server = axum::Server::bind(&metrics_addr).serve(router.into_make_service());

            select! {
                res = server => {
                    if let Err(e) = res {
                        error!(error=?e, "Metrics server finalizó con error");
                        return Err(e);
                    }
                    Ok::<(), hyper::Error>(())
                }
                _ = metrics_shutdown_rx.recv() => {
                    info!("🔻 Recibida señal de shutdown para Metrics");
                    Ok(())
                }
            }
        });
    }

    // 5.f) Schedulers (retry/purge)
    #[cfg(feature = "schedulers")]
    {
        // Retry scheduler
        let app_state = state.clone();
        let mut retry_shutdown_rx = shutdown.subscribe();
        tasks.spawn(async move {
            info!("⏱️  Retry scheduler iniciado");
            if let Err(e) =
                scheduler::retry_scheduler::run(app_state.clone(), async move {
                    let _ = retry_shutdown_rx.recv().await;
                    info!("🔻 Shutdown para Retry scheduler");
                })
                .await
            {
                error!(error=?e, "Retry scheduler error");
            }
        });

        // Purge scheduler
        let app_state = state.clone();
        let mut purge_shutdown_rx = shutdown.subscribe();
        tasks.spawn(async move {
            info!("🧹 Purge scheduler iniciado");
            if let Err(e) =
                scheduler::purge_scheduler::run(app_state.clone(), async move {
                    let _ = purge_shutdown_rx.recv().await;
                    info!("🔻 Shutdown para Purge scheduler");
                })
                .await
            {
                error!(error=?e, "Purge scheduler error");
            }
        });
    }

    // 6) Hot-reload de configuración (opcional)
    #[cfg(feature = "hot-reload")]
    {
        let app_state = state.clone();
        let cfg_path = app_state.cfg.source_file.clone();
        let mut reload_shutdown_rx = shutdown.subscribe();
        tasks.spawn(async move {
            if let Err(e) =
                config::app_config::watch_and_reload(cfg_path, app_state.cfg.clone(), async move {
                    let _ = reload_shutdown_rx.recv().await;
                    info!("🔻 Shutdown para watcher de configuración");
                })
                .await
            {
                warn!(error=?e, "Hot-reload deshabilitado por error");
            }
        });
    }

    // 7) Señales del SO y espera activa
    info!("🚀 alerting-service iniciando con perfil: {}", state.cfg.env);
    let graceful = async {
        // Ctrl+C en cualquier plataforma
        let ctrl_c = async {
            signal::ctrl_c()
                .await
                .expect("No se pudo registrar handler de Ctrl+C");
        };

        // SIGTERM/SIGINT (Unix)
        #[cfg(unix)]
        let terminate = async {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term =
                signal(SignalKind::terminate()).expect("No se pudo registrar SIGTERM");
            term.recv().await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        select! {
            _ = ctrl_c => {},
            _ = terminate => {},
        }

        info!("🛑 Recibida señal de parada: iniciando graceful shutdown");
    };

    // Esperar señal y disparar shutdown
    graceful.await;
    shutdown.trigger();

    // Dar tiempo a que servidores cierren conexiones (grace period)
    let grace = Duration::from_secs(10);
    let mut timed = tokio::time::timeout(grace, async {
        while let Some(res) = tasks.join_next().await {
            if let Err(e) = res {
                error!(error=?e, "Tarea terminó con panic/error durante shutdown");
            }
        }
    });
    match &mut timed.await {
        Ok(_) => info!("🧯 Shutdown limpio completado."),
        Err(_) => warn!("⏲️  Timeout en shutdown; forzando salida."),
    }

    // 8) Cierre de pipelines de OpenTelemetry (si están activos)
    if let Err(e) = telemetry::opentelemetry_setup::shutdown_otel().await {
        warn!(error=?e, "Fallo al cerrar OpenTelemetry limpiamente");
    }
    info!("👋 Servicio detenido.");
}

// -------------------------
// Módulos mínimos requeridos
// -------------------------

pub mod adapter {
    // Estos `mod` se resuelven contra los archivos reales del proyecto.
    // Se declaran para permitir `use crate::adapter::...` en `main.rs`.
    pub mod http {
        pub mod controller;
        pub mod middleware;
        pub mod routes;
    }
    pub mod grpc {
        pub mod grpc_server;
    }
    pub mod kafka {
        pub mod kafka_consumer;
        pub mod kafka_producer;
    }
    pub mod notifier {
        pub mod email_notifier;
        pub mod sms_notifier;
        pub mod telegram_notifier;
        pub mod webpush_notifier;
        pub mod notifier_registry;
    }
}

pub mod repository {
    pub mod alert_store;
    pub mod retry_queue;
    pub mod rule_store;
}

pub mod scheduler {
    pub mod purge_scheduler;
    pub mod retry_scheduler;
}

pub mod telemetry {
    pub mod metrics;
    pub mod opentelemetry_setup;
    pub mod tracing;
}

pub mod config {
    pub mod app_config;
    pub mod kafka_config;
    pub mod smtp_config;
    pub mod tls_config;
    pub mod notifier_plugins;
}

pub mod domain {
    pub mod error;
    pub mod model {
        pub mod alert;
        pub mod delivery_status;
        pub mod severity;
        pub mod tenant;
    }
    pub mod usecase {
        pub mod classify_alert_severity;
        pub mod dispatch_notification;
        pub mod expose_alerts_api;
        pub mod generate_alert_insights;
        pub mod log_alert_audit;
        pub mod purge_old_alerts;
        pub mod receive_and_validate_event;
        pub mod replay_alerts;
        pub mod retry_failed_deliveries;
        pub mod route_event;
        pub mod simulate_alert;
        pub mod trigger_support_ticket;
        pub mod update_notifier_plugins;
        pub mod verify_alert_origin;
        pub mod register_notification_rules;
    }
    pub mod service {
        pub mod alert_classifier;
        pub mod audit_logger;
        pub mod event_deduplicator;
        pub mod notification_dispatcher;
        pub mod plugin_loader;
        pub mod routing_engine;
    }
}
