import { getContext, setContext } from 'svelte';
import { HttpApiClient, type ApiClient } from './client';

export * from './client';
export type * from './types';

const KEY = Symbol('knowell.api');

export function provideApi(client: ApiClient): void {
  setContext(KEY, client);
}

export function getApi(): ApiClient {
  const c = getContext<ApiClient | undefined>(KEY);
  if (!c) throw new Error('no ApiClient provided; wrap the tree with provideApi()');
  return c;
}

/** Context map for tests: `render(Comp, { context: apiContext(client) })`. */
export function apiContext(client: ApiClient): Map<symbol, ApiClient> {
  return new Map([[KEY, client]]);
}

/**
 * Chooses the client. The mock is only reachable in dev (`vite`) or when the bundle is built
 * with `VITE_API=mock`; the constant conditions let the bundler drop it from production builds.
 */
export async function createApiClient(): Promise<{ client: ApiClient; mock: boolean }> {
  if (
    import.meta.env.VITE_API === 'mock' ||
    (import.meta.env.DEV && import.meta.env.VITE_API !== 'http')
  ) {
    const { MockApiClient } = await import('./mock');
    return {
      client: new MockApiClient({
        role: import.meta.env.VITE_MOCK_ROLE === 'hub' ? 'hub' : 'standalone',
        shape: import.meta.env.VITE_MOCK_SHAPE === 'server' ? 'server' : 'rich',
        requireLogin: import.meta.env.VITE_MOCK_LOGIN === '1',
        engineUnavailable:
          import.meta.env.VITE_MOCK_ENGINE === 'unavailable'
            ? 'no engine is wired into this server yet'
            : undefined
      }),
      mock: true
    };
  }
  return { client: new HttpApiClient(), mock: false };
}
