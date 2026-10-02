//! `storefront-web`: the customer website (TypeScript / React).

pub(super) const FILES: &[(&str, &str)] = &[
    (
        "README.md",
        r##"
# storefront-web

The Acme Goods customer website: catalog, checkout and the account area where
members manage their Acme Plus membership.

## Development

    npm install
    NEXT_PUBLIC_BILLING_API_URL=http://localhost:8080 \
    NEXT_PUBLIC_ORDERS_API_URL=http://localhost:8081 npm run dev

The site talks to two backends:

- `billing-api` for plans, memberships and checkout
  (`src/api/subscriptions.ts`, `src/api/checkout.ts`);
- `orders-service` for the order history (`src/api/orders.ts`).

Both base URLs are baked in at build time from the `NEXT_PUBLIC_*` variables
(see `src/api/client.ts`).

Translations live in `src/i18n/locales/{en,tr}.json`; `src/i18n/index.ts`
explains how the visitor's language is chosen.
"##,
    ),
    (
        "package.json",
        r##"
{
  "name": "@acme/storefront-web",
  "version": "3.14.0",
  "private": true,
  "scripts": {
    "dev": "next dev",
    "build": "next build",
    "start": "next start",
    "lint": "eslint src",
    "test": "vitest run"
  },
  "dependencies": {
    "next": "14.2.3",
    "react": "18.3.1",
    "react-dom": "18.3.1"
  },
  "devDependencies": {
    "eslint": "8.57.0",
    "typescript": "5.4.5",
    "vitest": "1.6.0"
  }
}
"##,
    ),
    (
        "src/api/client.ts",
        r##"
// Thin fetch wrapper shared by every API module. Base URLs are injected at
// build time so a bundle can never point at another environment.

export const BILLING_API_URL = process.env.NEXT_PUBLIC_BILLING_API_URL ?? "";
export const ORDERS_API_URL = process.env.NEXT_PUBLIC_ORDERS_API_URL ?? "";

export class ApiError extends Error {
  constructor(
    public readonly status: number,
    public readonly code: string,
    message: string,
  ) {
    super(message);
  }
}

export interface RequestOptions {
  method?: "GET" | "POST" | "PUT" | "DELETE";
  body?: unknown;
  headers?: Record<string, string>;
  signal?: AbortSignal;
}

function authHeader(): Record<string, string> {
  if (typeof window === "undefined") {
    return {};
  }
  const token = window.localStorage.getItem("acme.session");
  return token ? { Authorization: `Bearer ${token}` } : {};
}

export async function request<T>(baseUrl: string, path: string, options: RequestOptions = {}): Promise<T> {
  if (!baseUrl) {
    throw new ApiError(0, "config", "API base URL is not configured");
  }
  const response = await fetch(`${baseUrl}${path}`, {
    method: options.method ?? "GET",
    headers: {
      "Content-Type": "application/json",
      ...authHeader(),
      ...options.headers,
    },
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
    signal: options.signal,
  });
  if (!response.ok) {
    const problem = await response.json().catch(() => ({}));
    throw new ApiError(response.status, problem.code ?? "http_error", problem.message ?? response.statusText);
  }
  return (await response.json()) as T;
}
"##,
    ),
    (
        "src/api/subscriptions.ts",
        r##"
import { BILLING_API_URL, request } from "./client";

export type SubscriptionStatus = "active" | "past_due" | "cancelled" | "expired";

export type CancelReason = "too_expensive" | "not_using" | "switching_provider" | "other";

export interface Subscription {
  id: string;
  planId: string;
  status: SubscriptionStatus;
  currentPeriodEnd: string;
  cancelledAt: string | null;
  cancelReason: CancelReason | null;
}

export interface Plan {
  id: string;
  name: string;
  priceMinor: number;
  currency: string;
  interval: "month" | "year";
}

export function getSubscription(subscriptionId: string): Promise<Subscription> {
  return request<Subscription>(BILLING_API_URL, `/v1/subscriptions/${encodeURIComponent(subscriptionId)}`);
}

export function listPlans(): Promise<Plan[]> {
  return request<Plan[]>(BILLING_API_URL, "/v1/plans");
}

/**
 * Cancels the member's subscription. The billing API keeps the membership
 * active until `currentPeriodEnd`; the returned object reflects that.
 */
export function cancelSubscription(subscriptionId: string, reason: CancelReason, feedback?: string): Promise<Subscription> {
  return request<Subscription>(BILLING_API_URL, `/v1/subscriptions/${encodeURIComponent(subscriptionId)}/cancel`, {
    method: "POST",
    body: { reason, feedback },
  });
}

/** Undoes a cancellation while the current period has not ended yet. */
export function resumeSubscription(subscriptionId: string): Promise<Subscription> {
  return request<Subscription>(BILLING_API_URL, `/v1/subscriptions/${encodeURIComponent(subscriptionId)}/resume`, {
    method: "POST",
  });
}
"##,
    ),
    (
        "src/api/orders.ts",
        r##"
import { ORDERS_API_URL, request } from "./client";

export interface OrderLine {
  sku: string;
  title: string;
  quantity: number;
  unitPriceMinor: number;
}

export interface Order {
  id: string;
  status: "pending" | "paid" | "shipped" | "cancelled";
  totalMinor: number;
  currency: string;
  placedAt: string;
  lines: OrderLine[];
}

export interface OrderPage {
  items: Order[];
  nextCursor: string | null;
}

export function listOrders(cursor?: string): Promise<OrderPage> {
  const query = cursor ? `?cursor=${encodeURIComponent(cursor)}` : "";
  return request<OrderPage>(ORDERS_API_URL, `/v1/orders${query}`);
}

export function getOrder(orderId: string): Promise<Order> {
  return request<Order>(ORDERS_API_URL, `/v1/orders/${encodeURIComponent(orderId)}`);
}
"##,
    ),
    (
        "src/api/checkout.ts",
        r##"
import { BILLING_API_URL, request } from "./client";

export interface CheckoutLine {
  sku: string;
  quantity: number;
}

export interface CheckoutRequest {
  lines: CheckoutLine[];
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
 * Starts checkout. One key is generated per checkout attempt and reused when
 * the visitor presses "Pay" again after a network error, so the backend can
 * recognise the retry and answer with the original result.
 */
export function createCheckout(payload: CheckoutRequest, attemptKey: string = crypto.randomUUID()): Promise<CheckoutResult> {
  return request<CheckoutResult>(BILLING_API_URL, "/v1/checkout", {
    method: "POST",
    body: payload,
    headers: { "Idempotency-Key": attemptKey },
  });
}
"##,
    ),
    (
        "src/pages/checkout.tsx",
        r##"
import { useRef, useState } from "react";
import { createCheckout, type CheckoutRequest } from "../api/checkout";
import { PriceTag } from "../components/PriceTag";
import { useT } from "../i18n";

interface CheckoutPageProps {
  draft: CheckoutRequest;
  totalMinor: number;
  currency: string;
}

export default function CheckoutPage({ draft, totalMinor, currency }: CheckoutPageProps) {
  const t = useT();
  // The same key is sent for every retry during this page visit.
  const attemptKey = useRef(crypto.randomUUID());
  const [state, setState] = useState<"idle" | "submitting" | "failed">("idle");

  async function pay() {
    setState("submitting");
    try {
      const result = await createCheckout(draft, attemptKey.current);
      window.location.assign(`/orders/${result.orderId}`);
    } catch {
      setState("failed");
    }
  }

  return (
    <main>
      <h1>{t("checkout.title")}</h1>
      <PriceTag amountMinor={totalMinor} currency={currency} />
      {state === "failed" && <p role="alert">{t("checkout.failed")}</p>}
      <button disabled={state === "submitting"} onClick={pay}>
        {t("checkout.pay")}
      </button>
    </main>
  );
}
"##,
    ),
    (
        "src/pages/account/subscription.tsx",
        r##"
import { useState } from "react";
import { CancelSubscriptionDialog } from "../../components/CancelSubscriptionDialog";
import { useSubscription } from "../../hooks/useSubscription";
import { formatDate, useT } from "../../i18n";

export default function SubscriptionPage() {
  const t = useT();
  const { subscription, cancel, resume, loading } = useSubscription();
  const [dialogOpen, setDialogOpen] = useState(false);

  if (loading || !subscription) {
    return <p>{t("common.loading")}</p>;
  }

  const endsAt = formatDate(subscription.currentPeriodEnd);
  return (
    <section>
      <h1>{t("subscription.title")}</h1>
      {subscription.status === "cancelled" ? (
        <>
          <p>{t("subscription.cancelledUntil", { date: endsAt })}</p>
          <button onClick={resume}>{t("subscription.resume")}</button>
        </>
      ) : (
        <>
          <p>{t("subscription.renewsOn", { date: endsAt })}</p>
          <button onClick={() => setDialogOpen(true)}>{t("subscription.cancel.button")}</button>
        </>
      )}
      <CancelSubscriptionDialog
        open={dialogOpen}
        onClose={() => setDialogOpen(false)}
        onConfirm={async (reason, feedback) => {
          await cancel(reason, feedback);
          setDialogOpen(false);
        }}
      />
    </section>
  );
}
"##,
    ),
    (
        "src/pages/orders/index.tsx",
        r##"
import { useEffect, useState } from "react";
import { listOrders, type Order } from "../../api/orders";
import { PriceTag } from "../../components/PriceTag";
import { formatDate, useT } from "../../i18n";

export default function OrderHistoryPage() {
  const t = useT();
  const [orders, setOrders] = useState<Order[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);

  useEffect(() => {
    listOrders().then((page) => {
      setOrders(page.items);
      setCursor(page.nextCursor);
    });
  }, []);

  async function loadMore() {
    if (!cursor) return;
    const page = await listOrders(cursor);
    setOrders((current) => [...current, ...page.items]);
    setCursor(page.nextCursor);
  }

  if (orders.length === 0) {
    return <p>{t("orders.empty")}</p>;
  }
  return (
    <section>
      <h1>{t("orders.title")}</h1>
      <ul>
        {orders.map((order) => (
          <li key={order.id}>
            <a href={`/orders/${order.id}`}>{formatDate(order.placedAt)}</a>
            <PriceTag amountMinor={order.totalMinor} currency={order.currency} />
            <span>{t(`orders.status.${order.status}`)}</span>
          </li>
        ))}
      </ul>
      {cursor && <button onClick={loadMore}>{t("orders.more")}</button>}
    </section>
  );
}
"##,
    ),
    (
        "src/components/CancelSubscriptionDialog.tsx",
        r##"
import { useState } from "react";
import type { CancelReason } from "../api/subscriptions";
import { useT } from "../i18n";

const REASONS: CancelReason[] = ["too_expensive", "not_using", "switching_provider", "other"];

export interface CancelSubscriptionDialogProps {
  open: boolean;
  onClose: () => void;
  onConfirm: (reason: CancelReason, feedback?: string) => Promise<void>;
}

export function CancelSubscriptionDialog({ open, onClose, onConfirm }: CancelSubscriptionDialogProps) {
  const t = useT();
  const [reason, setReason] = useState<CancelReason>("not_using");
  const [feedback, setFeedback] = useState("");
  if (!open) {
    return null;
  }
  return (
    <div role="dialog" aria-labelledby="cancel-title">
      <h2 id="cancel-title">{t("subscription.cancel.title")}</h2>
      <p>{t("subscription.cancel.body")}</p>
      <label>
        {t("subscription.cancel.reasonLabel")}
        <select value={reason} onChange={(event) => setReason(event.target.value as CancelReason)}>
          {REASONS.map((r) => (
            <option key={r} value={r}>
              {t(`subscription.cancel.reasons.${r}`)}
            </option>
          ))}
        </select>
      </label>
      <textarea
        value={feedback}
        onChange={(event) => setFeedback(event.target.value)}
        placeholder={t("subscription.cancel.feedbackPlaceholder")}
      />
      <button onClick={onClose}>{t("subscription.cancel.keep")}</button>
      <button onClick={() => onConfirm(reason, feedback || undefined)}>{t("subscription.cancel.confirm")}</button>
    </div>
  );
}
"##,
    ),
    (
        "src/components/PriceTag.tsx",
        r##"
import { currentLocale } from "../i18n";

/**
 * Renders an amount given in minor units (cents, kuruş) in the visitor's
 * locale, e.g. 123450 TRY becomes "₺1.234,50" for `tr` and "TRY 1,234.50"
 * for `en`. Amounts are never stored as floating point.
 */
export function PriceTag({ amountMinor, currency }: { amountMinor: number; currency: string }) {
  const formatter = new Intl.NumberFormat(currentLocale(), { style: "currency", currency });
  return <span className="price">{formatter.format(amountMinor / 100)}</span>;
}
"##,
    ),
    (
        "src/hooks/useSubscription.ts",
        r##"
import { useCallback, useEffect, useState } from "react";
import {
  cancelSubscription,
  getSubscription,
  resumeSubscription,
  type CancelReason,
  type Subscription,
} from "../api/subscriptions";

/** Loads the signed-in member's subscription and exposes cancel / resume. */
export function useSubscription() {
  const subscriptionId = typeof window === "undefined" ? null : window.localStorage.getItem("acme.subscriptionId");
  const [subscription, setSubscription] = useState<Subscription | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    if (!subscriptionId) {
      setLoading(false);
      return;
    }
    getSubscription(subscriptionId)
      .then(setSubscription)
      .finally(() => setLoading(false));
  }, [subscriptionId]);

  const cancel = useCallback(
    async (reason: CancelReason, feedback?: string) => {
      if (!subscriptionId) return;
      setSubscription(await cancelSubscription(subscriptionId, reason, feedback));
    },
    [subscriptionId],
  );

  const resume = useCallback(async () => {
    if (!subscriptionId) return;
    setSubscription(await resumeSubscription(subscriptionId));
  }, [subscriptionId]);

  return { subscription, loading, cancel, resume };
}
"##,
    ),
    (
        "src/i18n/index.ts",
        r##"
import en from "./locales/en.json";
import tr from "./locales/tr.json";

type Messages = { [key: string]: string | Messages };

const CATALOGS: Record<string, Messages> = { en, tr };
export const DEFAULT_LOCALE = "en";
export const SUPPORTED_LOCALES = Object.keys(CATALOGS);

/**
 * Chooses the UI language: the `lang` cookie wins, then the browser's
 * preferred languages in order; a language we do not ship falls back to
 * English. Region subtags are ignored (`tr-TR` -> `tr`).
 */
export function currentLocale(): string {
  if (typeof document !== "undefined") {
    const cookie = document.cookie.split("; ").find((c) => c.startsWith("lang="));
    const fromCookie = cookie?.slice("lang=".length);
    if (fromCookie && SUPPORTED_LOCALES.includes(fromCookie)) {
      return fromCookie;
    }
  }
  if (typeof navigator !== "undefined") {
    for (const tag of navigator.languages ?? []) {
      const base = tag.toLowerCase().split("-")[0];
      if (SUPPORTED_LOCALES.includes(base)) {
        return base;
      }
    }
  }
  return DEFAULT_LOCALE;
}

function lookup(messages: Messages | undefined, key: string): string | undefined {
  let node: string | Messages | undefined = messages;
  for (const part of key.split(".")) {
    if (typeof node !== "object") return undefined;
    node = node[part];
  }
  return typeof node === "string" ? node : undefined;
}

/** Missing keys fall back to the English text, then to the key itself. */
export function translate(locale: string, key: string, params: Record<string, string> = {}): string {
  const template = lookup(CATALOGS[locale], key) ?? lookup(CATALOGS[DEFAULT_LOCALE], key) ?? key;
  return template.replace(/\{(\w+)\}/g, (_, name: string) => params[name] ?? `{${name}}`);
}

export function useT() {
  const locale = currentLocale();
  return (key: string, params?: Record<string, string>) => translate(locale, key, params);
}

export function formatDate(iso: string): string {
  return new Intl.DateTimeFormat(currentLocale(), { dateStyle: "long" }).format(new Date(iso));
}
"##,
    ),
    (
        "src/i18n/locales/en.json",
        r##"
{
  "common": {
    "loading": "Loading…"
  },
  "checkout": {
    "title": "Checkout",
    "pay": "Pay now",
    "failed": "Payment could not be completed. Nothing was taken from your card; please try again."
  },
  "subscription": {
    "title": "Your Acme Plus membership",
    "renewsOn": "Renews on {date}",
    "cancelledUntil": "Cancelled. Your benefits stay active until {date}.",
    "resume": "Keep my membership",
    "cancel": {
      "button": "Cancel membership",
      "title": "Cancel Acme Plus?",
      "body": "You keep free delivery and member prices until the end of the current period.",
      "reasonLabel": "Why are you leaving?",
      "feedbackPlaceholder": "Anything we could do better? (optional)",
      "keep": "Keep membership",
      "confirm": "Yes, cancel",
      "reasons": {
        "too_expensive": "It is too expensive",
        "not_using": "I don't use it enough",
        "switching_provider": "I'm moving to another shop",
        "other": "Something else"
      }
    }
  },
  "orders": {
    "title": "Your orders",
    "empty": "You have not placed any orders yet.",
    "more": "Show older orders",
    "status": {
      "pending": "Waiting for payment",
      "paid": "Paid",
      "shipped": "On its way",
      "cancelled": "Cancelled"
    }
  }
}
"##,
    ),
    (
        "src/i18n/locales/tr.json",
        r##"
{
  "common": {
    "loading": "Yükleniyor…"
  },
  "checkout": {
    "title": "Ödeme",
    "pay": "Şimdi öde",
    "failed": "Ödeme tamamlanamadı. Kartınızdan çekim yapılmadı; lütfen tekrar deneyin."
  },
  "subscription": {
    "title": "Acme Plus üyeliğiniz",
    "renewsOn": "{date} tarihinde yenilenir",
    "cancelledUntil": "İptal edildi. Avantajlarınız {date} tarihine kadar geçerli.",
    "resume": "Üyeliğime devam et",
    "cancel": {
      "button": "Üyeliği iptal et",
      "title": "Acme Plus iptal edilsin mi?",
      "body": "Ücretsiz teslimat ve üyelere özel fiyatlar mevcut dönemin sonuna kadar devam eder.",
      "reasonLabel": "Neden ayrılıyorsunuz?",
      "feedbackPlaceholder": "Neyi daha iyi yapabiliriz? (isteğe bağlı)",
      "keep": "Üyeliğimi koru",
      "confirm": "Evet, iptal et",
      "reasons": {
        "too_expensive": "Çok pahalı",
        "not_using": "Yeterince kullanmıyorum",
        "switching_provider": "Başka bir mağazaya geçiyorum",
        "other": "Başka bir neden"
      }
    }
  },
  "orders": {
    "title": "Siparişleriniz",
    "empty": "Henüz sipariş vermediniz.",
    "more": "Daha eski siparişleri göster",
    "status": {
      "pending": "Ödeme bekleniyor",
      "paid": "Ödendi",
      "shipped": "Yolda",
      "cancelled": "İptal edildi"
    }
  }
}
"##,
    ),
];
