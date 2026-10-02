import { ApiError, type ApiClient, type Unsubscribe } from './client';
import * as d from './mockData';
import type {
  EmbeddingProfile,
  GraphQuery,
  GraphSlice,
  IndexesOverview,
  Job,
  JobsQuery,
  MemoryDecision,
  MemoryRecord,
  ProgressEvent,
  ReindexRequest,
  SearchRequest,
  SearchResponse,
  SearchResult,
  Session,
  SwitchEstimate,
  SwitchRequest,
  SwitchStarted
} from './types';
import {
  serverHealth,
  serverIndexes,
  serverProject,
  serverProjectSummary,
  serverWorkspace,
  serverWorkspaceDetail
} from './mockServerShape';

export interface MockOptions {
  /** Simulated latency per call. Tests pass 0. */
  latencyMs?: number;
  role?: 'standalone' | 'hub';
  /** Method names that should fail with the given error (for error-state tests and demos). */
  failures?: Partial<Record<keyof ApiClient, ApiError>>;
  /** Make list calls return empty data to show empty states. */
  empty?: boolean;
  /**
   * `rich` (default) fills every field for a good demo; `server` reports `null` wherever the real
   * server cannot know the value yet, exactly like `knowell-server` does today.
   */
  shape?: 'rich' | 'server';
  /** Without organization-wide read access `jobs` and `deadLetters` are `null`. Default true. */
  orgWide?: boolean;
  /** Pretend the engine is not wired: delegated routes answer 503 `engine_unavailable`. */
  engineUnavailable?: string;
  /** `GET /session` answers 401 until `login` is called with a token (`kn_...`). */
  requireLogin?: boolean;
}

/** Routes the real server delegates to the engine (503 `engine_unavailable` without one). */
const DELEGATED: ReadonlySet<keyof ApiClient> = new Set([
  'search',
  'getGraph',
  'getGraphInsights',
  'listDomains',
  'listGlossary',
  'listMemory',
  'decideMemory',
  'listTasks',
  'listRules',
  'listProfiles',
  'estimateSwitch',
  'startSwitch',
  'listEvalReports',
  'getUsage',
  'getIntegrations'
]);

const tokens = (s: string): string[] =>
  s
    .toLowerCase()
    .split(/[^a-z0-9_]+/)
    .filter((t) => t.length > 1);

/** Deterministic pseudo-similarity in [0.2, 0.9] from two strings (no RNG: stable in tests). */
function pseudoSimilarity(a: string, b: string): number {
  let h = 2166136261;
  for (const ch of a + '|' + b) h = Math.imul(h ^ ch.charCodeAt(0), 16777619) >>> 0;
  return 0.2 + (h % 700) / 1000;
}

export class MockApiClient implements ApiClient {
  private readonly opts: Required<Pick<MockOptions, 'latencyMs' | 'role'>> & MockOptions;
  private memoryRecords: MemoryRecord[] = d.memory();
  private idx: IndexesOverview = d.indexes();
  private profs: EmbeddingProfile[] = d.profiles();
  private jobSeq = 5000;
  private loggedIn: boolean;
  private readonly keys = new Map<string, string>();
  private readonly authLost = new Set<() => void>();

  constructor(opts: MockOptions = {}) {
    this.opts = { latencyMs: 150, role: 'standalone', ...opts };
    this.loggedIn = !this.opts.requireLogin;
    if (this.opts.shape === 'server') this.idx = serverIndexes(this.idx, this.orgWide);
  }

  private get orgWide(): boolean {
    return this.opts.orgWide !== false;
  }
  private get server(): boolean {
    return this.opts.shape === 'server';
  }
  private jobs(): Job[] {
    return this.idx.jobs ?? [];
  }

  private async call<T>(name: keyof ApiClient, produce: () => T): Promise<T> {
    if (this.opts.latencyMs > 0) await new Promise((r) => setTimeout(r, this.opts.latencyMs));
    if (!this.loggedIn && name !== 'getSession' && name !== 'login') {
      for (const h of this.authLost) h();
      throw new ApiError('unauthenticated', 'sign in with an api token', 401);
    }
    const fail = this.opts.failures?.[name];
    if (fail) throw fail;
    if (this.opts.engineUnavailable !== undefined && DELEGATED.has(name)) {
      throw new ApiError('engine_unavailable', this.opts.engineUnavailable, 503);
    }
    return structuredClone(produce());
  }

  private session(): Session {
    return {
      user: 'user:018f2b7e-6c1a-7000-8000-000000000001',
      role: this.opts.role,
      csrfToken: 'mock-csrf-token',
      expiresAt: new Date(Date.now() + 7 * 24 * 3600_000).toISOString()
    };
  }

