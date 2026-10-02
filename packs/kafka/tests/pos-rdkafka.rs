use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::message::{BorrowedMessage, Message};
use rdkafka::producer::FutureRecord;

pub const TOPICS: &[&str] = &["tote.picked", "tote.packed"];

pub fn subscribe(consumer: &StreamConsumer) -> rdkafka::error::KafkaResult<()> {
    consumer.subscribe(TOPICS)
}

pub fn record(payload: &str) -> FutureRecord<'_, str, str> {
    FutureRecord::to("tote.shipped").payload(payload)
}

pub fn route(message: &BorrowedMessage<'_>) -> &'static str {
    match message.topic() {
        "tote.lost" => "lost",
        _ => "other",
    }
}
