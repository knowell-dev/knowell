import { describe, expect, it, vi } from 'vitest';
import { ApiError, HttpApiClient, CSRF_HEADER, isUnavailable, newIdempotencyKey } from '$lib/api';
import type { ProgressEvent } from '$lib/api';

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/problem+json' }
  });
}

type Call = { url: string; init: RequestInit };

function setup(respond: (url: string, init: RequestInit) => Response | Promise<Response>) {
  const calls: Call[] = [];
  const f = vi.fn(async (url: string, init: RequestInit = {}) => {
    calls.push({ url, init });
    return respond(url, init);
  });
  const client = new HttpApiClient({ fetchImpl: f as unknown as typeof fetch, timeoutMs: 1000 });
  return { client, calls };
}

const session = {
  user: 'user:018f2b7e-6c1a-7000-8000-000000000001',
  role: 'hub',
  csrfToken: 'tok-9',
  expiresAt: '2030-01-01T00:00:00Z'
};

describe('problem+json from knowell-server', () => {
  it('keeps code, message and requestId; the extra RFC 7807 fields are ignored', async () => {
    const { client } = setup(() =>
      json(
        {
          type: 'urn:knowell:problem:not_found',
          title: 'Not Found',
          status: 404,
          detail: 'the requested resource does not exist',
          code: 'not_found',
          message: 'the requested resource does not exist',
          requestId: '018f-req'
        },
        404
      )
    );
    const e = (await client.getProject('x').catch((x: unknown) => x)) as ApiError;
    expect(e).toBeInstanceOf(ApiError);
    expect([e.code, e.status, e.requestId]).toEqual(['not_found', 404, '018f-req']);
  });

  it('recognises 503 engine_unavailable and keeps the reason', async () => {
    const { client } = setup(() =>
      json(
        { code: 'engine_unavailable', message: 'the engine is not wired yet', requestId: 'r' },
        503
      )
    );
    const e = (await client
      .search({
        query: 'x',
        expandGraph: false,
        rerank: false,
        limit: 5
      })
      .catch((x: unknown) => x)) as ApiError;
    expect(isUnavailable(e)).toBe(true);
    expect(e.message).toBe('the engine is not wired yet');
    expect(isUnavailable(new ApiError('internal_error', 'x', 500))).toBe(false);
  });
});

describe('session, login and logout', () => {
  it('login posts the token without CSRF, then uses the new CSRF token', async () => {
    const { client, calls } = setup((url) =>
      url.endsWith('/session/login') ? json(session) : json({ jobId: 'j', created: true }, 202)
    );
    const s = await client.login('kn_fake_token_value');
    expect(s.expiresAt).toBe('2030-01-01T00:00:00Z');
    expect(calls[0]?.url).toBe('/api/v1/session/login');
    expect(JSON.parse(String(calls[0]?.init.body))).toEqual({ token: 'kn_fake_token_value' });
    await client.reindex({ viewId: 'v', scope: 'full' });
    expect((calls[1]?.init.headers as Record<string, string>)[CSRF_HEADER]).toBe('tok-9');
  });

  it('logout accepts 204 and forgets the CSRF token', async () => {
    const { client, calls } = setup((url) =>
      url.endsWith('/logout') ? new Response(null, { status: 204 }) : json(session)
    );
    await client.getSession();
    await expect(client.logout()).resolves.toBeUndefined();
    expect((calls[1]?.init.headers as Record<string, string>)[CSRF_HEADER]).toBe('tok-9');
    await client.logout();
    expect((calls[2]?.init.headers as Record<string, string>)[CSRF_HEADER]).toBeUndefined();
  });

  it('a 401 after sign-in notifies listeners; before sign-in it does not', async () => {
    let signedIn = false;
    const { client } = setup((url) => {
      if (url.endsWith('/session')) {
        return signedIn
          ? json(session)
          : json({ code: 'unauthenticated', message: 'sign in' }, 401);
      }
      return json({ code: 'unauthenticated', message: 'sign in' }, 401);
    });
    const lost = vi.fn();
    client.onUnauthenticated(lost);
    await client.getSession().catch(() => undefined);
    expect(lost).not.toHaveBeenCalled();
    signedIn = true;
    await client.getSession();
    await client.getHealth().catch(() => undefined);
    expect(lost).toHaveBeenCalledTimes(1);
  });
});

