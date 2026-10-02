//! `ledger-service`: payments, refunds and the money ledger (Python / FastAPI).
//!
//! `ledger/db/models.py` deliberately maps the `subscriptions` table without
//! the `cancel_reason` / `cancel_feedback` columns added by migration 0009
//! (planted schema drift).

pub(super) const FILES: &[(&str, &str)] = &[
    (
        "README.md",
        r##"
# ledger-service

The source of truth for money at Acme Goods: captures card payments through
the payment service provider (PSP), issues refunds and records every movement.

- REST (FastAPI): `POST /v1/payments/capture`, `GET /v1/payments/{id}`,
  `POST /v1/refunds`, `POST /v1/refunds/prorated`; see
  `contracts/openapi/ledger-api.yaml`.
- gRPC: `acme.ledger.v1.LedgerService` for internal reads (orders-service).
- Publishes `payment.captured`.

Capture requests must carry an `Idempotency-Key`; see
`ledger/payments/idempotency.py` and handbook ADR-0004.

## Configuration

`DATABASE_URL`, `KAFKA_BROKERS`, `PSP_API_URL`, `PSP_API_KEY`,
`PSP_TIMEOUT_SECONDS`, `IDEMPOTENCY_TTL_HOURS`, `GRPC_PORT`.
"##,
    ),
    (
        "pyproject.toml",
        r##"
[project]
name = "acme-ledger"
version = "1.8.2"
requires-python = ">=3.11"
dependencies = [
    "aiokafka==0.10.0",
    "fastapi==0.111.0",
    "grpcio==1.64.0",
    "httpx==0.27.0",
    "pydantic-settings==2.2.1",
    "sqlalchemy==2.0.30",
]

[project.optional-dependencies]
dev = ["pytest==8.2.1"]
"##,
    ),
    (
        "ledger/main.py",
        r##"
from contextlib import asynccontextmanager

from fastapi import FastAPI

from ledger.api import payments, refunds
from ledger.grpc_server import serve as serve_grpc
from ledger.settings import get_settings


@asynccontextmanager
async def lifespan(app: FastAPI):
    grpc_server = serve_grpc(get_settings().grpc_port)
    yield
    grpc_server.stop(grace=5)


app = FastAPI(title="ledger-service", lifespan=lifespan)
app.include_router(payments.router)
app.include_router(refunds.router)
"##,
    ),
    (
        "ledger/settings.py",
        r##"
from functools import lru_cache

from pydantic_settings import BaseSettings, SettingsConfigDict


class Settings(BaseSettings):
    """Runtime configuration, read from environment variables.

    PSP_API_KEY is injected from the cluster secret store (see
    infra/k8s/ledger-service.yaml); it never appears in files.
    """

    model_config = SettingsConfigDict(case_sensitive=False)

    database_url: str
    kafka_brokers: str = "kafka:9092"
    psp_api_url: str = "https://psp.example.com"
    psp_api_key: str
    # Give up on the provider after this many seconds; the client retries
    # with the same Idempotency-Key.
    psp_timeout_seconds: float = 8.0
    idempotency_ttl_hours: int = 24
    grpc_port: int = 50051


@lru_cache
def get_settings() -> Settings:
    return Settings()
"##,
    ),
    (
        "ledger/api/payments.py",
        r##"
from fastapi import APIRouter, Depends, Header, HTTPException

from ledger.db.session import get_session
from ledger.events import EventPublisher, get_publisher
from ledger.payments.capture import CaptureRequest, CaptureResult, capture_payment, get_payment_view
from ledger.payments.idempotency import IdempotencyConflict, RequestInProgress
from ledger.psp.gateway import PaymentGateway, get_gateway
from ledger.settings import Settings, get_settings

router = APIRouter(prefix="/v1/payments", tags=["payments"])


@router.post("/capture", status_code=201, response_model=CaptureResult)
def capture(
    request: CaptureRequest,
    idempotency_key: str = Header(..., alias="Idempotency-Key", min_length=8, max_length=128),
    session=Depends(get_session),
    gateway: PaymentGateway = Depends(get_gateway),
    publisher: EventPublisher = Depends(get_publisher),
    settings: Settings = Depends(get_settings),
):
    try:
        return capture_payment(session, gateway, publisher, request, idempotency_key, settings)
    except IdempotencyConflict:
        raise HTTPException(status_code=422, detail="Idempotency-Key was already used with a different request body")
    except RequestInProgress:
        raise HTTPException(status_code=409, detail="a request with this Idempotency-Key is still being processed")


@router.get("/{payment_id}")
def get_payment(payment_id: str, session=Depends(get_session)):
    view = get_payment_view(session, payment_id)
    if view is None:
        raise HTTPException(status_code=404, detail="payment not found")
    return view
"##,
    ),
    (
        "ledger/api/refunds.py",
        r##"
from datetime import datetime

from fastapi import APIRouter, Depends, Header
from pydantic import BaseModel

from ledger.db.session import get_session
from ledger.payments.refunds import create_refund, refund_prorated
from ledger.psp.gateway import PaymentGateway, get_gateway

router = APIRouter(prefix="/v1/refunds", tags=["refunds"])


class RefundRequest(BaseModel):
    payment_id: str
    amount_minor: int
    reason: str


class ProratedRefundRequest(BaseModel):
    subscription_id: str
    customer_id: str
    period_start: datetime
    period_end: datetime
    cancelled_at: datetime


@router.post("", status_code=201)
def refund(request: RefundRequest, idempotency_key: str = Header(..., alias="Idempotency-Key"), session=Depends(get_session), gateway: PaymentGateway = Depends(get_gateway)):
    return create_refund(session, gateway, request.payment_id, request.amount_minor, request.reason, idempotency_key)


@router.post("/prorated", status_code=201)
def prorated(request: ProratedRefundRequest, session=Depends(get_session), gateway: PaymentGateway = Depends(get_gateway)):
    refund = refund_prorated(
        session,
        gateway,
        subscription_id=request.subscription_id,
        customer_id=request.customer_id,
        period_start=request.period_start,
        period_end=request.period_end,
        cancelled_at=request.cancelled_at,
    )
    return {"refundId": refund.id if refund else None}
"##,
    ),
    (
        "ledger/payments/idempotency.py",
        r##"
"""Idempotency keys for payment capture (ADR-0004).

Every capture request carries an ``Idempotency-Key`` header. The first
request with a given key reserves it; a replay of the same request gets the
stored response instead of a new call to the provider, and a different
request reusing the key is rejected. Keys expire after IDEMPOTENCY_TTL_HOURS.
"""

import hashlib
import json
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone

from sqlalchemy import select
from sqlalchemy.exc import IntegrityError
from sqlalchemy.orm import Session

from ledger.db.models import IdempotencyRecord


class IdempotencyConflict(Exception):
    """The key was already used for a request with a different body."""


class RequestInProgress(Exception):
    """Another worker holds the key and has not finished yet."""


@dataclass(frozen=True)
class Replay:
    status_code: int
    body: dict


def fingerprint_request(method: str, path: str, body: dict) -> str:
    canonical = json.dumps(body, sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(f"{method} {path}\n{canonical}".encode()).hexdigest()


class IdempotencyStore:
    def __init__(self, session: Session, ttl: timedelta):
        self._session = session
        self._ttl = ttl

    def reserve(self, key: str, fingerprint: str) -> Replay | None:
        """Claims ``key``, or returns the stored response of the earlier request.

        The primary key on idempotency_keys.key makes the claim atomic across
        workers: the loser of a race gets IntegrityError and re-reads the row.
        """
        now = datetime.now(timezone.utc)
        existing = self._session.get(IdempotencyRecord, key)
        if existing is not None and existing.expires_at > now:
            return self._dedupe(existing, fingerprint)
        if existing is not None:
            self._session.delete(existing)
            self._session.flush()
        self._session.add(IdempotencyRecord(key=key, fingerprint=fingerprint, created_at=now, expires_at=now + self._ttl))
        try:
            self._session.flush()
        except IntegrityError:
            self._session.rollback()
            raced = self._session.scalar(select(IdempotencyRecord).where(IdempotencyRecord.key == key))
            if raced is None:
                raise
            return self._dedupe(raced, fingerprint)
        return None

    def complete(self, key: str, status_code: int, body: dict) -> None:
        record = self._session.get(IdempotencyRecord, key)
        if record is None:
            return
        record.response_status = status_code
        record.response_body = body
        self._session.flush()

    @staticmethod
    def _dedupe(record: IdempotencyRecord, fingerprint: str) -> Replay:
        if record.fingerprint != fingerprint:
            raise IdempotencyConflict(record.key)
        if record.response_status is None:
            raise RequestInProgress(record.key)
        return Replay(status_code=record.response_status, body=record.response_body)
"##,
    ),
    (
        "ledger/payments/capture.py",
        r##"
"""Payment capture: reserve the idempotency key, call the provider once,
record the payment, publish ``payment.captured``."""

from datetime import datetime, timedelta, timezone
from uuid import uuid4

from pydantic import BaseModel
from sqlalchemy.orm import Session

from ledger.db.models import Payment
from ledger.events import EventPublisher
from ledger.payments.idempotency import IdempotencyStore, fingerprint_request
from ledger.psp.gateway import PaymentGateway
from ledger.settings import Settings


class CaptureRequest(BaseModel):
    order_id: str
    customer_id: str
    amount_minor: int
    currency: str
    payment_method_id: str


class CaptureResult(BaseModel):
    payment_id: str
    status: str
    psp_reference: str | None = None


def capture_payment(
    session: Session,
    gateway: PaymentGateway,
    publisher: EventPublisher,
    request: CaptureRequest,
    idempotency_key: str,
    settings: Settings,
) -> CaptureResult:
    store = IdempotencyStore(session, timedelta(hours=settings.idempotency_ttl_hours))
    fingerprint = fingerprint_request("POST", "/v1/payments/capture", request.model_dump(mode="json"))
    replay = store.reserve(idempotency_key, fingerprint)
    if replay is not None:
        # Duplicate submission (client retry, double click, gateway timeout):
        # hand back the original outcome; the provider is not called again.
        return CaptureResult.model_validate(replay.body)

    psp_result = gateway.capture(
        amount_minor=request.amount_minor,
        currency=request.currency,
        payment_method_id=request.payment_method_id,
        reference=f"order:{request.order_id}",
        idempotency_key=idempotency_key,
    )
    payment = Payment(
        id=str(uuid4()),
        order_id=request.order_id,
        customer_id=request.customer_id,
        amount_minor=request.amount_minor,
        currency=request.currency,
        status=psp_result.status,
        psp_reference=psp_result.reference,
        captured_at=datetime.now(timezone.utc) if psp_result.status == "captured" else None,
    )
    session.add(payment)
    result = CaptureResult(payment_id=payment.id, status=payment.status, psp_reference=payment.psp_reference)
    store.complete(idempotency_key, 201, result.model_dump(mode="json"))
    session.commit()
    if payment.status == "captured":
        publisher.payment_captured(payment)
    return result


def get_payment_view(session: Session, payment_id: str) -> dict | None:
    payment = session.get(Payment, payment_id)
    if payment is None:
        return None
    return {
        "paymentId": payment.id,
        "orderId": payment.order_id,
        "status": payment.status,
        "amountMinor": payment.amount_minor,
        "currency": payment.currency,
    }
"##,
    ),
    (
        "ledger/payments/refunds.py",
        r##"
"""Refunds, including the prorated refund issued when a yearly membership is
cancelled before its term ends (ADR-0005)."""

from datetime import datetime, timezone
from uuid import uuid4

from sqlalchemy import select
from sqlalchemy.orm import Session

from ledger.db.models import Payment, Refund
from ledger.psp.gateway import PaymentGateway


def months_between(start: datetime, end: datetime) -> int:
    return (end.year - start.year) * 12 + (end.month - start.month)


def prorated_amount(paid_minor: int, period_start: datetime, period_end: datetime, cancelled_at: datetime) -> int:
    """Unused whole months are paid back; the month in progress is not.

    Monthly plans therefore never get money back: they simply run until the
    end of the month that was paid for.
    """
    total_months = months_between(period_start, period_end)
    if total_months <= 0:
        return 0
    used_months = months_between(period_start, cancelled_at) + 1
    unused_months = max(total_months - used_months, 0)
    return paid_minor * unused_months // total_months


def refund_prorated(
    session: Session,
    gateway: PaymentGateway,
    *,
    subscription_id: str,
    customer_id: str,
    period_start: datetime,
    period_end: datetime,
    cancelled_at: datetime,
) -> Refund | None:
    payment = session.scalar(
        select(Payment)
        .where(Payment.subscription_id == subscription_id, Payment.status == "captured")
        .order_by(Payment.captured_at.desc())
        .limit(1)
    )
    if payment is None:
        return None
    amount = prorated_amount(payment.amount_minor, period_start, period_end, cancelled_at)
    if amount == 0:
        return None
    psp = gateway.refund(payment.psp_reference, amount, idempotency_key=f"prorated:{subscription_id}")
    refund = Refund(
        id=str(uuid4()),
        payment_id=payment.id,
        customer_id=customer_id,
        amount_minor=amount,
        reason="membership_cancelled",
        psp_reference=psp.reference,
        created_at=datetime.now(timezone.utc),
    )
    session.add(refund)
    session.commit()
    return refund


def create_refund(session: Session, gateway: PaymentGateway, payment_id: str, amount_minor: int, reason: str, idempotency_key: str) -> Refund:
    """Manual refund issued by customer support."""
    payment = session.get(Payment, payment_id)
    if payment is None or amount_minor <= 0 or amount_minor > payment.amount_minor:
        raise ValueError("refund amount must be between 1 and the captured amount")
    psp = gateway.refund(payment.psp_reference, amount_minor, idempotency_key=idempotency_key)
    refund = Refund(
        id=str(uuid4()),
        payment_id=payment.id,
        customer_id=payment.customer_id,
        amount_minor=amount_minor,
        reason=reason,
        psp_reference=psp.reference,
        created_at=datetime.now(timezone.utc),
    )
    session.add(refund)
    session.commit()
    return refund
"##,
    ),
    (
        "ledger/psp/gateway.py",
        r##"
"""HTTP adapter for the card payment service provider (PSP)."""

from dataclasses import dataclass

import httpx

from ledger.settings import get_settings


@dataclass(frozen=True)
class PspResult:
    status: str
    reference: str | None


class PaymentGateway:
    """Talks to the PSP. Requests time out after PSP_TIMEOUT_SECONDS; on a
    timeout the call is retried once with the same Idempotency-Key, which the
    provider also honours, so the retry cannot create a second capture."""

    def __init__(self, base_url: str, api_key: str, timeout_seconds: float):
        self._client = httpx.Client(
            base_url=base_url,
            timeout=timeout_seconds,
            headers={"Authorization": f"Bearer {api_key}"},
        )

    def capture(self, *, amount_minor: int, currency: str, payment_method_id: str, reference: str, idempotency_key: str) -> PspResult:
        body = {
            "amount": amount_minor,
            "currency": currency,
            "payment_method": payment_method_id,
            "reference": reference,
        }
        data = self._post("/captures", body, idempotency_key)
        return PspResult(status=data.get("status", "failed"), reference=data.get("id"))

    def refund(self, psp_reference: str | None, amount_minor: int, *, idempotency_key: str) -> PspResult:
        data = self._post("/refunds", {"capture": psp_reference, "amount": amount_minor}, idempotency_key)
        return PspResult(status=data.get("status", "failed"), reference=data.get("id"))

    def _post(self, path: str, body: dict, idempotency_key: str) -> dict:
        headers = {"Idempotency-Key": idempotency_key}
        try:
            response = self._client.post(path, json=body, headers=headers)
        except httpx.TimeoutException:
            response = self._client.post(path, json=body, headers=headers)
        response.raise_for_status()
        return response.json()


def get_gateway() -> PaymentGateway:
    settings = get_settings()
    return PaymentGateway(settings.psp_api_url, settings.psp_api_key, settings.psp_timeout_seconds)
"##,
    ),
    (
        "ledger/db/models.py",
        r##"
from datetime import datetime

from sqlalchemy import JSON, BigInteger, DateTime, SmallInteger, String
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column


class Base(DeclarativeBase):
    pass


class Payment(Base):
    __tablename__ = "payments"

    id: Mapped[str] = mapped_column(String(36), primary_key=True)
    order_id: Mapped[str] = mapped_column(String(36), index=True)
    subscription_id: Mapped[str | None] = mapped_column(String(36), nullable=True)
    customer_id: Mapped[str] = mapped_column(String(36))
    amount_minor: Mapped[int] = mapped_column(BigInteger)
    currency: Mapped[str] = mapped_column(String(3))
    status: Mapped[str] = mapped_column(String(16))
    psp_reference: Mapped[str | None] = mapped_column(String(64), nullable=True)
    captured_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True), nullable=True)


class Refund(Base):
    __tablename__ = "refunds"

    id: Mapped[str] = mapped_column(String(36), primary_key=True)
    payment_id: Mapped[str] = mapped_column(String(36), index=True)
    customer_id: Mapped[str] = mapped_column(String(36))
    amount_minor: Mapped[int] = mapped_column(BigInteger)
    reason: Mapped[str] = mapped_column(String(64))
    psp_reference: Mapped[str | None] = mapped_column(String(64), nullable=True)
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True))


