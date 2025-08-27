use chrono::{DateTime, Utc};
use std::collections::HashMap;
use tokio::sync::RwLock;
use uuid::Uuid;
use std::sync::Arc;

/// Severidad de la alerta
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SeverityLevel {
    Info,
    Warning,
    Critical,
    Custom(String),
}

/// Representa una alerta relevante para el ticket
#[derive(Debug, Clone)]
pub struct Alert {
    pub alert_id: String,
    pub tenant_id: String,
    pub severity: SeverityLevel,
    pub event_type: String,
    pub description: String,
    pub timestamp: DateTime<Utc>,
}

/// Ticket generado en sistema externo o interno
#[derive(Debug, Clone)]
pub struct SupportTicket {
    pub ticket_id: String,
    pub alert_id: String,
    pub tenant_id: String,
    pub system: SupportSystem,
    pub created_at: DateTime<Utc>,
    pub priority: String,
    pub status: TicketStatus,
    pub details: String, // Enriquecimiento
}

/// Estados básicos del ticket
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TicketStatus {
    Open,
    InProgress,
    Resolved,
    Closed,
}

/// Sistemas de soporte soportados
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SupportSystem {
    Zendesk,
    Jira,
    ServiceNow,
    Internal,
    Custom(String),
}

/// Servicio para administrar tickets (simulación)
#[derive(Debug, Default, Clone)]
pub struct SupportTicketService {
    // Map alert_id => ticket
    tickets: Arc<RwLock<HashMap<String, SupportTicket>>>,
}

impl SupportTicketService {
    pub fn new() -> Self {
        Self {
            tickets: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Regla simple para decidir si abrir ticket
    fn should_trigger_ticket(alert: &Alert) -> bool {
        matches!(alert.severity, SeverityLevel::Critical)
    }

    /// Generar prioridad según severidad
    fn priority_from_severity(severity: &SeverityLevel) -> String {
        match severity {
            SeverityLevel::Critical => "High".into(),
            SeverityLevel::Warning => "Medium".into(),
            SeverityLevel::Info => "Low".into(),
            SeverityLevel::Custom(_) => "Medium".into(),
        }
    }

    /// Enriquecer detalles del ticket con logs y pasos (simulado)
    async fn enrich_ticket_details(&self, alert: &Alert) -> String {
        format!(
            "Alerta ID: {}\nDescripción: {}\nTimestamp: {}\nPasos sugeridos: Ver logs relacionados y métricas de red.",
            alert.alert_id, alert.description, alert.timestamp
        )
    }

    /// Crear un ticket en un sistema específico
    async fn create_ticket_in_system(
        &self,
        alert: &Alert,
        system: &SupportSystem,
        details: String,
    ) -> SupportTicket {
        // Simulación de creación externa, podría ser HTTP/gRPC en realidad
        SupportTicket {
            ticket_id: Uuid::new_v4().to_string(),
            alert_id: alert.alert_id.clone(),
            tenant_id: alert.tenant_id.clone(),
            system: system.clone(),
            created_at: Utc::now(),
            priority: Self::priority_from_severity(&alert.severity),
            status: TicketStatus::Open,
            details,
        }
    }

    /// Generar ticket si corresponde y registrar en sistema interno
    pub async fn trigger_ticket(
        &self,
        alert: Alert,
        systems: &[SupportSystem], // Sistemas posibles donde crear ticket
    ) -> Option<SupportTicket> {
        if !Self::should_trigger_ticket(&alert) {
            return None;
        }

        let details = self.enrich_ticket_details(&alert).await;

        // Aquí podemos crear tickets en varios sistemas, pero por simplicidad creamos en el primero disponible
        if let Some(system) = systems.first() {
            let ticket = self.create_ticket_in_system(&alert, system, details).await;

            // Guardar en memoria (simula persistencia)
            let mut tickets = self.tickets.write().await;
            tickets.insert(alert.alert_id.clone(), ticket.clone());

            Some(ticket)
        } else {
            None
        }
    }

    /// Obtener ticket relacionado a una alerta
    pub async fn get_ticket_for_alert(&self, alert_id: &str) -> Option<SupportTicket> {
        let tickets = self.tickets.read().await;
        tickets.get(alert_id).cloned()
    }

    /// Actualizar estado del ticket (simulado)
    pub async fn update_ticket_status(
        &self,
        alert_id: &str,
        new_status: TicketStatus,
    ) -> Result<(), String> {
        let mut tickets = self.tickets.write().await;
        if let Some(ticket) = tickets.get_mut(alert_id) {
            ticket.status = new_status;
            Ok(())
        } else {
            Err("Ticket no encontrado para esa alerta".into())
        }
    }
}

// --- Ejemplo de uso ---

#[tokio::main]
async fn main() {
    let ticket_service = SupportTicketService::new();

    let alert = Alert {
        alert_id: "alert-001".to_string(),
        tenant_id: "tenant-123".to_string(),
        severity: SeverityLevel::Critical,
        event_type: "payment.failure".to_string(),
        description: "Falla crítica en pago, requiere atención inmediata".to_string(),
        timestamp: Utc::now(),
    };

    let systems = vec![
        SupportSystem::Zendesk,
        SupportSystem::Jira,
        SupportSystem::Internal,
    ];

    if let Some(ticket) = ticket_service.trigger_ticket(alert.clone(), &systems).await {
        println!("Ticket creado: {:?}", ticket);

        // Simular actualización de estado
        ticket_service
            .update_ticket_status(&alert.alert_id, TicketStatus::InProgress)
            .await
            .unwrap();

        let updated = ticket_service.get_ticket_for_alert(&alert.alert_id).await.unwrap();
        println!("Ticket actualizado: {:?}", updated);
    } else {
        println!("No se creó ticket para la alerta.");
    }
}
