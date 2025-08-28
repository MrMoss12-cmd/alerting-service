# 📡 Alerting Service

## 🎯 Rol principal

El microservicio **alerting-service** es el núcleo del sistema de **monitoreo y gestión de alertas multitenant**.  
Su objetivo es **detectar, procesar y notificar alertas críticas** de múltiples fuentes en tiempo real, garantizando **seguridad, auditabilidad y alta disponibilidad**.  

---

## ⚙️ Cómo funciona

### 🔔 Ingesta y procesamiento de alertas
- APIs **gRPC/REST** definidas en `api/proto/alerting.proto`.  
- Procesamiento mediante motor de correlación y normalización.  
- Soporte para alertas **unitarias y streaming**.  

### 🧹 Filtros y políticas
- Configuración por **tenant, severidad, estado o timestamp**.  
- Políticas de deduplicación y correlación de eventos.  

### ⏪ Replay y simulación
- `replay_alerts.sh`: reenvío de alertas históricas para auditoría o pruebas.  
- `simulate_alert.sh`: generación de alertas sintéticas para validación en CI/CD.  

### 📤 Notificaciones y destinos
- Integración con **Kafka** y buses de eventos.  
- Notificación a **email, Slack, PagerDuty** u otros.  
- Registro de entregas con **idempotencia y trazabilidad**.  

### 🔒 Seguridad y trazabilidad
- Metadatos con **tenant-id, correlation-id y credenciales JWT/mTLS**.  
- Auditoría de cada alerta procesada y notificada.  

### ⚡ Escalabilidad y resiliencia
- Construido sobre **Tokio** para concurrencia eficiente.  
- Workers distribuidos para alta carga.  
- Configuración flexible: `config/config.yaml`.  

---

## 🔄 Evolución futura

- 🤖 **Machine learning**: detección de anomalías y correlación inteligente.  
- 📊 **UI de monitoreo**: panel visual en tiempo real.  
- ⏯️ **Replay avanzado**: flujos completos de alertas.  
- 📈 **Integración con métricas**: Prometheus / OpenTelemetry.  
- 🛠️ **Self-healing**: disparo automático de acciones correctivas.  

---

## 📂 Casos de uso principales

### 1️⃣ Ingesta y normalización
- `receive_alert`: recibir y validar alertas entrantes.  
- `normalize_alert`: estandarizar formato y severidad.  

### 2️⃣ Filtros y correlación
- `filter_alerts`: aplicar criterios por tenant o criticidad.  
- `deduplicate_alerts`: evitar notificaciones redundantes.  
- `correlate_alerts`: agrupar alertas relacionadas.  

### 3️⃣ Replay y simulación
- `replay_alerts`: reinyectar alertas históricas.  
- `simulate_alert`: generar alertas sintéticas.  

### 4️⃣ Notificación y entrega
- `emit_alert_event`: emitir evento hacia Kafka/RabbitMQ.  
- `notify_external_system`: integrar con email, Slack, PagerDuty.  
- `log_delivery`: registrar historial de notificaciones.  

### 5️⃣ Monitoreo y auditoría
- `audit_alerts`: registrar todas las operaciones.  
- `export_alert_logs`: generar reportes de incidentes.  

### 6️⃣ Seguridad y acceso
- `secure_api_endpoints`: proteger endpoints con JWT/mTLS.  
- `authorize_tenant_access`: validar permisos por tenant.  

---

## 🗂️ Arquitectura (Mermaid)

```mermaid
flowchart TD
    A[🔔 Fuente de alertas] -->|gRPC/REST| B[📡 Alerting Service]
    B --> C[🧹 Motor de filtros y correlación]
    C --> D[📤 Notificaciones externas]
    C --> E[📦 Kafka / Bus de eventos]
    D -->|Slack/Email/PagerDuty| F[👥 Equipos de respuesta]
    E --> G[(📊 Almacenamiento/Auditoría)]


---

## 🎯 Rol principal

El alerting-service es el **cerebro de monitoreo y notificaciones**.  
Centraliza la ingesta de alertas, las filtra, correlaciona y reenvía de forma **segura, auditable y escalable** garantizando que los equipos reciban la información crítica a tiempo. 
---