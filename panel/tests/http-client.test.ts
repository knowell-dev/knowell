import { describe, expect, it, vi } from 'vitest';
import { ApiError, HttpApiClient, CSRF_HEADER } from '$lib/api';

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' }
  });
}

function client(fetchImpl: typeof fetch, timeoutMs = 1000) {
  return new HttpApiClient({ fetchImpl, timeoutMs });
}

describe('HttpApiClient error handling', () => {
  it('maps a structured error body to ApiError without losing the code', async () => {
    const f = vi.fn(async () =>
      json({ code: 'ref_not_found', message: 'ref release/9 was not found', requestId: 'r1' }, 404)
    );
    const err = await client(f as unknown as typeof fetch)
      .getProject('p')
      .catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    const e = err as ApiError;
    expect(e.code).toBe('ref_not_found');
    expect(e.status).toBe(404);
    expect(e.requestId).toBe('r1');
    expect(e.retryable).toBe(false);
  });

  it('never echoes a non-JSON error body', async () => {
    const f = vi.fn(
      async () => new Response('<html>stack trace KNOWELL_CANARY_SECRET</html>', { status: 500 })
    );
    const e = (await client(f as unknown as typeof fetch)
      .getHealth()
      .catch((x: unknown) => x)) as ApiError;
    expect(e.code).toBe('server_error');
    expect(e.message).not.toContain('CANARY');
    expect(e.retryable).toBe(true);
  });

  it('reports a network failure as retryable and actionable', async () => {
    const f = vi.fn(async () => {
      throw new TypeError('failed to fetch');
    });
    const e = (await client(f as unknown as typeof fetch)
      .getHealth()
      .catch((x: unknown) => x)) as ApiError;
    expect(e.code).toBe('network');
    expect(e.retryable).toBe(true);
    expect(e.message).toMatch(/engine/);
  });

  it('times out slow requests', async () => {
    const f = vi.fn(
      (_u: unknown, init?: RequestInit) =>
        new Promise<Response>((_res, rej) => {
          init?.signal?.addEventListener('abort', () =>
            rej(new DOMException('aborted', 'AbortError'))
          );
        })
    );
    const e = (await client(f as unknown as typeof fetch, 20)
      .getHealth()
      .catch((x: unknown) => x)) as ApiError;
    expect(e.code).toBe('timeout');
  });

  it('rejects a 200 response that is not JSON', async () => {
    const f = vi.fn(async () => new Response('hello', { status: 200 }));
    const e = (await client(f as unknown as typeof fetch)
      .getHealth()
      .catch((x: unknown) => x)) as ApiError;
    expect(e.code).toBe('bad_response');
  });

  it('maps auth statuses to stable codes', async () => {
    for (const [status, code] of [
      [401, 'unauthenticated'],
      [403, 'forbidden'],
      [429, 'rate_limited']
    ] as const) {
      const f = vi.fn(async () => new Response('', { status }));
      const e = (await client(f as unknown as typeof fetch)
        .getHealth()
        .catch((x: unknown) => x)) as ApiError;
      expect(e.code).toBe(code);
    }
  });

  it('sends the CSRF header on mutations only, after the session is known', async () => {
    const calls: { url: string; init?: RequestInit }[] = [];
    const f = vi.fn(async (url: string, init?: RequestInit) => {
      calls.push({ url, init });
      if (url.endsWith('/session'))
        return json({ user: 'u', role: 'standalone', csrfToken: 'tok-1' });
      return json({ jobId: 'job-1' });
    });
    const c = client(f as unknown as typeof fetch);
    await c.getSession();
    await c.reindex({ viewId: 'v', scope: 'changed' });
    const get = calls[0]?.init?.headers as Record<string, string>;
    const post = calls[1]?.init?.headers as Record<string, string>;
    expect(get[CSRF_HEADER]).toBeUndefined();
    expect(post[CSRF_HEADER]).toBe('tok-1');
    expect(calls[1]?.url).toBe('/api/v1/indexes/reindex');
    expect(calls[1]?.init?.credentials).toBe('same-origin');
  });

  it('encodes path segments', async () => {
    const f = vi.fn(async (_url: string) => json({}));
    await client(f as unknown as typeof fetch).getProject('a/b c');
    expect(f.mock.calls[0]?.[0]).toBe('/api/v1/projects/a%2Fb%20c');
  });

  it('reports a missing EventSource instead of throwing', () => {
    const c = new HttpApiClient({
      fetchImpl: vi.fn() as unknown as typeof fetch,
      eventSourceImpl: undefined
    });
    // jsdom has no EventSource, so the constructor leaves it undefined.
    const onError = vi.fn();
    const stop = c.subscribeProgress(() => {}, onError);
    expect(onError).toHaveBeenCalledWith(expect.objectContaining({ code: 'unsupported' }));
    stop();
  });
});
