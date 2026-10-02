//! `billing-api`: plans, subscriptions and checkout (TypeScript / NestJS).

pub(super) const FILES: &[(&str, &str)] = &[
    (
        "README.md",
        r##"
# billing-api

Owns plans and Acme Plus subscriptions, and orchestrates checkout. NestJS on
Node 20, TypeORM on the shared Postgres (this service owns the `plans` and
`subscriptions` tables, see handbook ADR-0003).

## Endpoints

| Method | Path | Notes |
|---|---|---|
| GET | `/v1/plans` | public |
| GET | `/v1/subscriptions/:id` | owner only |
| POST | `/v1/subscriptions/:id/cancel` | owner only, cancels at period end |
| POST | `/v1/subscriptions/:id/resume` | owner only |
| POST | `/v1/checkout` | requires `Idempotency-Key` |

The contract is published in `contracts/openapi/billing-api.yaml`.

## Events

Publishes `subscription.cancelled` and `subscription.resumed` to Kafka
(`src/events/`). Consumers: notification-worker, orders-service.

## Configuration

`PORT`, `DATABASE_URL`, `KAFKA_BROKERS`, `JWT_PUBLIC_KEY`, `LEDGER_API_URL`,
`ORDERS_API_URL`.
"##,
    ),
    (
        "package.json",
        r##"
{
  "name": "@acme/billing-api",
  "version": "5.2.1",
  "private": true,
  "scripts": {
    "build": "nest build",
    "start": "node dist/main.js",
    "test": "jest"
  },
  "dependencies": {
    "@nestjs/common": "10.3.8",
    "@nestjs/core": "10.3.8",
    "@nestjs/platform-express": "10.3.8",
    "@nestjs/typeorm": "10.0.2",
    "class-validator": "0.14.1",
    "jose": "5.3.0",
    "kafkajs": "2.2.4",
    "pg": "8.11.5",
    "typeorm": "0.3.20"
  },
  "devDependencies": {
    "@nestjs/cli": "10.3.2",
    "jest": "29.7.0",
    "typescript": "5.4.5"
  }
}
"##,
    ),
    (
        "src/main.ts",
        r##"
import { ValidationPipe } from "@nestjs/common";
import { NestFactory } from "@nestjs/core";
import { AppModule } from "./app.module";

async function bootstrap() {
  const app = await NestFactory.create(AppModule);
  app.useGlobalPipes(new ValidationPipe({ whitelist: true, forbidNonWhitelisted: true }));
  app.enableShutdownHooks();
  await app.listen(Number(process.env.PORT ?? 8080));
}

void bootstrap();
"##,
    ),
    (
        "src/app.module.ts",
        r##"
import { Module } from "@nestjs/common";
import { TypeOrmModule } from "@nestjs/typeorm";
import { CheckoutController } from "./checkout/checkout.controller";
import { CheckoutService } from "./checkout/checkout.service";
import { EventPublisher } from "./events/event-publisher";
import { OrdersClient } from "./orders/orders.client";
import { LedgerClient } from "./payments/ledger.client";
import { PlanEntity } from "./subscriptions/plan.entity";
import { PlansController } from "./subscriptions/plans.controller";
import { SubscriptionEntity } from "./subscriptions/subscription.entity";
import { SubscriptionService } from "./subscriptions/subscription.service";
import { SubscriptionsController } from "./subscriptions/subscriptions.controller";

@Module({
  imports: [
    TypeOrmModule.forRoot({
      type: "postgres",
      url: process.env.DATABASE_URL,
      entities: [SubscriptionEntity, PlanEntity],
      // Schema changes go through db-migrations, never through the ORM.
      synchronize: false,
    }),
    TypeOrmModule.forFeature([SubscriptionEntity, PlanEntity]),
  ],
  controllers: [SubscriptionsController, PlansController, CheckoutController],
  providers: [SubscriptionService, CheckoutService, EventPublisher, LedgerClient, OrdersClient],
})
export class AppModule {}
"##,
    ),
    (
        "src/subscriptions/subscriptions.controller.ts",
        r##"
import { Body, Controller, Get, HttpCode, Param, Post, UseGuards } from "@nestjs/common";
import { CurrentCustomer, type AuthenticatedCustomer } from "../auth/current-customer.decorator";
import { JwtAuthGuard } from "../auth/jwt-auth.guard";
import { SubscriptionOwnerGuard } from "../auth/subscription-owner.guard";
import { CancelSubscriptionDto } from "./dto/cancel-subscription.dto";
import { SubscriptionService } from "./subscription.service";

@Controller("v1/subscriptions")
@UseGuards(JwtAuthGuard, SubscriptionOwnerGuard)
export class SubscriptionsController {
  constructor(private readonly subscriptionService: SubscriptionService) {}

  @Get(":id")
  get(@Param("id") id: string, @CurrentCustomer() customer: AuthenticatedCustomer) {
    return this.subscriptionService.findForCustomer(id, customer.id);
  }

  @Post(":id/cancel")
  @HttpCode(200)
  cancel(
    @Param("id") id: string,
    @Body() dto: CancelSubscriptionDto,
    @CurrentCustomer() customer: AuthenticatedCustomer,
  ) {
    return this.subscriptionService.cancelSubscription(id, customer, dto.reason, dto.feedback);
  }

  @Post(":id/resume")
  @HttpCode(200)
  resume(@Param("id") id: string, @CurrentCustomer() customer: AuthenticatedCustomer) {
    return this.subscriptionService.resumeSubscription(id, customer.id);
  }
}
"##,
    ),
    (
        "src/subscriptions/plans.controller.ts",
        r##"
import { Controller, Get } from "@nestjs/common";
import { InjectRepository } from "@nestjs/typeorm";
import { Repository } from "typeorm";
import { PlanEntity } from "./plan.entity";

@Controller("v1/plans")
export class PlansController {
  constructor(@InjectRepository(PlanEntity) private readonly plans: Repository<PlanEntity>) {}

  @Get()
  list() {
    return this.plans.find({ where: { active: true }, order: { priceMinor: "ASC" } });
  }
}
"##,
    ),
    (
        "src/subscriptions/subscription.service.ts",
        r##"
import { ConflictException, Injectable, NotFoundException } from "@nestjs/common";
import { InjectRepository } from "@nestjs/typeorm";
import { LessThan, Repository } from "typeorm";
import type { AuthenticatedCustomer } from "../auth/current-customer.decorator";
import { EventPublisher } from "../events/event-publisher";
import {
  SUBSCRIPTION_CANCELLED,
  SUBSCRIPTION_RESUMED,
  type SubscriptionCancelledEvent,
  type SubscriptionResumedEvent,
} from "../events/subscription-events";
import { LedgerClient } from "../payments/ledger.client";
import type { CancelReason } from "./dto/cancel-subscription.dto";
import { SubscriptionEntity } from "./subscription.entity";

@Injectable()
export class SubscriptionService {
  constructor(
    @InjectRepository(SubscriptionEntity) private readonly subscriptions: Repository<SubscriptionEntity>,
    private readonly events: EventPublisher,
    private readonly ledger: LedgerClient,
  ) {}

  async findForCustomer(subscriptionId: string, customerId: string): Promise<SubscriptionEntity> {
    const subscription = await this.subscriptions.findOne({ where: { id: subscriptionId, customerId } });
    if (!subscription) {
      throw new NotFoundException("subscription not found");
    }
    return subscription;
  }

  /**
   * Cancels at period end (ADR-0005): the row is marked cancelled now, but
   * benefits continue until `currentPeriodEnd`. Members on a yearly plan get
   * the unused full months back through the ledger.
   */
  async cancelSubscription(
    subscriptionId: string,
    customer: AuthenticatedCustomer,
    reason: CancelReason,
    feedback?: string,
  ): Promise<SubscriptionEntity> {
    const subscription = await this.findForCustomer(subscriptionId, customer.id);
    if (subscription.status === "cancelled") {
      return subscription; // cancelling twice is a no-op, not an error
    }
    if (subscription.status === "expired") {
      throw new ConflictException("subscription has already expired");
    }

    const cancelledAt = new Date();
    subscription.status = "cancelled";
    subscription.cancelledAt = cancelledAt;
    subscription.cancelReason = reason;
    subscription.cancelFeedback = feedback ?? null;
    await this.subscriptions.save(subscription);

    if (subscription.interval === "year") {
      await this.ledger.refundProrated({
        subscriptionId: subscription.id,
        customerId: customer.id,
        periodStart: subscription.currentPeriodStart.toISOString(),
        periodEnd: subscription.currentPeriodEnd.toISOString(),
        cancelledAt: cancelledAt.toISOString(),
      });
    }

    const event: SubscriptionCancelledEvent = {
      subscriptionId: subscription.id,
      customerId: customer.id,
      customerEmail: customer.email,
      locale: customer.locale,
      planId: subscription.planId,
      reason,
      cancelledAt: cancelledAt.toISOString(),
      accessUntil: subscription.currentPeriodEnd.toISOString(),
    };
    await this.events.publish(SUBSCRIPTION_CANCELLED, event);
    return subscription;
  }

  /** Reverts a cancellation as long as the paid period has not ended. */
  async resumeSubscription(subscriptionId: string, customerId: string): Promise<SubscriptionEntity> {
    const subscription = await this.findForCustomer(subscriptionId, customerId);
    if (subscription.status !== "cancelled" || subscription.currentPeriodEnd <= new Date()) {
      throw new ConflictException("only a cancelled subscription inside its paid period can be resumed");
    }
    subscription.status = "active";
    subscription.cancelledAt = null;
    subscription.cancelReason = null;
    subscription.cancelFeedback = null;
    await this.subscriptions.save(subscription);
    const event: SubscriptionResumedEvent = { subscriptionId, customerId, resumedAt: new Date().toISOString() };
    await this.events.publish(SUBSCRIPTION_RESUMED, event);
    return subscription;
  }

  /** Nightly job: cancelled subscriptions whose paid period is over become expired. */
  async expireEndedSubscriptions(now: Date = new Date()): Promise<number> {
    const ended = await this.subscriptions.find({
      where: { status: "cancelled", currentPeriodEnd: LessThan(now) },
    });
    for (const subscription of ended) {
      subscription.status = "expired";
    }
    await this.subscriptions.save(ended);
    return ended.length;
  }
}
"##,
    ),
    (
        "src/subscriptions/dto/cancel-subscription.dto.ts",
        r##"
import { IsIn, IsOptional, IsString, MaxLength } from "class-validator";

export const CANCEL_REASONS = ["too_expensive", "not_using", "switching_provider", "other"] as const;

export type CancelReason = (typeof CANCEL_REASONS)[number];

/** Body of `POST /v1/subscriptions/:id/cancel`. */
export class CancelSubscriptionDto {
  @IsIn(CANCEL_REASONS)
  reason!: CancelReason;

  @IsOptional()
  @IsString()
  @MaxLength(2000)
  feedback?: string;
}
"##,
    ),
    (
        "src/subscriptions/subscription.entity.ts",
        r##"
import { Column, CreateDateColumn, Entity, PrimaryColumn, UpdateDateColumn } from "typeorm";

export type SubscriptionStatus = "active" | "past_due" | "cancelled" | "expired";

/** Row of the `subscriptions` table (owned by billing-api). */
@Entity({ name: "subscriptions" })
export class SubscriptionEntity {
  @PrimaryColumn("uuid")
  id!: string;

  @Column({ name: "customer_id", type: "uuid" })
  customerId!: string;

  @Column({ name: "plan_id", type: "varchar", length: 64 })
  planId!: string;

  @Column({ type: "varchar", length: 16 })
  status!: SubscriptionStatus;

  @Column({ name: "interval", type: "varchar", length: 8 })
  interval!: "month" | "year";

  @Column({ name: "current_period_start", type: "timestamptz" })
  currentPeriodStart!: Date;

  @Column({ name: "current_period_end", type: "timestamptz" })
  currentPeriodEnd!: Date;

  @Column({ name: "cancelled_at", type: "timestamptz", nullable: true })
  cancelledAt!: Date | null;

  // Added by db-migrations 0009_add_cancel_reason_to_subscriptions.sql.
  @Column({ name: "cancel_reason", type: "varchar", length: 32, nullable: true })
  cancelReason!: string | null;

  @Column({ name: "cancel_feedback", type: "text", nullable: true })
  cancelFeedback!: string | null;

  @CreateDateColumn({ name: "created_at", type: "timestamptz" })
  createdAt!: Date;

  @UpdateDateColumn({ name: "updated_at", type: "timestamptz" })
  updatedAt!: Date;
}
"##,
    ),
    (
        "src/subscriptions/plan.entity.ts",
        r##"
import { Column, Entity, PrimaryColumn } from "typeorm";

@Entity({ name: "plans" })
export class PlanEntity {
  @PrimaryColumn({ type: "varchar", length: 64 })
  id!: string;

  @Column({ type: "varchar", length: 120 })
  name!: string;

  @Column({ name: "price_minor", type: "integer" })
  priceMinor!: number;

  @Column({ type: "char", length: 3 })
  currency!: string;

  @Column({ name: "interval", type: "varchar", length: 8 })
  interval!: "month" | "year";

  @Column({ type: "boolean", default: true })
  active!: boolean;
}
"##,
    ),
    (
        "src/auth/jwt-auth.guard.ts",
        r##"
import { type CanActivate, type ExecutionContext, Injectable, UnauthorizedException } from "@nestjs/common";
import { createPublicKey, type KeyObject } from "node:crypto";
import { jwtVerify } from "jose";

/**
 * Verifies the `Authorization: Bearer <jwt>` header issued by the identity
 * provider. The public key comes from JWT_PUBLIC_KEY (PEM); tokens must be
 * RS256-signed, unexpired and carry the `acme-api` audience. The verified
 * customer is attached to the request for `@CurrentCustomer()`.
 */
@Injectable()
export class JwtAuthGuard implements CanActivate {
  private readonly key: KeyObject = createPublicKey(process.env.JWT_PUBLIC_KEY ?? "");

  async canActivate(context: ExecutionContext): Promise<boolean> {
    const request = context.switchToHttp().getRequest();
    const header: string | undefined = request.headers["authorization"];
    if (!header?.startsWith("Bearer ")) {
      throw new UnauthorizedException("missing bearer token");
    }
    try {
      const { payload } = await jwtVerify(header.slice("Bearer ".length), this.key, {
        audience: "acme-api",
        algorithms: ["RS256"],
      });
      request.customer = {
        id: String(payload.sub),
        email: String(payload.email ?? ""),
        locale: String(payload.locale ?? "en"),
      };
      return true;
    } catch {
      throw new UnauthorizedException("invalid or expired token");
    }
  }
}
"##,
    ),
    (
        "src/auth/subscription-owner.guard.ts",
        r##"
import { type CanActivate, type ExecutionContext, Injectable, NotFoundException } from "@nestjs/common";
import { InjectRepository } from "@nestjs/typeorm";
import { Repository } from "typeorm";
import { SubscriptionEntity } from "../subscriptions/subscription.entity";

/**
 * Lets a request through only when `:id` belongs to the signed-in customer.
 * Foreign ids answer 404 rather than 403 so ids cannot be probed.
 * Must run after JwtAuthGuard.
 */
@Injectable()
export class SubscriptionOwnerGuard implements CanActivate {
  constructor(@InjectRepository(SubscriptionEntity) private readonly subscriptions: Repository<SubscriptionEntity>) {}

  async canActivate(context: ExecutionContext): Promise<boolean> {
    const request = context.switchToHttp().getRequest();
    const owned = await this.subscriptions.exists({
      where: { id: request.params.id, customerId: request.customer?.id },
    });
    if (!owned) {
      throw new NotFoundException("subscription not found");
    }
    return true;
  }
}
"##,
    ),
    (
        "src/auth/current-customer.decorator.ts",
        r##"
import { createParamDecorator, type ExecutionContext } from "@nestjs/common";

export interface AuthenticatedCustomer {
  id: string;
  email: string;
  locale: string;
}

/** The customer verified by JwtAuthGuard. */
export const CurrentCustomer = createParamDecorator(
  (_: unknown, context: ExecutionContext): AuthenticatedCustomer => context.switchToHttp().getRequest().customer,
);
"##,
    ),
    (
        "src/events/event-publisher.ts",
        r##"
import { Injectable, type OnModuleDestroy } from "@nestjs/common";
import { randomUUID } from "node:crypto";
import { Kafka, type Producer } from "kafkajs";

export interface EventEnvelope<T> {
  id: string;
  type: string;
  version: number;
  occurredAt: string;
  data: T;
}

/**
 * Publishes domain events to Kafka. The topic name equals the event type
 * (`<entity>.<past-tense verb>`, ADR-0002); the envelope id lets consumers
 * drop redelivered messages.
 */
@Injectable()
export class EventPublisher implements OnModuleDestroy {
  private readonly producer: Producer;
  private connected = false;

  constructor() {
    const brokers = (process.env.KAFKA_BROKERS ?? "").split(",").filter(Boolean);
    this.producer = new Kafka({ clientId: "billing-api", brokers }).producer({ idempotent: true });
  }

  async publish<T>(type: string, data: T, version = 1): Promise<void> {
    if (!this.connected) {
      await this.producer.connect();
      this.connected = true;
    }
    const envelope: EventEnvelope<T> = {
      id: randomUUID(),
      type,
      version,
      occurredAt: new Date().toISOString(),
      data,
    };
    await this.producer.send({ topic: type, messages: [{ key: envelope.id, value: JSON.stringify(envelope) }] });
  }

  async onModuleDestroy(): Promise<void> {
    if (this.connected) {
      await this.producer.disconnect();
    }
  }
}
"##,
    ),
    (
        "src/events/subscription-events.ts",
        r##"
// Event types published by billing-api. JSON schemas live in the contracts
// repository: contracts/events/subscription.cancelled.v1.json.

export const SUBSCRIPTION_CANCELLED = "subscription.cancelled";
export const SUBSCRIPTION_RESUMED = "subscription.resumed";

/** Payload of `subscription.cancelled` v1. */
export interface SubscriptionCancelledEvent {
  subscriptionId: string;
  customerId: string;
  customerEmail: string;
  locale: string;
  planId: string;
  reason: string;
  cancelledAt: string;
  /** End of the paid period; benefits stay active until then. */
  accessUntil: string;
}

/** Payload of `subscription.resumed` v1. */
export interface SubscriptionResumedEvent {
  subscriptionId: string;
  customerId: string;
  resumedAt: string;
}
"##,
    ),
    (
        "src/checkout/checkout.controller.ts",
        r##"
import { BadRequestException, Body, Controller, Headers, Post, UseGuards } from "@nestjs/common";
import { CurrentCustomer, type AuthenticatedCustomer } from "../auth/current-customer.decorator";
import { JwtAuthGuard } from "../auth/jwt-auth.guard";
import { CheckoutService, type CheckoutDto } from "./checkout.service";

@Controller("v1/checkout")
@UseGuards(JwtAuthGuard)
export class CheckoutController {
  constructor(private readonly checkoutService: CheckoutService) {}

  @Post()
  checkout(
    @Body() dto: CheckoutDto,
    @Headers("idempotency-key") idempotencyKey: string | undefined,
    @CurrentCustomer() customer: AuthenticatedCustomer,
  ) {
    if (!idempotencyKey) {
      throw new BadRequestException("Idempotency-Key header is required");
    }
    return this.checkoutService.placeOrder(customer.id, dto, idempotencyKey);
  }
}
"##,
    ),
    (
        "src/checkout/checkout.service.ts",
        r##"
import { Injectable } from "@nestjs/common";
import { OrdersClient } from "../orders/orders.client";
import { LedgerClient } from "../payments/ledger.client";

export interface CheckoutDto {
  lines: { sku: string; quantity: number }[];
  planId?: string;
  shippingAddressId: string;
  paymentMethodId: string;
}

export interface CheckoutResult {
  orderId: string;
  paymentId: string;
  status: "paid" | "requires_action";
}

/**
 * Checkout = create the order in orders-service, then capture the payment in
 * ledger-service. The client's Idempotency-Key is forwarded to the ledger,
 * which turns a retried checkout into a replay of the first capture.
 */
@Injectable()
export class CheckoutService {
  constructor(
    private readonly orders: OrdersClient,
    private readonly ledger: LedgerClient,
  ) {}

  async placeOrder(customerId: string, dto: CheckoutDto, idempotencyKey: string): Promise<CheckoutResult> {
    const order = await this.orders.createOrder({
      customerId,
      lines: dto.lines,
      shippingAddressId: dto.shippingAddressId,
      clientReference: idempotencyKey,
    });
    const payment = await this.ledger.capture(
      {
        orderId: order.id,
        customerId,
        amountMinor: order.totalMinor,
        currency: order.currency,
        paymentMethodId: dto.paymentMethodId,
      },
      idempotencyKey,
    );
    return {
      orderId: order.id,
      paymentId: payment.paymentId,
      status: payment.status === "captured" ? "paid" : "requires_action",
    };
  }
}
"##,
    ),
    (
        "src/payments/ledger.client.ts",
        r##"
import { Injectable } from "@nestjs/common";

const LEDGER_API_URL = process.env.LEDGER_API_URL ?? "http://ledger-service:8000";

export interface CaptureRequest {
  orderId: string;
  customerId: string;
  amountMinor: number;
  currency: string;
  paymentMethodId: string;
}

export interface CaptureResult {
  paymentId: string;
  status: "captured" | "requires_action" | "failed";
}

export interface ProratedRefundRequest {
  subscriptionId: string;
  customerId: string;
  periodStart: string;
  periodEnd: string;
  cancelledAt: string;
}

/** HTTP client for ledger-service (contracts/openapi/ledger-api.yaml). */
@Injectable()
export class LedgerClient {
  async capture(request: CaptureRequest, idempotencyKey: string): Promise<CaptureResult> {
    return this.post<CaptureResult>("/v1/payments/capture", request, `checkout:${idempotencyKey}`);
  }

  /** Called when a yearly membership is cancelled; the key makes repeats harmless. */
  async refundProrated(request: ProratedRefundRequest): Promise<void> {
    await this.post("/v1/refunds/prorated", request, `cancel:${request.subscriptionId}`);
  }

  private async post<T>(path: string, body: unknown, idempotencyKey: string): Promise<T> {
    const response = await fetch(`${LEDGER_API_URL}${path}`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "Idempotency-Key": idempotencyKey },
      body: JSON.stringify(body),
    });
    if (!response.ok) {
      throw new Error(`ledger ${path} failed with ${response.status}`);
    }
    return (await response.json()) as T;
  }
}
"##,
    ),
    (
        "src/orders/orders.client.ts",
        r##"
import { Injectable } from "@nestjs/common";

const ORDERS_API_URL = process.env.ORDERS_API_URL ?? "http://orders-service:8081";

export interface CreateOrderRequest {
  customerId: string;
  lines: { sku: string; quantity: number }[];
  shippingAddressId: string;
  clientReference: string;
}

export interface CreatedOrder {
  id: string;
  totalMinor: number;
  currency: string;
}

/** Calls the internal `POST /v1/orders` endpoint of orders-service. */
@Injectable()
export class OrdersClient {
  async createOrder(request: CreateOrderRequest): Promise<CreatedOrder> {
    const response = await fetch(`${ORDERS_API_URL}/v1/orders`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Customer-Id": request.customerId },
      body: JSON.stringify(request),
    });
    if (!response.ok) {
      throw new Error(`orders-service returned ${response.status}`);
    }
    return (await response.json()) as CreatedOrder;
  }
}
"##,
    ),
];
