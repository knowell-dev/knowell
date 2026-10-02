//! `db-migrations`: SQL migrations of the shared Postgres database.

pub(super) const FILES: &[(&str, &str)] = &[
    (
        "README.md",
        r##"
# db-migrations

Forward-only SQL migrations for the shared `acme` Postgres database, applied
by the deploy pipeline with

    migrate -path migrations -database "$DATABASE_URL" up

## Table ownership (ADR-0003)

| Table | Owner (only writer) | Also mapped by |
|---|---|---|
| `customers` | identity team | - |
| `plans`, `subscriptions` | billing-api | ledger-service (read model) |
| `orders`, `products` | orders-service | - |
| `payments`, `refunds`, `idempotency_keys` | ledger-service | - |
| `notification_log` | notification-worker | - |

Rule: a migration that changes a table must ship together with the entity /
model changes of the owner **and of every service that maps the table**.
"##,
    ),
    (
        "migrations/0001_create_customers.sql",
        r##"
CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE customers (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    email      varchar(320) NOT NULL UNIQUE,
    locale     varchar(8)   NOT NULL DEFAULT 'en',
    created_at timestamptz  NOT NULL DEFAULT now()
);
"##,
    ),
    (
        "migrations/0002_create_plans.sql",
        r##"
CREATE TABLE plans (
    id          varchar(64)  PRIMARY KEY,
    name        varchar(120) NOT NULL,
    price_minor integer      NOT NULL CHECK (price_minor >= 0),
    currency    char(3)      NOT NULL,
    interval    varchar(8)   NOT NULL CHECK (interval IN ('month', 'year')),
    active      boolean      NOT NULL DEFAULT true
);

INSERT INTO plans (id, name, price_minor, currency, interval) VALUES
    ('plus-monthly', 'Acme Plus monthly', 799, 'EUR', 'month'),
    ('plus-yearly', 'Acme Plus yearly', 7990, 'EUR', 'year');
"##,
    ),
    (
        "migrations/0003_create_subscriptions.sql",
        r##"
CREATE TABLE subscriptions (
    id                   uuid        PRIMARY KEY,
    customer_id          uuid        NOT NULL REFERENCES customers (id),
    plan_id              varchar(64) NOT NULL REFERENCES plans (id),
    status               varchar(16) NOT NULL CHECK (status IN ('active', 'past_due', 'cancelled', 'expired')),
    interval             varchar(8)  NOT NULL,
    current_period_start timestamptz NOT NULL,
    current_period_end   timestamptz NOT NULL,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX subscriptions_customer_id_idx ON subscriptions (customer_id);
"##,
    ),
    (
        "migrations/0004_create_orders.sql",
        r##"
CREATE TABLE products (
    sku         varchar(64)  PRIMARY KEY,
    title       varchar(200) NOT NULL,
    price_minor integer      NOT NULL
);

CREATE TABLE orders (
    id               uuid        PRIMARY KEY,
    customer_id      uuid        NOT NULL REFERENCES customers (id),
    subscription_id  uuid        REFERENCES subscriptions (id),
    kind             varchar(16) NOT NULL CHECK (kind IN ('one_off', 'replenishment')),
    status           varchar(16) NOT NULL CHECK (status IN ('pending', 'paid', 'shipped', 'cancelled')),
    total_minor      bigint      NOT NULL,
    currency         char(3)     NOT NULL,
    client_reference varchar(128),
    payment_id       uuid,
    scheduled_for    date,
    placed_at        timestamptz NOT NULL DEFAULT now(),
    paid_at          timestamptz,
    cancelled_at     timestamptz
);

CREATE INDEX orders_customer_id_idx ON orders (customer_id, id DESC);
CREATE INDEX orders_subscription_pending_idx ON orders (subscription_id) WHERE status = 'pending';
"##,
    ),
    (
        "migrations/0005_create_payments.sql",
        r##"
CREATE TABLE payments (
    id              uuid        PRIMARY KEY,
    order_id        uuid        NOT NULL,
    subscription_id uuid,
    customer_id     uuid        NOT NULL,
    amount_minor    bigint      NOT NULL CHECK (amount_minor >= 0),
    currency        char(3)     NOT NULL,
    status          varchar(16) NOT NULL,
    psp_reference   varchar(64),
    captured_at     timestamptz
);

CREATE INDEX payments_order_id_idx ON payments (order_id);
"##,
    ),
    (
        "migrations/0006_create_idempotency_keys.sql",
        r##"
-- One row per Idempotency-Key seen by the ledger's capture endpoint. The
-- primary key makes reserving a key atomic; the stored response is replayed
-- for retries of the same request (ADR-0004). Rows expire after 24 hours
-- (IDEMPOTENCY_TTL_HOURS) and are purged by the nightly cleanup job.
CREATE TABLE idempotency_keys (
    key             varchar(128) PRIMARY KEY,
    fingerprint     char(64)     NOT NULL,
    response_status smallint,
    response_body   jsonb,
    created_at      timestamptz  NOT NULL DEFAULT now(),
    expires_at      timestamptz  NOT NULL
);

CREATE INDEX idempotency_keys_expires_at_idx ON idempotency_keys (expires_at);
"##,
    ),
    (
        "migrations/0007_create_refunds.sql",
        r##"
CREATE TABLE refunds (
    id            uuid        PRIMARY KEY,
    payment_id    uuid        NOT NULL REFERENCES payments (id),
    customer_id   uuid        NOT NULL,
    amount_minor  bigint      NOT NULL CHECK (amount_minor > 0),
    reason        varchar(64) NOT NULL,
    psp_reference varchar(64),
    created_at    timestamptz NOT NULL DEFAULT now()
);
"##,
    ),
    (
        "migrations/0008_add_cancelled_at_to_subscriptions.sql",
        r##"
ALTER TABLE subscriptions ADD COLUMN cancelled_at timestamptz;

CREATE INDEX subscriptions_cancelled_period_end_idx
    ON subscriptions (current_period_end)
    WHERE status = 'cancelled';
"##,
    ),
    (
        "migrations/0009_add_cancel_reason_to_subscriptions.sql",
        r##"
-- Why members leave, collected by the cancellation dialog on web and mobile.
ALTER TABLE subscriptions
    ADD COLUMN cancel_reason   varchar(32),
    ADD COLUMN cancel_feedback text;

ALTER TABLE subscriptions
    ADD CONSTRAINT subscriptions_cancel_reason_check
    CHECK (cancel_reason IS NULL OR cancel_reason IN ('too_expensive', 'not_using', 'switching_provider', 'other'));
"##,
    ),
    (
        "migrations/0010_create_notification_log.sql",
        r##"
CREATE TABLE notification_log (
    id          bigserial    PRIMARY KEY,
    event_id    uuid         NOT NULL UNIQUE,
    template    varchar(64)  NOT NULL,
    recipient   varchar(320) NOT NULL,
    sent_at     timestamptz  NOT NULL DEFAULT now()
);
"##,
    ),
];
