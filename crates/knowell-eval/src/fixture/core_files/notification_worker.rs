//! `notification-worker`: e-mails triggered by domain events (Rust).

pub(super) const FILES: &[(&str, &str)] = &[
    (
        "README.md",
        r##"
# notification-worker

Consumes domain events from Kafka and sends transactional e-mail over SMTP.

| Topic | E-mail |
|---|---|
| `subscription.cancelled` | "your membership was cancelled" (en / tr) |
| `payment.captured` | payment receipt |

Templates are plain text in `templates/`, one file per language.

## Configuration

| Variable | Meaning |
|---|---|
| `KAFKA_BROKERS` | comma-separated broker list |
| `SMTP_HOST`, `SMTP_PORT` | mail relay (STARTTLS) |
| `SMTP_USERNAME`, `SMTP_PASSWORD` | relay credentials (from the secret store) |
| `MAIL_FROM` | sender address |
| `MAIL_MAX_ATTEMPTS` | delivery attempts before a message is left for redelivery |

Why Rust: see handbook ADR-0006.
"##,
    ),
    (
        "Cargo.toml",
        r##"
[package]
name = "notification-worker"
version = "0.7.1"
edition = "2021"
publish = false

[dependencies]
anyhow = "1"
lettre = { version = "0.11", default-features = false, features = ["builder", "smtp-transport", "tokio1-rustls-tls"] }
rdkafka = "0.36"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "time"] }
tracing = "0.1"
"##,
    ),
    (
        "src/main.rs",
        r##"
mod config;
mod consumer;
mod handlers;
mod mailer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = config::Config::from_env()?;
    let mailer = mailer::SmtpMailer::new(&config)?;
    consumer::run(&config, &mailer).await
}
"##,
    ),
    (
        "src/config.rs",
        r##"
//! Worker configuration from environment variables. The variable names are
//! declared in infra/docker-compose.yml and infra/k8s/notification-worker.yaml.

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub kafka_brokers: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_username: String,
    pub smtp_password: String,
    pub mail_from: String,
    pub max_send_attempts: u32,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            kafka_brokers: required("KAFKA_BROKERS")?,
            smtp_host: required("SMTP_HOST")?,
            smtp_port: optional("SMTP_PORT", "587")
                .parse()
                .context("SMTP_PORT must be a port number")?,
            smtp_username: required("SMTP_USERNAME")?,
            smtp_password: required("SMTP_PASSWORD")?,
            mail_from: optional("MAIL_FROM", "Acme Goods <no-reply@example.com>"),
            max_send_attempts: optional("MAIL_MAX_ATTEMPTS", "5")
                .parse()
                .context("MAIL_MAX_ATTEMPTS must be a number")?,
        })
    }
}

fn required(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is not set"))
}

fn optional(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}
"##,
    ),
    (
        "src/consumer.rs",
        r##"
//! Kafka consumer loop. Each topic maps to one handler; a message is
//! committed only after its handler succeeded, so a crash or a failed send
//! leads to redelivery instead of a lost e-mail.

use anyhow::Result;
use rdkafka::consumer::{CommitMode, Consumer, StreamConsumer};
use rdkafka::{ClientConfig, Message};

use crate::config::Config;
use crate::handlers;
use crate::mailer::SmtpMailer;

pub const TOPICS: &[&str] = &["subscription.cancelled", "payment.captured"];

pub async fn run(config: &Config, mailer: &SmtpMailer) -> Result<()> {
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &config.kafka_brokers)
        .set("group.id", "notification-worker")
        .set("enable.auto.commit", "false")
        .create()?;
    consumer.subscribe(TOPICS)?;

    loop {
        let message = consumer.recv().await?;
        let payload = message.payload().unwrap_or_default();
        let outcome = match message.topic() {
            "subscription.cancelled" => handlers::subscription_cancelled::handle(payload, mailer).await,
            "payment.captured" => handlers::payment_receipt::handle(payload, mailer).await,
            other => {
                tracing::warn!(topic = other, "no handler for topic");
                Ok(())
            }
        };
        match outcome {
            Ok(()) => consumer.commit_message(&message, CommitMode::Async)?,
            Err(error) => tracing::error!(%error, "handler failed; the message will be redelivered"),
        }
    }
}
"##,
    ),
    (
        "src/mailer.rs",
        r##"
//! SMTP delivery. Transient failures (connection problems, 4xx replies) are
//! retried with exponential backoff up to `MAIL_MAX_ATTEMPTS`; permanent 5xx
//! rejections are returned immediately.

use std::time::Duration;

use anyhow::Result;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::config::Config;

pub struct Email {
    pub to: String,
    pub subject: String,
    pub body: String,
}

pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    max_attempts: u32,
}