class IdempotencyRecord(Base):
    __tablename__ = "idempotency_keys"

    key: Mapped[str] = mapped_column(String(128), primary_key=True)
    fingerprint: Mapped[str] = mapped_column(String(64))
    response_status: Mapped[int | None] = mapped_column(SmallInteger, nullable=True)
    response_body: Mapped[dict | None] = mapped_column(JSON, nullable=True)
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True))
    expires_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), index=True)


class SubscriptionRow(Base):
    """Read-only mapping of billing-api's subscriptions table, used to look up
    the paid period when a refund is requested. billing-api owns the table
    (ADR-0003); never write through this model."""

    __tablename__ = "subscriptions"

    id: Mapped[str] = mapped_column(String(36), primary_key=True)
    customer_id: Mapped[str] = mapped_column(String(36))
    plan_id: Mapped[str] = mapped_column(String(64))
    status: Mapped[str] = mapped_column(String(16))
    interval: Mapped[str] = mapped_column(String(8))
    current_period_start: Mapped[datetime] = mapped_column(DateTime(timezone=True))
    current_period_end: Mapped[datetime] = mapped_column(DateTime(timezone=True))
    cancelled_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True), nullable=True)
"##,
    ),
    (
        "ledger/db/session.py",
        r##"
from collections.abc import Iterator

from sqlalchemy import create_engine
from sqlalchemy.orm import Session, sessionmaker

from ledger.settings import get_settings

_engine = create_engine(get_settings().database_url, pool_pre_ping=True)
_Session = sessionmaker(bind=_engine, expire_on_commit=False)


def get_session() -> Iterator[Session]:
    session = _Session()
    try:
        yield session
    finally:
        session.close()
"##,
    ),
    (
        "ledger/events.py",
        r##"
"""Domain events published by the ledger (topic == event type, ADR-0002)."""

import json
from datetime import datetime, timezone
from uuid import uuid4

from ledger.db.models import Payment

PAYMENT_CAPTURED = "payment.captured"


class EventPublisher:
    def __init__(self, producer):
        self._producer = producer

    def payment_captured(self, payment: Payment) -> None:
        """Payload schema: contracts/events/payment.captured.v1.json."""
        envelope = {
            "id": str(uuid4()),
            "type": PAYMENT_CAPTURED,
            "version": 1,
            "occurredAt": datetime.now(timezone.utc).isoformat(),
            "data": {
                "paymentId": payment.id,
                "orderId": payment.order_id,
                "customerId": payment.customer_id,
                "amountMinor": payment.amount_minor,
                "currency": payment.currency,
                "capturedAt": payment.captured_at.isoformat() if payment.captured_at else None,
            },
        }
        self._producer.send(PAYMENT_CAPTURED, key=payment.order_id.encode(), value=json.dumps(envelope).encode())


_publisher: EventPublisher | None = None


def get_publisher() -> EventPublisher:
    if _publisher is None:
        raise RuntimeError("event publisher not initialised")
    return _publisher
"##,
    ),
    (
        "ledger/grpc_server.py",
        r##"
"""gRPC read API (contracts/proto/ledger/v1/ledger.proto) for internal callers
such as orders-service."""

from concurrent import futures

import grpc
from acme_contracts.ledger.v1 import ledger_pb2, ledger_pb2_grpc

from ledger.db.models import Payment
from ledger.db.session import get_session

_STATUS = {
    "pending": ledger_pb2.PAYMENT_STATUS_PENDING,
    "captured": ledger_pb2.PAYMENT_STATUS_CAPTURED,
    "failed": ledger_pb2.PAYMENT_STATUS_FAILED,
    "refunded": ledger_pb2.PAYMENT_STATUS_REFUNDED,
}


class LedgerServicer(ledger_pb2_grpc.LedgerServiceServicer):
    def GetPaymentStatus(self, request, context):
        session = next(get_session())
        payment = session.get(Payment, request.payment_id)
        if payment is None:
            context.abort(grpc.StatusCode.NOT_FOUND, "payment not found")
        return ledger_pb2.GetPaymentStatusResponse(
            payment_id=payment.id,
            status=_STATUS.get(payment.status, ledger_pb2.PAYMENT_STATUS_UNSPECIFIED),
        )

    def ListPaymentsForOrder(self, request, context):
        session = next(get_session())
        payments = session.query(Payment).filter(Payment.order_id == request.order_id).all()
        return ledger_pb2.ListPaymentsForOrderResponse(payment_ids=[p.id for p in payments])


def serve(port: int) -> grpc.Server:
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=8))
    ledger_pb2_grpc.add_LedgerServiceServicer_to_server(LedgerServicer(), server)
    server.add_insecure_port(f"[::]:{port}")
    server.start()
    return server
"##,
    ),
];
