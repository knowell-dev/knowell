//! `handbook`: architecture decision records, runbooks and the glossary.

pub(super) const FILES: &[(&str, &str)] = &[
    (
        "README.md",
        r##"
# Acme Goods engineering handbook

## Architecture decision records

| ADR | Title |
|---|---|
| [0001](adr/0001-record-architecture-decisions.md) | Record architecture decisions |
| [0002](adr/0002-domain-events-on-kafka.md) | Domain events on Kafka |
| [0003](adr/0003-shared-postgres-table-ownership.md) | Shared Postgres with table ownership |
| [0004](adr/0004-idempotency-keys-for-payment-capture.md) | Idempotency keys for payment capture |
| [0005](adr/0005-cancel-at-period-end.md) | Cancel memberships at period end |
| [0006](adr/0006-bildirim-iscisi-rust.md) | Bildirim işçisi için Rust |

## Runbooks

- [Payment capture incidents](runbooks/payment-capture-incidents.md)

See also the [glossary](glossary.md).
"##,
    ),
    (
        "glossary.md",
        r##"
# Glossary

**Acme Plus** - the paid membership: free delivery, member prices and a
monthly replenishment box. Code calls it a *subscription*.

**Replenishment box** - an order created automatically for an Acme Plus
member (`orders.kind = 'replenishment'`).

**Capture** - taking money from the customer's card after authorisation.
Only ledger-service talks to the payment provider.

**PSP** - payment service provider, the external card processor.

**Idempotency key** - client-chosen identifier that makes a retried request
return the first result instead of acting again (ADR-0004).

**Period end** - `subscriptions.current_period_end`; a cancelled member keeps
benefits until then (ADR-0005).

**Minor units** - amounts are integers in cents / kuruş.
"##,
    ),
    (
        "adr/0001-record-architecture-decisions.md",
        r##"
# ADR-0001: Record architecture decisions

Status: accepted

## Context

Decisions about service boundaries, data ownership and integration patterns
were made in meetings and chat threads and were hard to find later.

## Decision

We keep lightweight architecture decision records in this handbook, one
Markdown file per decision, numbered sequentially, with the sections
Context, Decision and Consequences. ADRs are immutable once accepted; a later
ADR supersedes an earlier one explicitly. Turkish or English are both fine.

## Consequences

New engineers can read why the system looks the way it does. Every ADR is
reviewed like code.
"##,
    ),
    (
        "adr/0002-domain-events-on-kafka.md",
        r##"
# ADR-0002: Domain events on Kafka

Status: accepted

## Context

Services called each other synchronously for side effects (e-mails, order
updates), so an outage of the mail relay could fail a cancellation.

## Decision

State changes that other services react to are published as domain events
on Kafka.

- The topic name is the event type, written `<entity>.<past-tense verb>`:
  `subscription.cancelled`, `subscription.resumed`, `payment.captured`.
- Every message is an envelope `{id, type, version, occurredAt, data}`.
  Payload schemas live in the contracts repository (`events/*.v1.json`).
- Publishers use idempotent producers; consumers commit offsets only after
  successful handling and must tolerate redelivery (dedupe on envelope id
  where a side effect is not naturally repeatable).

## Consequences

Producers do not know their consumers; the contracts repository is the place
to find them. Delivery is at-least-once.
"##,
    ),
    (
        "adr/0003-shared-postgres-table-ownership.md",
        r##"
# ADR-0003: Shared Postgres with table ownership

Status: accepted

## Context

Splitting into one database per service was too expensive for our team size,
but several services writing the same tables caused incidents.

## Decision

All services use one Postgres cluster. Every table has exactly one owning
service, the only one allowed to write it (ownership table in
db-migrations/README.md). Other services may map a table read-only when a
synchronous API call would be too slow, e.g. ledger-service reads
`subscriptions` to compute refunds.

Schema changes are made only through db-migrations. A migration must be
released together with the model changes of the owner and of every read
model of the table.

## Consequences

Read models silently drift when a migration forgets one of them; reviews must
check all services that map a changed table.
"##,
    ),
    (
        "adr/0004-idempotency-keys-for-payment-capture.md",
        r##"
# ADR-0004: Idempotency keys for payment capture

Status: accepted

## Context

Mobile networks drop responses. Customers pressed "Pay" again, the app
retried after timeouts, and the ledger occasionally created a second capture
for the same checkout. Support had to refund these by hand.

## Decision

`POST /v1/payments/capture` requires an `Idempotency-Key` header.

- The website and the app create one key per checkout attempt and reuse it
  for retries; billing-api forwards it to the ledger.
- The ledger stores the key with a SHA-256 fingerprint of the request body in
  `idempotency_keys` (primary key on the key, so reservation is atomic).
- A repeated key with the same fingerprint replays the stored response and
  never calls the provider again; a different body is rejected with 422; a
  request still in flight answers 409.
- Keys expire after 24 hours (`IDEMPOTENCY_TTL_HOURS`).
- We also pass the key to the provider, which dedupes on its side.

## Consequences

Retries are safe end to end. Clients must keep the key for the lifetime of
an attempt; generating a new key per retry defeats the mechanism.
"##,
    ),
    (
        "adr/0005-cancel-at-period-end.md",
        r##"
# ADR-0005: Cancel memberships at period end

Status: accepted

## Context

Immediate cancellation forced us to refund partial months and confused
members who lost free delivery mid-month.

## Decision

Cancelling an Acme Plus membership marks it `cancelled` immediately but
benefits stay active until `current_period_end`; a nightly job then sets it
to `expired`. Until that date the member can resume.

- Monthly plans: no refund, the paid month runs out.
- Yearly plans: unused whole months are refunded pro rata by the ledger
  (`POST /v1/refunds/prorated`); the month in progress is not refunded.
- Scheduled replenishment boxes after the period end are cancelled by
  orders-service when it sees `subscription.cancelled`.

## Consequences

"Cancelled" does not mean "no access"; UIs must show the access-until date.
"##,
    ),
    (
        "adr/0006-bildirim-iscisi-rust.md",
        r##"
# ADR-0006: Bildirim işçisi için Rust

Durum: kabul edildi

## Bağlam

E-posta gönderimi billing-api içinde senkron yapılıyordu. SMTP sunucusu
yavaşladığında iptal istekleri zaman aşımına uğruyordu. Ayrı bir işçi
(worker) servisine ihtiyaç vardı; düşük bellek kullanımı ve öngörülebilir
gecikme istiyorduk.

## Karar

Bildirimler, Kafka olaylarını dinleyen ayrı bir `notification-worker`
servisine taşındı ve bu servis Rust ile yazıldı:

- Tek bir küçük ikili dosya, 64 MiB bellekle çalışıyor.
- Tip sistemi olay şemalarındaki hataları derleme sırasında yakalıyor.
- Ekipte Rust deneyimi olan iki kişi var; servis küçük olduğu için risk düşük.

## Sonuçlar

Başarısız gönderimler üstel bekleme ile yeniden deneniyor; mesaj ancak
başarıdan sonra onaylanıyor. Yeni bir e-posta türü eklemek için yeni bir
handler ve iki dilde şablon gerekiyor.
"##,
    ),
    (
        "runbooks/payment-capture-incidents.md",
        r##"
# Runbook: payment capture incidents

## Symptoms

- Spike of 5xx from `POST /v1/payments/capture`.
- Orders stuck in `pending` although customers report a successful payment.

## Checks

1. Provider status page and the ledger's timeout rate (`PSP_TIMEOUT_SECONDS`).
2. Lag of the `orders-service` consumer group on `payment.captured`.
3. Rows in `idempotency_keys` with a NULL `response_status` older than ten
   minutes: requests that died mid-flight. Their retries answer 409.

## Remedies

- Stuck in-flight keys: confirm the capture with the provider, then complete
  or delete the row so the client's retry can proceed.
- Consumer lag: scale orders-service; it is safe to replay `payment.captured`
  because marking an order paid is a no-op the second time.
"##,
    ),
];
