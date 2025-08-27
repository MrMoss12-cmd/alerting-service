use rdkafka::{
    consumer::{Consumer, StreamConsumer, CommitMode},
    message::{Message, BorrowedMessage},
    ClientConfig, TopicPartitionList,
    error::KafkaError,
};
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc::Sender;
use tracing::{error, info};

pub struct KafkaConsumer {
    consumer: StreamConsumer,
}

impl KafkaConsumer {
    pub async fn new(brokers: &str, group_id: &str, topics: &[&str]) -> Result<Self, KafkaError> {
        let consumer: StreamConsumer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("group.id", group_id)
            .set("enable.partition.eof", "false")
            .set("session.timeout.ms", "6000")
            .set("enable.auto.commit", "false") // Commit manual
            .create()?;

        consumer.subscribe(topics)?;

        Ok(Self { consumer })
    }

    /// Ejecuta el bucle de consumo de mensajes Kafka
    /// Envía mensajes validados al canal `tx` para procesamiento downstream
    pub async fn run(&self, tx: Sender<Value>) {
        info!("KafkaConsumer started");
        let mut message_stream = self.consumer.stream();

        while let Some(result) = message_stream.next().await {
            match result {
                Ok(msg) => {
                    if let Err(e) = self.process_message(&msg, &tx).await {
                        error!("Failed to process message: {:?}", e);
                        // No interrumpir la recepción, seguir procesando
                    }
                }
                Err(e) => {
                    error!("Kafka error: {:?}", e);
                    // Aquí podría implementar backoff o reconexión automática si es necesario
                }
            }
        }
    }

    async fn process_message(&self, msg: &BorrowedMessage<'_>, tx: &Sender<Value>) -> Result<(), KafkaError> {
        let payload = match msg.payload_view::<str>() {
            Some(Ok(payload_str)) => payload_str,
            Some(Err(e)) => {
                error!("Invalid UTF-8 payload: {:?}", e);
                self.consumer.commit_message(msg, CommitMode::Async)?;
                return Ok(());
            }
            None => {
                error!("Empty payload");
                self.consumer.commit_message(msg, CommitMode::Async)?;
                return Ok(());
            }
        };

        // Validación básica: parsear JSON
        let json: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(e) => {
                error!("Invalid JSON format: {:?}", e);
                self.consumer.commit_message(msg, CommitMode::Async)?;
                return Ok(());
            }
        };

        // Correlación para trazabilidad (se puede extraer headers)
        if let Some(correlation_id) = msg.headers().and_then(|h| h.get_last("correlation_id")) {
            info!("Received message with correlation_id: {:?}", correlation_id);
        }

        // Enviar a canal para procesamiento posterior
        match tx.send(json).await {
            Ok(_) => {
                // Commit manual tras procesamiento correcto
                self.consumer.commit_message(msg, CommitMode::Async)?;
            }
            Err(e) => {
                error!("Failed to send message to processing channel: {:?}", e);
                // No hacer commit para reintentar después
            }
        }

        Ok(())
    }
}
