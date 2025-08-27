use rdkafka::{
    producer::{FutureProducer, FutureRecord},
    ClientConfig,
    error::KafkaError,
};
use serde::Serialize;
use std::time::Duration;
use tracing::{error, info};

pub struct KafkaProducer {
    producer: FutureProducer,
    topic: String,
}

impl KafkaProducer {
    pub fn new(brokers: &str, topic: &str) -> Result<Self, KafkaError> {
        let producer = ClientConfig::new()
            .set("bootstrap.servers", brokers)
            .set("message.timeout.ms", "5000")
            .create()?;

        Ok(Self {
            producer,
            topic: topic.to_string(),
        })
    }

    /// Publica un evento serializable en Kafka con reintentos y confirmación
    pub async fn send<T: Serialize + ?Sized>(&self, key: &str, payload: &T) -> Result<(), KafkaError> {
        let payload_str = match serde_json::to_string(payload) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to serialize payload: {:?}", e);
                return Err(KafkaError::MessageProduction(e.to_string()));
            }
        };

        let record = FutureRecord::to(&self.topic)
            .payload(&payload_str)
            .key(key);

        let produce_future = self.producer.send(record, Duration::from_secs(0));

        match produce_future.await {
            Ok(delivery) => {
                match delivery {
                    Ok(_) => {
                        info!("Message delivered to topic {}", &self.topic);
                        Ok(())
                    }
                    Err((e, _)) => {
                        error!("Delivery failed: {:?}", e);
                        Err(e)
                    }
                }
            }
            Err(e) => {
                error!("Producer future error: {:?}", e);
                Err(KafkaError::MessageProduction(e.to_string()))
            }
        }
    }
}