impl SmtpMailer {
    pub fn new(config: &Config) -> Result<Self> {
        let transport = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.smtp_host)?
            .port(config.smtp_port)
            .credentials(Credentials::new(config.smtp_username.clone(), config.smtp_password.clone()))
            .build();
        Ok(Self {
            transport,
            from: config.mail_from.parse()?,
            max_attempts: config.max_send_attempts.max(1),
        })
    }

    pub async fn send(&self, email: &Email) -> Result<()> {
        let message = Message::builder()
            .from(self.from.clone())
            .to(email.to.parse()?)
            .subject(email.subject.clone())
            .body(email.body.clone())?;
        let mut delay = Duration::from_millis(500);
        let mut attempt = 1;
        loop {
            match self.transport.send(message.clone()).await {
                Ok(_) => return Ok(()),
                Err(error) if error.is_permanent() => return Err(error.into()),
                Err(error) if attempt < self.max_attempts => {
                    tracing::warn!(attempt, %error, "smtp send failed, backing off");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                    attempt += 1;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}
"##,
    ),
    (
        "src/handlers/mod.rs",
        r##"
pub mod payment_receipt;
pub mod subscription_cancelled;
"##,
    ),
    (
        "src/handlers/subscription_cancelled.rs",
        r##"
//! `subscription.cancelled` -> "your membership was cancelled" e-mail in the
//! member's language, stating until when the benefits stay active.

use anyhow::Result;
use serde::Deserialize;

use crate::mailer::{Email, SmtpMailer};

const TEMPLATE_EN: &str = include_str!("../../templates/subscription_cancelled.en.txt");
const TEMPLATE_TR: &str = include_str!("../../templates/subscription_cancelled.tr.txt");

#[derive(Debug, Deserialize)]
struct Envelope {
    data: SubscriptionCancelled,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscriptionCancelled {
    subscription_id: String,
    customer_email: String,
    locale: String,
    access_until: String,
}

pub async fn handle(payload: &[u8], mailer: &SmtpMailer) -> Result<()> {
    let event = serde_json::from_slice::<Envelope>(payload)?.data;
    let (subject, template) = match event.locale.as_str() {
        "tr" => ("Aboneliğiniz iptal edildi", TEMPLATE_TR),
        _ => ("Your Acme Plus membership was cancelled", TEMPLATE_EN),
    };
    let body = template
        .replace("{{access_until}}", &event.access_until)
        .replace("{{subscription_id}}", &event.subscription_id);
    mailer
        .send(&Email {
            to: event.customer_email,
            subject: subject.to_owned(),
            body,
        })
        .await
}
"##,
    ),
    (
        "src/handlers/payment_receipt.rs",
        r##"
//! `payment.captured` -> payment receipt e-mail.

use anyhow::Result;
use serde::Deserialize;

use crate::mailer::{Email, SmtpMailer};

#[derive(Debug, Deserialize)]
struct Envelope {
    data: PaymentCaptured,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PaymentCaptured {
    order_id: String,
    amount_minor: i64,
    currency: String,
    customer_email: Option<String>,
}

pub async fn handle(payload: &[u8], mailer: &SmtpMailer) -> Result<()> {
    let event = serde_json::from_slice::<Envelope>(payload)?.data;
    let Some(to) = event.customer_email else {
        tracing::info!(order = %event.order_id, "no e-mail address on payment event, receipt skipped");
        return Ok(());
    };
    let amount = format!("{}.{:02} {}", event.amount_minor / 100, event.amount_minor % 100, event.currency);
    mailer
        .send(&Email {
            to,
            subject: format!("Receipt for order {}", event.order_id),
            body: format!("Thank you! We received your payment of {amount} for order {}.\n", event.order_id),
        })
        .await
}
"##,
    ),
    (
        "templates/subscription_cancelled.en.txt",
        r##"
Hello,

your Acme Plus membership has been cancelled. You keep free delivery and
member prices until {{access_until}}.

Changed your mind? You can resume the membership from your account page
before that date.

The Acme Goods team
"##,
    ),
    (
        "templates/subscription_cancelled.tr.txt",
        r##"
Merhaba,

Acme Plus üyeliğiniz iptal edildi. Ücretsiz teslimat ve üyelere özel
fiyatlardan {{access_until}} tarihine kadar yararlanmaya devam edebilirsiniz.

Fikrinizi mi değiştirdiniz? Bu tarihten önce hesap sayfanızdan üyeliğinizi
yeniden başlatabilirsiniz.

Acme Goods ekibi
"##,
    ),
];
