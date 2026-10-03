export function formatBytes(n: number): string {
  if (!Number.isFinite(n)) return '-';
  const u = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${u[i]}`;
}

export function formatDuration(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)} ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)} s`;
  const m = s / 60;
  if (m < 60) return `${Math.round(m)} min`;
  const h = m / 60;
  if (h < 48) return `${h.toFixed(h < 10 ? 1 : 0)} h`;
  return `${Math.round(h / 24)} d`;
}

export function formatPercent(v: number, digits = 0): string {
  return `${(v * 100).toFixed(digits)}%`;
}

export function formatUsd(micros: number): string {
  return `$${(micros / 1_000_000).toFixed(micros >= 100_000_000 ? 0 : 2)}`;
}

export function formatNumber(n: number): string {
  return new Intl.NumberFormat('en-US').format(n);
}

export function formatCompact(n: number): string {
  return new Intl.NumberFormat('en-US', { notation: 'compact', maximumFractionDigits: 1 }).format(
    n
  );
}

export function relativeTime(iso: string | null | undefined, now = Date.now()): string {
  if (!iso) return 'never';
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return '-';
  const diff = now - t;
  if (diff < 0) return `in ${formatDuration(-diff)}`;
  if (diff < 45_000) return 'just now';
  return `${formatDuration(diff)} ago`;
}

export function shortSha(s: string | null | undefined): string {
  return s ? s.slice(0, 7) : '-';
}

/** A run of prose or of inline code, as produced by {@link splitInlineCode}. */
export interface TextPart {
  text: string;
  code: boolean;
}

/**
 * Splits text that marks commands with backticks (`know init`) into prose and code parts, so
 * they render as `<code>` instead of literal backticks. Works on plain strings only, never HTML;
 * an unmatched backtick stays literal.
 */
export function splitInlineCode(text: string): TextPart[] {
  const parts: TextPart[] = [];
  let rest = text;
  for (;;) {
    const open = rest.indexOf('`');
    const close = open < 0 ? -1 : rest.indexOf('`', open + 1);
    if (close < 0) break;
    if (open > 0) parts.push({ text: rest.slice(0, open), code: false });
    if (close > open + 1) parts.push({ text: rest.slice(open + 1, close), code: true });
    rest = rest.slice(close + 1);
  }
  if (rest) parts.push({ text: rest, code: false });
  return parts;
}

/**
 * A principal (`user:<uuid>`) short enough for the top bar: the kind prefix stays, a long id is
 * cut to its first 8 characters. The full value belongs in a tooltip.
 */
export function shortUserId(id: string): string {
  const colon = id.indexOf(':');
  const prefix = colon < 0 ? '' : id.slice(0, colon + 1);
  const rest = id.slice(prefix.length);
  return rest.length > 12 ? `${prefix}${rest.slice(0, 8)}…` : id;
}
