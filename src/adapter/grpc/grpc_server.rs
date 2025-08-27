// adapter/grpc/grpc_server.rs

use tonic::{transport::Server, Request, Response, Status, Code};
use tonic::metadata::MetadataMap;
use tonic::service::Interceptor;
use tonic::metadata::MetadataValue;
use tonic::codegen::InterceptedService;
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
use tokio::sync::mpsc;
use tracing::{info, error};
use uuid::Uuid;

use std::pin::Pin;
use futures_core::Stream;

pub mod alerting {
    tonic::include_proto!("alerting.v1"); // Protobuf package, assumed compiled with prost
}

use alerting::{
    alert_service_server::{AlertService, AlertServiceServer},
    AlertRequest, AlertResponse, Alert, ReplayRequest, ReplayResponse, ListAlertsRequest, ListAlertsResponse,
};

type ResponseStream = Pin<Box<dyn Stream<Item = Result<AlertResponse, Status>> + Send + Sync + 'static>>;

// --- JWT Authentication Interceptor ---

#[derive(Clone)]
pub struct JwtInterceptor {
    // In real use, this would contain keys/configs
}

impl Interceptor for JwtInterceptor {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let metadata = request.metadata();
        if let Some(token) = extract_bearer_token(metadata) {
            if validate_jwt(&token) {
                Ok(request)
            } else {
                Err(Status::unauthenticated("Invalid JWT token"))
            }
        } else {
            Err(Status::unauthenticated("Authorization token missing"))
        }
    }
}

fn extract_bearer_token(metadata: &MetadataMap) -> Option<String> {
    metadata.get("authorization").and_then(|val| val.to_str().ok())
        .and_then(|auth| {
            if auth.starts_with("Bearer ") {
                Some(auth[7..].to_string())
            } else {
                None
            }
        })
}

// Dummy JWT validation stub (replace with real validation)
fn validate_jwt(token: &str) -> bool {
    // In real life: decode, verify signature, check claims, expiration, scopes
    !token.is_empty()
}

// --- Core Alert Service Implementation ---

#[derive(Default)]
pub struct AlertingServer {}

#[tonic::async_trait]
impl AlertService for AlertingServer {
    // Unary RPC: Receive alert
    async fn send_alert(&self, request: Request<AlertRequest>) -> Result<Response<AlertResponse>, Status> {
        let alert = request.into_inner().alert.ok_or_else(|| Status::invalid_argument("Missing alert data"))?;

        info!("Received alert with ID: {}", alert.id);

        // Validate alert (simplified)
        if alert.id.is_empty() {
            return Err(Status::invalid_argument("Alert ID cannot be empty"));
        }

        // Here: Call domain logic, enqueue alert, etc.

        Ok(Response::new(AlertResponse {
            status: "received".to_string(),
            alert_id: alert.id,
        }))
    }

    // Server streaming: List alerts with pagination and filters
    type ListAlertsStream = ResponseStream;

    async fn list_alerts(&self, request: Request<ListAlertsRequest>) -> Result<Response<Self::ListAlertsStream>, Status> {
        let req = request.into_inner();
        info!("Listing alerts for tenant: {}", req.tenant_id);

        // Here: Query domain/store for alerts filtered by tenant, severity, dates, page, size...

        // Dummy example: stream 5 fake alerts
        let (tx, rx) = mpsc::channel(5);

        tokio::spawn(async move {
            for i in 0..5 {
                let alert = AlertResponse {
                    status: "delivered".to_string(),
                    alert_id: format!("alert-{}", i),
                };
                if tx.send(Ok(alert)).await.is_err() {
                    error!("Client disconnected during alert stream");
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx)) as Self::ListAlertsStream))
    }

    // Bidirectional streaming example: streaming alert send + ack
    type StreamAlertsStream = ResponseStream;

    async fn stream_alerts(&self, request: Request<tonic::Streaming<AlertRequest>>) -> Result<Response<Self::StreamAlertsStream>, Status> {
        let mut stream = request.into_inner();

        let (tx, rx) = mpsc::channel(10);

        tokio::spawn(async move {
            while let Some(result) = stream.next().await {
                match result {
                    Ok(alert_req) => {
                        if let Some(alert) = alert_req.alert {
                            info!("Streamed alert received: ID={}", alert.id);
                            // Here: Validate and process alert

                            let resp = AlertResponse {
                                status: "acknowledged".to_string(),
                                alert_id: alert.id.clone(),
                            };
                            if tx.send(Ok(resp)).await.is_err() {
                                error!("Response channel closed");
                                break;
                            }
                        } else {
                            let err_resp = Err(Status::invalid_argument("Missing alert data in stream"));
                            if tx.send(err_resp).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        error!("Error reading stream: {:?}", e);
                        break;
                    }
                }
            }
            info!("Stream closed by client");
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx)) as Self::StreamAlertsStream))
    }

    // Unary replay command
    async fn replay_alerts(&self, request: Request<ReplayRequest>) -> Result<Response<ReplayResponse>, Status> {
        let req = request.into_inner();

        info!("Replay request for tenant: {}, from {} to {}", req.tenant_id, req.start_timestamp, req.end_timestamp);

        // Here: validate permissions, apply replay logic safely

        Ok(Response::new(ReplayResponse {
            status: "replay_started".to_string(),
            replay_id: Uuid::new_v4().to_string(),
        }))
    }
}

// --- Server bootstrap ---

pub async fn start_grpc_server(addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Setup tracing subscriber or logging before this

    let interceptor = JwtInterceptor {};

    let svc = AlertServiceServer::with_interceptor(AlertingServer::default(), interceptor);

    info!("Starting gRPC server on {}", addr);

    Server::builder()
        .add_service(svc)
        .serve(addr.parse()?)
        .await?;

    Ok(())
}