  getSession = () =>
    this.call('getSession', () => {
      if (!this.loggedIn) {
        throw new ApiError('unauthenticated', 'sign in with an api token', 401);
      }
      return this.session();
    });
  login = (token: string) =>
    this.call('login', () => {
      if (!/^kn_\S+$/.test(token)) {
        throw new ApiError('invalid_token', 'the api token is not valid', 401);
      }
      this.loggedIn = true;
      return this.session();
    });
  logout = async () => {
    await this.call('logout', () => undefined);
    if (this.opts.requireLogin) this.loggedIn = false;
  };
  onUnauthenticated(handler: () => void): Unsubscribe {
    this.authLost.add(handler);
    return () => this.authLost.delete(handler);
  }
  getHealth = () =>
    this.call('getHealth', () => (this.server ? serverHealth(d.health()) : d.health()));

  listWorkspaces = () =>
    this.call('listWorkspaces', () =>
      this.opts.empty
        ? []
        : d
            .workspaceList()
            .map(({ projectIds: _p, members: _m, ...s }, i) =>
              this.server ? serverWorkspace(s, i) : s
            )
    );
  getWorkspace = (id: string) =>
    this.call('getWorkspace', () => {
      const all = d.workspaceList();
      const i = all.findIndex((x) => x.id === id);
      const w = all[i];
      if (!w) throw new ApiError('not_found', `workspace ${id} does not exist`, 404);
      return this.server ? serverWorkspaceDetail(w, i) : w;
    });
  listProjects = (workspaceId?: string) =>
    this.call('listProjects', () => {
      if (this.opts.empty) return [];
      const list = d
        .projectSummaries()
        .filter((p) => !workspaceId || p.workspaceId === workspaceId);
      return this.server ? list.map(serverProjectSummary) : list;
    });
  getProject = (id: string) =>
    this.call('getProject', () => {
      const p = d.projectDetail(id);
      if (!p) throw new ApiError('not_found', `project ${id} does not exist`, 404);
      return this.server ? serverProject(p) : p;
    });

  getIndexes = () =>
    this.call('getIndexes', (): IndexesOverview => {
      if (this.opts.empty) {
        return {
          views: [],
          jobs: this.orgWide ? [] : null,
          deadLetters: this.orgWide ? [] : null,
          migrations: this.server ? null : []
        };
      }
      return this.idx;
    });
  reindex = (req: ReindexRequest, idempotencyKey?: string) =>
    this.call('reindex', () => {
      const view = this.idx.views.find((v) => v.id === req.viewId);
      if (!view) throw new ApiError('not_found', 'the view does not exist', 404);
      const known = idempotencyKey ? this.keys.get(idempotencyKey) : undefined;
      if (known) return { jobId: known, created: false };
      const id = `job-${++this.jobSeq}`;
      if (idempotencyKey) this.keys.set(idempotencyKey, id);
      const now = new Date().toISOString();
      this.idx.jobs?.unshift({
        id,
        kind: 'view.reindex',
        state: 'queued',
        projectName: view.projectName,
        workspaceName: view.workspaceName,
        attempts: 0,
        maxAttempts: 5,
        enqueuedAt: now,
        updatedAt: now,
        progress: null,
        error: null
      });
      return { jobId: id, created: true };
    });
  retryDeadLetter = (jobId: string) =>
    this.call('retryDeadLetter', () => {
      const job = this.jobs().find((j) => j.id === jobId);
      if (!job) throw new ApiError('not_found', 'the job does not exist', 404);
      if (job.state !== 'dead') {
        throw new ApiError('not_dead', 'only dead-lettered jobs can be retried', 409);
      }
      if (this.idx.deadLetters) {
        this.idx.deadLetters = this.idx.deadLetters.filter((x) => x.jobId !== jobId);
      }
      job.state = 'queued';
      job.attempts = 0;
      job.error = null;
      job.updatedAt = new Date().toISOString();
    });
  listJobs = (q: JobsQuery = {}) =>
    this.call('listJobs', () => {
      if (!this.orgWide) {
        throw new ApiError('forbidden', 'organization-wide read access is required', 403);
      }
      return this.jobs()
        .filter((j) => !q.state || j.state === q.state)
        .slice(0, q.limit ?? 200);
    });

  subscribeProgress(onEvent: (e: ProgressEvent) => void): Unsubscribe {
    let p = 0.42;
    const t = setInterval(() => {
      p = p >= 1 ? 0.05 : Math.min(1, p + 0.04);
      const job = this.jobs().find((j) => j.state === 'running');
      if (job) onEvent({ type: 'job', job: { ...job, progress: p } });
      onEvent({ type: 'heartbeat', at: new Date().toISOString() });
    }, 2000);
    return () => clearInterval(t);
  }

