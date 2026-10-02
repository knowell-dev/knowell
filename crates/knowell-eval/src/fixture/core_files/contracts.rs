//! `contracts`: protobuf, OpenAPI and event schemas shared by all services.
//!
//! `openapi/billing-api.yaml` deliberately omits
//! `POST /v1/subscriptions/{subscriptionId}/resume`, which billing-api serves
//! and the website calls (planted contract drift).

pub(super) const FILES: &[(&str, &str)] = &[
    (
        "README.md",
        r##"
# contracts

Interfaces between Acme Goods services. Code is generated from this
repository for Go (`gen/go`), Python (`acme_contracts`) and TypeScript.

| Folder | What |
|---|---|
| `proto/` | gRPC services (internal, read-only APIs) |
| `openapi/` | REST APIs consumed across teams |
| `events/` | JSON Schemas of Kafka event payloads |

## Event conventions

- The topic name is the event type: `<entity>.<past-tense verb>`, lowercase,
  e.g. `subscription.cancelled`, `payment.captured` (ADR-0002).
- Every message is an envelope `{id, type, version, occurredAt, data}`;
  the schema files describe `data`.
- Additive changes only within a version; breaking changes get a new
  `*.v2.json` and both versions are published during the migration.
"##,
    ),
    (
        "proto/common/v1/money.proto",
        r##"
syntax = "proto3";

package acme.common.v1;

option go_package = "example.com/acme/contracts/gen/go/common/v1;commonv1";

// An amount of money in minor units (cents, kuruş). Never use floating point.
message Money {
  int64 amount_minor = 1;
  // ISO 4217 code, e.g. "EUR" or "TRY".
  string currency = 2;
}
"##,
    ),
    (
        "proto/ledger/v1/ledger.proto",
        r##"
syntax = "proto3";

package acme.ledger.v1;

option go_package = "example.com/acme/contracts/gen/go/ledger/v1;ledgerv1";

import "common/v1/money.proto";

// Internal read API of ledger-service. Writes go through the REST API
// (openapi/ledger-api.yaml) because they need Idempotency-Key semantics.
service LedgerService {
  // Authoritative status of one payment.
  rpc GetPaymentStatus(GetPaymentStatusRequest) returns (GetPaymentStatusResponse);
  rpc ListPaymentsForOrder(ListPaymentsForOrderRequest) returns (ListPaymentsForOrderResponse);
}

enum PaymentStatus {
  PAYMENT_STATUS_UNSPECIFIED = 0;
  PAYMENT_STATUS_PENDING = 1;
  PAYMENT_STATUS_CAPTURED = 2;
  PAYMENT_STATUS_FAILED = 3;
  PAYMENT_STATUS_REFUNDED = 4;
}

message GetPaymentStatusRequest {
  string payment_id = 1;
}

message GetPaymentStatusResponse {
  string payment_id = 1;
  PaymentStatus status = 2;
  acme.common.v1.Money amount = 3;
}

message ListPaymentsForOrderRequest {
  string order_id = 1;
}

message ListPaymentsForOrderResponse {
  repeated string payment_ids = 1;
}
"##,
    ),
    (
        "openapi/billing-api.yaml",
        r##"
openapi: 3.0.3
info:
  title: Acme billing API
  version: 5.2.0
servers:
  - url: https://api.example.com
paths:
  /v1/plans:
    get:
      operationId: listPlans
      responses:
        "200":
          description: Active plans, cheapest first.
          content:
            application/json:
              schema:
                type: array
                items:
                  $ref: "#/components/schemas/Plan"
  /v1/subscriptions/{subscriptionId}:
    get:
      operationId: getSubscription
      security:
        - bearerAuth: []
      parameters:
        - $ref: "#/components/parameters/SubscriptionId"
      responses:
        "200":
          description: The subscription.
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Subscription"
        "404":
          description: Unknown id or not owned by the caller.
  /v1/subscriptions/{subscriptionId}/cancel:
    post:
      operationId: cancelSubscription
      description: >
        Cancels at the end of the paid period. Benefits stay active until
        currentPeriodEnd; yearly plans receive a prorated refund.
      security:
        - bearerAuth: []
      parameters:
        - $ref: "#/components/parameters/SubscriptionId"
      requestBody:
        required: true
        content:
          application/json:
            schema:
              $ref: "#/components/schemas/CancelSubscriptionRequest"
      responses:
        "200":
          description: The cancelled subscription.
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Subscription"
        "409":
          description: The subscription has already expired.
  /v1/checkout:
    post:
      operationId: checkout
      security:
        - bearerAuth: []
      parameters:
        - name: Idempotency-Key
          in: header
          required: true
          description: One value per checkout attempt; retries reuse it.
          schema:
            type: string
            minLength: 8
            maxLength: 128
      requestBody:
        required: true
        content:
          application/json:
            schema:
              $ref: "#/components/schemas/CheckoutRequest"
      responses:
        "200":
          description: Order created and payment captured (or requires action).
components:
  securitySchemes:
    bearerAuth:
      type: http
      scheme: bearer
      bearerFormat: JWT
  parameters:
    SubscriptionId:
      name: subscriptionId
      in: path
      required: true
      schema:
        type: string
        format: uuid
  schemas:
    Plan:
      type: object
      required: [id, name, priceMinor, currency, interval]
      properties:
        id: { type: string }
        name: { type: string }
        priceMinor: { type: integer }
        currency: { type: string }
        interval: { type: string, enum: [month, year] }
    Subscription:
      type: object
      required: [id, planId, status, currentPeriodEnd]
      properties:
        id: { type: string, format: uuid }
        planId: { type: string }
        status: { type: string, enum: [active, past_due, cancelled, expired] }
        currentPeriodEnd: { type: string, format: date-time }
        cancelledAt: { type: string, format: date-time, nullable: true }
        cancelReason: { type: string, nullable: true }
    CancelSubscriptionRequest:
      type: object
      required: [reason]
      properties:
        reason: { type: string, enum: [too_expensive, not_using, switching_provider, other] }
        feedback: { type: string, maxLength: 2000 }
    CheckoutRequest:
      type: object
      required: [lines, shippingAddressId, paymentMethodId]
      properties:
        lines:
          type: array
          items:
            type: object
            required: [sku, quantity]
            properties:
              sku: { type: string }
              quantity: { type: integer, minimum: 1 }
        planId: { type: string }
        shippingAddressId: { type: string }
        paymentMethodId: { type: string }
"##,
    ),
    (
        "openapi/ledger-api.yaml",
        r##"
openapi: 3.0.3
info:
  title: Acme ledger API (internal)
  version: 1.8.0
paths:
  /v1/payments/capture:
    post:
      operationId: capturePayment
      description: >
        Captures a card payment. Requests with an Idempotency-Key that was seen
        before replay the stored response (same body) or fail with 422
        (different body); 409 means the first request is still running.
      parameters:
        - $ref: "#/components/parameters/IdempotencyKey"
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [order_id, customer_id, amount_minor, currency, payment_method_id]
              properties:
                order_id: { type: string }
                customer_id: { type: string }
                amount_minor: { type: integer }
                currency: { type: string }
                payment_method_id: { type: string }
      responses:
        "201":
          description: Captured, or the replayed earlier result.
        "409":
          description: A request with this key is still in progress.
        "422":
          description: The key was used with a different request body.
  /v1/payments/{paymentId}:
    get:
      operationId: getPayment
      parameters:
        - name: paymentId
          in: path
          required: true
          schema: { type: string }
      responses:
        "200":
          description: The payment.
  /v1/refunds:
    post:
      operationId: createRefund
      parameters:
        - $ref: "#/components/parameters/IdempotencyKey"
      responses:
        "201":
          description: Refund issued.
  /v1/refunds/prorated:
    post:
      operationId: refundProrated
      description: Refund of unused whole months after a yearly membership is cancelled.
      responses:
        "201":
          description: Refund issued (refundId is null when nothing is owed).
components:
  parameters:
    IdempotencyKey:
      name: Idempotency-Key
      in: header
      required: true
      schema:
        type: string
        minLength: 8
        maxLength: 128
"##,
    ),
    (
        "events/subscription.cancelled.v1.json",
        r##"
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://contracts.example.com/events/subscription.cancelled.v1.json",
  "title": "subscription.cancelled",
  "description": "Published by billing-api when a member cancels. Consumed by notification-worker and orders-service.",
  "type": "object",
  "required": ["subscriptionId", "customerId", "customerEmail", "locale", "planId", "reason", "cancelledAt", "accessUntil"],
  "properties": {
    "subscriptionId": { "type": "string", "format": "uuid" },
    "customerId": { "type": "string", "format": "uuid" },
    "customerEmail": { "type": "string", "format": "email" },
    "locale": { "type": "string", "enum": ["en", "tr"] },
    "planId": { "type": "string" },
    "reason": { "type": "string", "enum": ["too_expensive", "not_using", "switching_provider", "other"] },
    "cancelledAt": { "type": "string", "format": "date-time" },
    "accessUntil": { "type": "string", "format": "date-time", "description": "End of the paid period." }
  },
  "additionalProperties": false
}
"##,
    ),
    (
        "events/payment.captured.v1.json",
        r##"
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://contracts.example.com/events/payment.captured.v1.json",
  "title": "payment.captured",
  "description": "Published by ledger-service after the provider confirmed a capture. Consumed by orders-service and notification-worker.",
  "type": "object",
  "required": ["paymentId", "orderId", "customerId", "amountMinor", "currency", "capturedAt"],
  "properties": {
    "paymentId": { "type": "string" },
    "orderId": { "type": "string" },
    "customerId": { "type": "string" },
    "amountMinor": { "type": "integer", "minimum": 0 },
    "currency": { "type": "string", "minLength": 3, "maxLength": 3 },
    "capturedAt": { "type": "string", "format": "date-time" },
    "customerEmail": { "type": "string", "format": "email" }
  }
}
"##,
    ),
];