describe('202, 204 and 409 handling', () => {
  it('reindex sends an Idempotency-Key and returns {jobId, created}', async () => {
    const { client, calls } = setup(() => json({ jobId: 'job-1', created: false }, 202));
    const r = await client.reindex({ viewId: 'v1', scope: 'changed' }, 'key-1');
    expect(r).toEqual({ jobId: 'job-1', created: false });
    const headers = calls[0]?.init.headers as Record<string, string>;
    expect(headers['Idempotency-Key']).toBe('key-1');
    await client.reindex({ viewId: 'v1', scope: 'changed' });
    const generated = (calls[1]?.init.headers as Record<string, string>)['Idempotency-Key'];
    expect(generated).toMatch(/^[A-Za-z0-9._:-]{1,128}$/);
    expect(newIdempotencyKey()).not.toBe(newIdempotencyKey());
  });

  it('retry resolves on 204', async () => {
    const { client, calls } = setup(() => new Response(null, { status: 204 }));
    await expect(client.retryDeadLetter('a/b')).resolves.toBeUndefined();
    expect(calls[0]?.url).toBe('/api/v1/jobs/a%2Fb/retry');
  });

  it('retry reports 409 not_dead and 404 as ApiError', async () => {
    const conflict = setup(() =>
      json({ code: 'not_dead', message: 'only dead-lettered jobs can be retried' }, 409)
    );
    const e = (await conflict.client.retryDeadLetter('j').catch((x: unknown) => x)) as ApiError;
    expect([e.code, e.status, e.retryable]).toEqual(['not_dead', 409, false]);
    const missing = setup(() =>
      json({ code: 'not_found', message: 'the job does not exist' }, 404)
    );
    const m = (await missing.client.retryDeadLetter('j').catch((x: unknown) => x)) as ApiError;
    expect([m.code, m.status]).toEqual(['not_found', 404]);
  });

  it('a profile switch returns the 202 engine body, or null when it is empty', async () => {
    const withBody = setup(() => json({ migrationId: 'mig-1' }, 202));
    expect(await withBody.client.startSwitch({ toProfileId: 'p' })).toEqual({
      migrationId: 'mig-1'
    });
    const empty = setup(() => new Response(null, { status: 202 }));
    expect(await empty.client.startSwitch({ toProfileId: 'p' })).toBeNull();
  });

  it('listJobs builds the query string', async () => {
    const { client, calls } = setup(() => json([]));
    await client.listJobs();
    await client.listJobs({ state: 'dead', limit: 25 });
    expect(calls.map((c) => c.url)).toEqual(['/api/v1/jobs', '/api/v1/jobs?state=dead&limit=25']);
  });
});

class FakeEventSource {
  static last: FakeEventSource | undefined;
  onmessage: ((m: MessageEvent<string>) => void) | null = null;
  onerror: (() => void) | null = null;
  onopen: (() => void) | null = null;
  closed = false;
  constructor(
    public url: string,
    public init?: EventSourceInit
  ) {
    FakeEventSource.last = this;
  }
  close() {
    this.closed = true;
  }
  emit(data: string) {
    this.onmessage?.({ data } as MessageEvent<string>);
  }
}

describe('progress stream', () => {
  function stream() {
    const client = new HttpApiClient({
      fetchImpl: vi.fn() as unknown as typeof fetch,
      eventSourceImpl: FakeEventSource as unknown as typeof EventSource
    });
    const events: ProgressEvent[] = [];
    const onError = vi.fn();
    const stop = client.subscribeProgress((e) => events.push(e), onError);
    const es = FakeEventSource.last as FakeEventSource;
    return { events, onError, stop, es };
  }

  it('delivers resync events with the missed count', () => {
    const { events, es } = stream();
    expect(es.url).toBe('/api/v1/events');
    expect(es.init?.withCredentials).toBe(true);
    es.emit('{"type":"resync","missed":7}');
    es.emit('{"type":"heartbeat","at":"1970-01-01T00:00:00Z"}');
    expect(events).toEqual([
      { type: 'resync', missed: 7 },
      { type: 'heartbeat', at: '1970-01-01T00:00:00Z' }
    ]);
  });

  it('ignores unknown event types and reports malformed data', () => {
    const { events, onError, es } = stream();
    es.emit('{"type":"from-the-future"}');
    es.emit('not json');
    expect(events).toEqual([]);
    expect(onError).toHaveBeenCalledWith(expect.objectContaining({ code: 'bad_response' }));
  });

  it('asks for a refetch after the stream reconnects', () => {
    const { events, onError, es, stop } = stream();
    es.onopen?.();
    expect(events).toEqual([]);
    es.onerror?.();
    expect(onError).toHaveBeenCalledWith(expect.objectContaining({ code: 'network' }));
    es.onopen?.();
    expect(events).toEqual([{ type: 'resync', missed: 0 }]);
    stop();
    expect(es.closed).toBe(true);
  });
});