  search = (req: SearchRequest) =>
    this.call('search', (): SearchResponse => {
      const q = tokens(req.query);
      if (q.length === 0) {
        return {
          queryClass: 'behavior',
          results: [],
          tookMs: 1,
          skipped: [],
          tokensReturned: 0,
          emptyReason: { code: 'empty_query', message: 'enter a query to search the workspace' }
        };
      }
      const skipped: SearchResponse['skipped'] = [];
      const unindexed = d.projectSummaries().filter((p) => !p.indexed);
      for (const p of unindexed) {
        if (!req.projectIds || req.projectIds.length === 0 || req.projectIds.includes(p.id)) {
          skipped.push({ projectName: p.name, reason: 'project not indexed yet' });
        }
      }
      const pool = d.corpus.filter(
        (r) =>
          (!req.projectIds?.length || req.projectIds.includes(r.projectId)) &&
          (!req.languages?.length || req.languages.includes(r.language)) &&
          (!req.pathPrefix || r.path.startsWith(req.pathPrefix))
      );
      const scored: SearchResult[] = [];
      for (const r of pool) {
        const hay = tokens(`${r.symbol ?? ''} ${r.path} ${r.snippet}`);
        const hits = q.filter((t) => hay.some((h) => h.includes(t)));
        const vec = pseudoSimilarity(req.query, r.id);
        // Semantic-only candidates need similarity above a threshold, as a real engine would.
        if (hits.length === 0 && vec < 0.78) continue;
        const bm25 = hits.length ? Math.min(1, hits.length / q.length) * 0.9 : null;
        const exact = r.symbol ? q.some((t) => r.symbol?.toLowerCase().includes(t)) : false;
        const graph =
          req.expandGraph && r.id === 'c9' ? 0.4 : req.expandGraph && hits.length > 0 ? 0.12 : null;
        const base = (bm25 ?? 0) * 0.5 + vec * 0.4 + (graph ?? 0) * 0.1;
        const rerank = req.rerank ? Math.min(1, base + (exact ? 0.15 : 0)) : null;
        const reasons: SearchResult['reasons'] = [];
        if (exact && r.symbol) reasons.push({ type: 'exact-symbol', symbol: r.symbol });
        if (hits.length) reasons.push({ type: 'lexical', terms: hits });
        if (vec > 0.5) reasons.push({ type: 'semantic', similarity: Number(vec.toFixed(2)) });
        if (r.id === 'c9' && req.expandGraph) {
          reasons.push({ type: 'test-reference', target: 'Service.PlaceOrder' });
          reasons.push({
            type: 'graph',
            path: ['Service.PlaceOrder', 'tested-by', 'TestPlaceOrderPublishesEvent'],
            edge: 'syntactic'
          });
        }
        scored.push({
          ...r,
          score: {
            fused: Number((rerank ?? base).toFixed(3)),
            bm25: bm25 === null ? null : Number(bm25.toFixed(3)),
            vector: Number(vec.toFixed(3)),
            graph,
            rerank: rerank === null ? null : Number(rerank.toFixed(3))
          },
          reasons
        });
      }
      scored.sort((a, b) => b.score.fused - a.score.fused || a.id.localeCompare(b.id));
      const results = scored.slice(0, Math.max(1, req.limit));
      const emptyReason =
        results.length === 0
          ? pool.length === 0
            ? {
                code: 'no_candidates',
                message: 'no indexed files match the selected project, language and path filters'
              }
            : {
                code: 'no_match',
                message: 'no lexical match and no semantic candidate above the similarity threshold'
              }
          : undefined;
      return {
        queryClass:
          q.some((t) => /^[a-z]+_[a-z_]+$/.test(t)) || /[A-Z][a-z]+[A-Z]/.test(req.query)
            ? 'exact-symbol'
            : req.query.includes('/v1/')
              ? 'endpoint'
              : req.query.toLowerCase().startsWith('why')
                ? 'why'
                : 'behavior',
        results,
        tookMs: 18 + results.length * 3,
        emptyReason,
        skipped,
        tokensReturned: results.reduce((s, r) => s + Math.ceil(r.snippet.length / 4), 0)
      };
    });

