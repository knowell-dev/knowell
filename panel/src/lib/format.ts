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
