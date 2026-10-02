import { describe, expect, it } from 'vitest';
import { ApiError } from '$lib/api';
import { MockApiClient } from '$lib/api/mock';

const mk = (o = {}) => new MockApiClient({ latencyMs: 0, ...o });

describe('MockApiClient', () => {
  it('keeps the three commits of a view separate', async () => {
    const { views } = await mk().getIndexes();
    const behind = views.find((v) => v.lastSeenCommit !== v.activeIndexCommit);
    expect(behind).toBeDefined();
    expect(behind?.trackTarget.name).toBeTruthy();
  });

  it('reports project settings with their origin', async () => {
    const p = await mk().getProject('p-orders');
    expect(p.trackedRef?.origin).toBe('project');
    expect(p.embeddingProfileId?.origin).toBe('workspace');
    expect(p.root.origin).toBe('builtin');
  });

  it('shows search score signals that did not run as null, not zero', async () => {
    const r = await mk().search({
      query: 'PlaceOrder',
      expandGraph: false,
      rerank: false,
      limit: 5
    });
    expect(r.results.length).toBeGreaterThan(0);
    expect(r.results[0]?.score.rerank).toBeNull();
    expect(r.results[0]?.score.graph).toBeNull();
  });

  it('explains why a search is empty or partial', async () => {
    const empty = await mk().search({ query: '   ', expandGraph: false, rerank: false, limit: 5 });
    expect(empty.emptyReason?.code).toBe('empty_query');
    const infra = await mk().search({
      query: 'terraform',
      projectIds: ['p-infra'],
      expandGraph: false,
      rerank: false,
      limit: 5
    });
    expect(infra.skipped[0]?.reason).toMatch(/not indexed/);
    expect(infra.emptyReason).toBeDefined();
  });

  it('applies memory decisions and rejects invalid transitions', async () => {
    const c = mk();
    const rec = await c.decideMemory({ id: 'm3', action: 'accept' });
    expect(rec.state).toBe('accepted');
    await expect(c.decideMemory({ id: 'm3', action: 'reject' })).rejects.toMatchObject({
      code: 'invalid_state'
    });
    await expect(c.decideMemory({ id: 'nope', action: 'accept' })).rejects.toMatchObject({
      code: 'not_found'
    });
  });

  it('never exposes a secret value, only references', async () => {
    const profiles = await mk().listProfiles();
    for (const p of profiles) if (p.apiKey) expect(p.apiKey).toMatch(/^(env|file|keyring):/);
    expect(JSON.stringify(profiles)).not.toMatch(/AIza/);
  });

  it('estimates a switch before doing anything', async () => {
    const c = mk();
    const e = await c.estimateSwitch('prof-gemini-1536');
    expect(e.needsReembedding).toBe(true);
    expect(e.estimatedCostUsdMicros).toBeGreaterThan(0);
    const local = await c.estimateSwitch('prof-local-bge');
    expect(local.estimatedCostUsdMicros).toBe(0);
  });

  it('injects failures per method', async () => {
    const c = mk({ failures: { getHealth: new ApiError('server_error', 'boom', 500) } });
    await expect(c.getHealth()).rejects.toMatchObject({ code: 'server_error' });
    await expect(c.getIndexes()).resolves.toBeDefined();
  });

  it('admin is unavailable outside the hub role', async () => {
    expect((await mk().getAdmin()).available).toBe(false);
    expect((await mk({ role: 'hub' }).getAdmin()).users.length).toBeGreaterThan(0);
  });
});