  getGraph = (q: GraphQuery) =>
    this.call('getGraph', (): GraphSlice => {
      if (this.opts.empty) return { nodes: [], edges: [], truncated: false };
      if (q.mode === 'contracts') {
        const kinds = new Set([
          'service',
          'endpoint',
          'event',
          'table',
          'env',
          'i18n-key',
          'package'
        ]);
        const nodes = d.graphNodes.filter((x) => kinds.has(x.kind));
        const ids = new Set(nodes.map((x) => x.id));
        const edges = d.graphEdges.filter(
          (x) =>
            !x.id.startsWith('svc-') && !x.id.startsWith('h') && ids.has(x.from) && ids.has(x.to)
        );
        return { nodes, edges, truncated: false };
      }
      const nodes = d.graphNodes.filter(
        (x) =>
          (x.parentId ?? undefined) === q.parentId &&
          ['service', 'module', 'symbol'].includes(x.kind)
      );
      const ids = new Set(nodes.map((x) => x.id));
      const edges = d.graphEdges.filter(
        (x) =>
          ids.has(x.from) &&
          ids.has(x.to) &&
          (q.parentId ? !x.id.startsWith('svc-') && !x.id.startsWith('x') : x.id.startsWith('svc-'))
      );
      return { nodes, edges, truncated: false };
    });
  getGraphInsights = () => this.call('getGraphInsights', () => (this.opts.empty ? [] : d.insights));

  listDomains = () => this.call('listDomains', () => (this.opts.empty ? [] : d.domains));
  listGlossary = () => this.call('listGlossary', () => (this.opts.empty ? [] : d.glossary));

  listMemory = () => this.call('listMemory', () => (this.opts.empty ? [] : this.memoryRecords));
  decideMemory = (dec: MemoryDecision) =>
    this.call('decideMemory', () => {
      const rec = this.memoryRecords.find((m) => m.id === dec.id);
      if (!rec) throw new ApiError('not_found', `memory record ${dec.id} does not exist`, 404);
      if (rec.state !== 'proposed' && rec.state !== 'stale') {
        throw new ApiError('invalid_state', `a ${rec.state} record cannot be ${dec.action}ed`, 409);
      }
      rec.state = dec.action === 'accept' ? 'accepted' : 'rejected';
      rec.version += 1;
      delete rec.staleReason;
      return rec;
    });
  listTasks = () => this.call('listTasks', () => (this.opts.empty ? [] : d.tasks));
  listRules = () => this.call('listRules', () => (this.opts.empty ? [] : d.rules));

  listProfiles = () => this.call('listProfiles', () => this.profs);
  estimateSwitch = (toProfileId: string) =>
    this.call('estimateSwitch', (): SwitchEstimate => {
      const to = this.profs.find((p) => p.id === toProfileId);
      const from = this.profs.find((p) => p.active);
      if (!to || !from)
        throw new ApiError('not_found', `profile ${toProfileId} does not exist`, 404);
      const chunks = d.projectSummaries().reduce((s, p) => s + (p.fileCount ?? 0) * 6, 0);
      const reduce =
        to.dimensions < from.dimensions && to.provider === from.provider && to.model === from.model;
      const free = to.locality === 'local-only';
      const tokens = reduce ? 0 : chunks * 520;
      const warnings: string[] = [];
      if (!to.measured)
        warnings.push(
          'this profile has no measured quality; it will be measured before activation'
        );
      if (to.locality === 'local-only' && from.locality !== 'local-only')
        warnings.push('switching to a local profile changes which hardware does the work');
      if (to.dimensions !== from.dimensions)
        warnings.push(
          'cross-project similarity stays valid only after every project is regenerated'
        );
      return {
        fromProfileId: from.id,
        toProfileId: to.id,
        affectedProjects: d
          .projectSummaries()
          .filter((p) => p.indexed)
          .map((p) => p.name),
        chunksToRegenerate: reduce ? 0 : chunks,
        needsReembedding: !reduce,
        estimatedTokens: tokens,
        estimatedCostUsdMicros: free || reduce ? 0 : Math.round(tokens * 0.15),
        estimatedDiskBytes: chunks * to.dimensions * (to.storage === 'halfvec' ? 2 : 4),
        estimatedDurationMs: reduce ? 4 * 60_000 : Math.round(chunks / 40) * 1000,
        warnings
      };
    });
  startSwitch = (req: SwitchRequest) =>
    this.call('startSwitch', (): SwitchStarted => {
      const to = this.profs.find((p) => p.id === req.toProfileId);
      if (!to) throw new ApiError('not_found', `profile ${req.toProfileId} does not exist`, 404);
      if (to.active) throw new ApiError('invalid_state', 'this profile is already active', 409);
      to.switch = { state: 'building', progress: 0 };
      return { toProfileId: to.id, state: 'building' };
    });

  listEvalReports = () =>
    this.call('listEvalReports', () => (this.opts.empty ? [] : d.evalReports()));
  getUsage = (days: number) =>
    this.call('getUsage', () => d.usage(Math.max(1, Math.min(90, Math.trunc(days)))));
  getIntegrations = () => this.call('getIntegrations', () => d.integrations);
  getAdmin = () => this.call('getAdmin', () => d.admin(this.opts.role));
}
