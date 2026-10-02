/**
 * Minimal hash router. Hash routing needs no server-side fallback, so the static files can be
 * embedded in the Rust binary and mounted under any prefix.
 */
import { SvelteURLSearchParams } from 'svelte/reactivity';

export interface Route {
  path: string;
  params: Record<string, string>;
  query: URLSearchParams;
}

export function parseHash(hash: string): Route {
  const raw = hash.replace(/^#/, '') || '/';
  const [p = '/', q = ''] = raw.split('?');
  const path = '/' + p.split('/').filter(Boolean).map(safeDecode).join('/');
  return { path, params: {}, query: new SvelteURLSearchParams(q) };
}

function safeDecode(s: string): string {
  try {
    return decodeURIComponent(s);
  } catch {
    return s;
  }
}

class RouterState {
  current = $state<Route>(parseHash(typeof location === 'undefined' ? '' : location.hash));

  constructor() {
    if (typeof window !== 'undefined') {
      window.addEventListener('hashchange', () => {
        this.current = parseHash(location.hash);
      });
    }
  }
}

export const router = new RouterState();

export function href(path: string, query?: Record<string, string>): string {
  const q =
    query && Object.keys(query).length ? '?' + new SvelteURLSearchParams(query).toString() : '';
  return `#${path}${q}`;
}

export function navigate(path: string, query?: Record<string, string>): void {
  location.hash = href(path, query).slice(1);
}
