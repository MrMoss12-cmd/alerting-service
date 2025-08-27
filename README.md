# 🛎️ Alerting Service

El microservicio `alerting-service` es parte del dominio **Monitoring & Billing** del ecosistema `WorkSphere`. Su función es recibir eventos de monitoreo, analizarlos, clasificarlos, generar alertas y despachar notificaciones multicanal a sistemas externos o administradores del tenant.

Este servicio es **asíncrono, concurrente, altamente modular y multicanal**, construido con **Rust**, el runtime **Tokio** y comunica mediante **gRPC**, **Kafka**, **HTTP REST** y **Webhooks**.

---

## 📌 Características

- ✅ Recepción de eventos desde Kafka, gRPC, Webhook y HTTP.
- ✅ Detección de duplicados y validación de eventos.
- ✅ Clasificación e insights automáticos sobre alertas.
- ✅ Enrutamiento dinámico a notifiers por tenant, tipo de alerta o prioridad.
- ✅ Notificaciones por correo, Telegram u otros plugins.
- ✅ Persistencia y auditoría de alertas.
- ✅ Sistema de purgado y replay.
- ✅ Carga dinámica de plugins de notificación.
- ✅ TLS, mTLS y JWT para seguridad.

---

## 📂 Estructura del Proyecto

```bash
alerting-service/
├── api/                        # Definición de contratos gRPC
│   └── proto/
│       └── alerting.proto

├── adapter/                   # Adaptadores de entrada/salida
│   ├── http/                  # HTTP REST
│   │   ├── controller.rs
│   │   ├── routes.rs
│   │   └── middleware.rs
│   ├── kafka/                 # Kafka I/O
│   │   ├── kafka_consumer.rs
│   │   └── kafka_producer.rs
│   ├── grpc/                  # Servidor gRPC
│   │   └── grpc_server.rs
│   └── webhook/               # Webhooks externos
│       └── webhook_handler.rs

├── config/                    # Configuración dinámica
│   ├── app_config.rs
│   ├── kafka_config.rs
│   ├── smtp_config.rs
│   ├── tls_config.rs
│   └── notifier_plugins.rs

├── notifier/                  # Canales de notificación
│   ├── email_notifier.rs
│   └── telegram_notifier.rs

├── service/                   # Lógica de dominio
│   ├── event_deduplicator.rs
│   ├── alert_classifier.rs
│   ├── routing_engine.rs
│   ├── notification_dispatcher.rs
│   ├── plugin_loader.rs
│   └── audit_logger.rs

├── usecase/                   # Casos de uso (aplicación)
│   ├── receive_and_validate_event.rs
│   ├── classify_alert.rs
│   ├── dispatch_notification.rs
│   ├── update_notifier_plugins.rs
│   ├── generate_alert_insights.rs
│   └── purge_old_alerts.rs

├── scripts/                   # Utilidades para test y administración
│   ├── simulate_alert.sh
│   └── replay_alerts.sh

├── Cargo.toml
├── main.rs
└── README.md
🚀 Cómo Ejecutarlo
📦 Requisitos
Rust 1.77+

Cargo

Kafka corriendo (si se usa Kafka)

Postfix/SMTP configurado (si se usa email)

Telegram bot API key (si se usa Telegram)

🧪 Ejecutar en modo desarrollo
bash
Copiar
Editar
cargo run
🛠️ Variables de entorno requeridas
env
Copiar
Editar
APP_ENV=development
KAFKA_BROKER=localhost:9092
SMTP_HOST=smtp.gmail.com
SMTP_USER=example@gmail.com
SMTP_PASS=secret
TELEGRAM_BOT_TOKEN=abc123:token
PLUGIN_PATH=/etc/alerting/plugins/
🧠 Principios Técnicos
Componente	Detalles Técnicos
Tokio	Runtime para concurrencia asíncrona y gestión de tareas.
gRPC	Interfaz para recepción de eventos o replay manual.
Kafka	Broker principal de eventos entrantes desde microservicios de monitoreo.
HTTP + Webhook	Interfaces expuestas para control y consumo de alertas.
Plugins dinámicos	Los notifiers pueden cargarse como dinámicos desde notifier_plugins.rs.
Seguridad	Soporte para TLS, mTLS y JWT en todos los endpoints.

📘 Ejemplos de Casos de Uso
Recibir y validar evento

Verifica autenticidad (JWT/mTLS).

Rechaza duplicados.

Guarda en log de auditoría.

Clasificar alerta

Reglas heurísticas y de ML básicas.

Determina prioridad y tipo.

Despachar notificación

Determina canal.

Envía por email o Telegram.

Registra resultado.

📤 Envío de una Alerta de Prueba
bash
Copiar
Editar
curl -X POST http://localhost:8080/api/alerts \
  -H "Authorization: Bearer <JWT>" \
  -d '{"tenant_id": "tenant123", "event_type": "cpu.overload", "severity": "high"}'
📦 Simular y Rejugar Alertas
bash
Copiar
Editar
# Simular
./scripts/simulate_alert.sh

# Replay desde archivo o base de datos
./scripts/replay_alerts.sh
🛡️ Seguridad
JWT con firma HS256 para autenticación.

Mutual TLS (cliente y servidor) opcional.

Rate limiting por IP/tenant en middleware HTTP.

📄 Licencia
MIT License © 2025 — WorkSphere Platform

yaml
Copiar
Editar

---
