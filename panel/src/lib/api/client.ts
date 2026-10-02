import type {
  AdminOverview,
  ApiErrorBody,
  ArchRule,
  Domain,
  EmbeddingProfile,
  EngineHealth,
  EvalReport,
  GlossaryTerm,
  GraphInsight,
  GraphQuery,
  GraphSlice,
  IndexesOverview,
  IntegrationsStatus,
  JobsQuery,
  Job,
  MemoryDecision,
  MemoryRecord,
  ProgressEvent,
  ProjectDetail,
  ProjectSummary,
  ReindexRequest,
  ReindexResult,
  SearchRequest,
  SearchResponse,
  Session,
  SwitchEstimate,
  SwitchRequest,
  SwitchStarted,
  TaskRecord,
  UsageReport,
  WorkspaceDetail,
  WorkspaceSummary
} from './types';

/** Error raised by every ApiClient implementation. Messages never contain response bodies. */
export class ApiError extends Error {
  readonly code: string;
  readonly status: number;
  readonly requestId?: string;

  constructor(code: string, message: string, status = 0, requestId?: string) {
    super(message);
    this.name = 'ApiError';
    this.code = code;
    this.status = status;
    this.requestId = requestId;
  }

  /** True when retrying the same request later may succeed. */
  get retryable(): boolean {
    return this.status === 0 || this.status === 429 || this.status >= 500;
  }
}

/** Problem codes that mean "this part of the server is not wired", not "something broke". */
export const UNAVAILABLE_CODES = ['engine_unavailable', 'store_unavailable', 'not_initialized'];

export function isUnavailable(e: ApiError | undefined): boolean {
  return e !== undefined && e.status === 503 && UNAVAILABLE_CODES.includes(e.code);
}

export type Unsubscribe = () => void;

/** Everything the panel needs from the engine. Implemented over HTTP and by a mock. */
export interface ApiClient {
  getSession(): Promise<Session>;
  /** Hub role: open a panel session with a user's API token (never stored by the panel). */
  login(token: string): Promise<Session>;
  logout(): Promise<void>;
  /** Called when a request answers 401 after the session was established (expired or revoked). */
  onUnauthenticated(handler: () => void): Unsubscribe;
  getHealth(): Promise<EngineHealth>;

  listWorkspaces(): Promise<WorkspaceSummary[]>;
  getWorkspace(id: string): Promise<WorkspaceDetail>;
  listProjects(workspaceId?: string): Promise<ProjectSummary[]>;
  getProject(id: string): Promise<ProjectDetail>;

  getIndexes(): Promise<IndexesOverview>;
  /** `idempotencyKey` is sent as `Idempotency-Key`; a fresh one is generated when omitted. */
  reindex(req: ReindexRequest, idempotencyKey?: string): Promise<ReindexResult>;
  /** Resolves on 204. Rejects with `not_dead` (409) or `not_found` (404). */
  retryDeadLetter(jobId: string): Promise<void>;
  listJobs(q?: JobsQuery): Promise<Job[]>;
  /** Progress stream (SSE). Returns an unsubscribe function. */
  subscribeProgress(
    onEvent: (e: ProgressEvent) => void,
    onError?: (e: ApiError) => void
  ): Unsubscribe;

  search(req: SearchRequest): Promise<SearchResponse>;

  getGraph(q: GraphQuery): Promise<GraphSlice>;
  getGraphInsights(): Promise<GraphInsight[]>;

  listDomains(): Promise<Domain[]>;
  listGlossary(): Promise<GlossaryTerm[]>;

  listMemory(): Promise<MemoryRecord[]>;
  decideMemory(d: MemoryDecision): Promise<MemoryRecord>;
  listTasks(): Promise<TaskRecord[]>;
  listRules(): Promise<ArchRule[]>;

  listProfiles(): Promise<EmbeddingProfile[]>;
  estimateSwitch(toProfileId: string): Promise<SwitchEstimate>;
  /** 202: the engine starts the switch and may return a body. */
  startSwitch(req: SwitchRequest): Promise<SwitchStarted>;

  listEvalReports(): Promise<EvalReport[]>;
  getUsage(days: number): Promise<UsageReport>;
  getIntegrations(): Promise<IntegrationsStatus>;
  getAdmin(): Promise<AdminOverview>;
}

// -------------------------------------------------------------------------------------------

export const CSRF_HEADER = 'X-Knowell-CSRF';
export const API_BASE = '/api/v1';

export interface HttpClientOptions {
  baseUrl?: string;
  fetchImpl?: typeof fetch;
  eventSourceImpl?: typeof EventSource;
  timeoutMs?: number;
}

function isErrorBody(v: unknown): v is ApiErrorBody {
  return (
    typeof v === 'object' &&
    v !== null &&
    typeof (v as ApiErrorBody).code === 'string' &&
    typeof (v as ApiErrorBody).message === 'string'
  );
}

/** Talks to `knowell-server` under `/api/v1`. */
export class HttpApiClient implements ApiClient {
  private readonly base: string;
  private readonly fetchImpl: typeof fetch;
  private readonly esImpl: typeof EventSource | undefined;
  private readonly timeoutMs: number;
  // Handed out by `GET /session` and `POST /session/login`; echoed on mutations.
  private csrfToken: string | null = null;
  private sessionOpen = false;
  private readonly authLost = new Set<() => void>();

  constructor(opts: HttpClientOptions = {}) {
    this.base = opts.baseUrl ?? API_BASE;
    this.fetchImpl = opts.fetchImpl ?? ((...a) => globalThis.fetch(...a));
    this.esImpl = opts.eventSourceImpl ?? globalThis.EventSource;
    this.timeoutMs = opts.timeoutMs ?? 15_000;
  }

  setCsrfToken(token: string | null): void {
    this.csrfToken = token;
  }

  onUnauthenticated(handler: () => void): Unsubscribe {
    this.authLost.add(handler);
    return () => this.authLost.delete(handler);
  }

  private async request<T>(
    method: 'GET' | 'POST',
    path: string,
    body?: unknown,
    extraHeaders: Record<string, string> = {},
    emptyOk = false
  ): Promise<T> {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), this.timeoutMs);
    const headers: Record<string, string> = { Accept: 'application/json', ...extraHeaders };
    if (method !== 'GET') {
      headers['Content-Type'] = 'application/json';
      if (this.csrfToken) headers[CSRF_HEADER] = this.csrfToken;
    }
    let res: Response;
    try {
      res = await this.fetchImpl(`${this.base}${path}`, {
        method,
        headers,
        credentials: 'same-origin',
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: ctrl.signal
      });
    } catch (e) {
      if (ctrl.signal.aborted) {
        throw new ApiError('timeout', `the engine did not answer within ${this.timeoutMs} ms`);
      }
      void e;
      throw new ApiError('network', 'cannot reach the engine; is `know` still running?');
    } finally {
      clearTimeout(timer);
    }

    let payload: unknown = undefined;
    const text = await res.text().catch(() => '');
    if (text.length > 0) {
      try {
        payload = JSON.parse(text);
      } catch {
        if (res.ok) {
          throw new ApiError(
            'bad_response',
            'the engine returned a response that is not JSON',
            res.status
          );
        }
      }
    }

    if (res.status === 401 && this.sessionOpen && !path.startsWith('/session')) {
      this.sessionOpen = false;
      for (const h of this.authLost) h();
    }
    if (!res.ok) {
      if (isErrorBody(payload)) {
        throw new ApiError(payload.code, payload.message, res.status, payload.requestId);
      }
      throw new ApiError(statusCode(res.status), statusMessage(res.status), res.status);
    }
    if (payload === undefined && res.status !== 204 && !emptyOk) {
      throw new ApiError('bad_response', 'the engine returned an empty response', res.status);
    }
    return payload as T;
  }

  private get<T>(path: string): Promise<T> {
    return this.request<T>('GET', path);
  }
  private post<T>(path: string, body?: unknown, headers?: Record<string, string>): Promise<T> {
    return this.request<T>('POST', path, body ?? {}, headers);
  }

  private adopt(s: Session): Session {
    this.csrfToken = s.csrfToken;
    this.sessionOpen = true;
    return s;
  }

  async getSession(): Promise<Session> {
    return this.adopt(await this.get<Session>('/session'));
  }
  async login(token: string): Promise<Session> {
    return this.adopt(await this.post<Session>('/session/login', { token }));
  }
  async logout(): Promise<void> {
    try {
      await this.request<void>('POST', '/session/logout', {}, {}, true);
    } finally {
      this.csrfToken = null;
      this.sessionOpen = false;
    }
  }
  getHealth = () => this.get<EngineHealth>('/health');

  listWorkspaces = () => this.get<WorkspaceSummary[]>('/workspaces');
  getWorkspace = (id: string) => this.get<WorkspaceDetail>(`/workspaces/${encodeURIComponent(id)}`);
  listProjects = (workspaceId?: string) =>
    this.get<ProjectSummary[]>(
      workspaceId ? `/projects?workspace=${encodeURIComponent(workspaceId)}` : '/projects'
    );
  getProject = (id: string) => this.get<ProjectDetail>(`/projects/${encodeURIComponent(id)}`);

  getIndexes = () => this.get<IndexesOverview>('/indexes');
  reindex = (req: ReindexRequest, idempotencyKey?: string) =>
    this.post<ReindexResult>('/indexes/reindex', req, {
      'Idempotency-Key': idempotencyKey ?? newIdempotencyKey()
    });
  retryDeadLetter = (jobId: string) => this.post<void>(`/jobs/${encodeURIComponent(jobId)}/retry`);
  listJobs = (q: JobsQuery = {}) => {
    const p = new URLSearchParams();
    if (q.state) p.set('state', q.state);
    if (q.limit !== undefined) p.set('limit', String(Math.trunc(q.limit)));
    const qs = p.toString();
    return this.get<Job[]>(qs ? `/jobs?${qs}` : '/jobs');
  };

  subscribeProgress(
    onEvent: (e: ProgressEvent) => void,
    onError?: (e: ApiError) => void
  ): Unsubscribe {
    if (!this.esImpl) {
      onError?.(new ApiError('unsupported', 'this browser has no EventSource support'));
      return () => {};
    }
    const es = new this.esImpl(`${this.base}/events`, { withCredentials: true });
    let interrupted = false;
    es.onmessage = (m: MessageEvent<string>) => {
      let ev: unknown;
      try {
        ev = JSON.parse(m.data);
      } catch {
        onError?.(new ApiError('bad_response', 'malformed progress event'));
        return;
      }
      // Unknown event types are ignored so a newer server does not break an older panel.
      if (isProgressEvent(ev)) onEvent(ev);
    };
    // EventSource reconnects by itself; events sent meanwhile are lost, so ask for a refetch.
    es.onopen = () => {
      if (interrupted) {
        interrupted = false;
        onEvent({ type: 'resync', missed: 0 });
      }
    };
    es.onerror = () => {
      interrupted = true;
      onError?.(new ApiError('network', 'progress stream interrupted; reconnecting'));
    };
    return () => es.close();
  }

  search = (req: SearchRequest) => this.post<SearchResponse>('/search', req);

  getGraph(q: GraphQuery): Promise<GraphSlice> {
    const p = new URLSearchParams({ mode: q.mode });
    if (q.parentId) p.set('parent', q.parentId);
    return this.get<GraphSlice>(`/graph?${p.toString()}`);
  }
  getGraphInsights = () => this.get<GraphInsight[]>('/graph/insights');

  listDomains = () => this.get<Domain[]>('/domains');
  listGlossary = () => this.get<GlossaryTerm[]>('/glossary');

  listMemory = () => this.get<MemoryRecord[]>('/memory');
  decideMemory = (d: MemoryDecision) =>
    this.post<MemoryRecord>(`/memory/${encodeURIComponent(d.id)}/decision`, d);
  listTasks = () => this.get<TaskRecord[]>('/tasks');
  listRules = () => this.get<ArchRule[]>('/rules');

  listProfiles = () => this.get<EmbeddingProfile[]>('/profiles');
  estimateSwitch = (toProfileId: string) =>
    this.get<SwitchEstimate>(`/profiles/${encodeURIComponent(toProfileId)}/switch-estimate`);
  startSwitch = async (req: SwitchRequest): Promise<SwitchStarted> =>
    (await this.request<SwitchStarted | undefined>('POST', '/profiles/switch', req, {}, true)) ??
    null;

  listEvalReports = () => this.get<EvalReport[]>('/quality/reports');
  getUsage = (days: number) => this.get<UsageReport>(`/usage?days=${Math.trunc(days)}`);
  getIntegrations = () => this.get<IntegrationsStatus>('/integrations');
  getAdmin = () => this.get<AdminOverview>('/admin');
}

const EVENT_TYPES = ['job', 'generation', 'heartbeat', 'resync'];

function isProgressEvent(v: unknown): v is ProgressEvent {
  return (
    typeof v === 'object' &&
    v !== null &&
    EVENT_TYPES.includes((v as { type?: unknown }).type as string)
  );
}

/** Key of 1-128 characters of [A-Za-z0-9._:-], as the server requires. */
export function newIdempotencyKey(): string {
  const c = globalThis.crypto;
  if (c && typeof c.randomUUID === 'function') return c.randomUUID();
  return `k-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

function statusCode(status: number): string {
  if (status === 401) return 'unauthenticated';
  if (status === 403) return 'forbidden';
  if (status === 404) return 'not_found';
  if (status === 429) return 'rate_limited';
  if (status >= 500) return 'server_error';
  return 'http_error';
}

function statusMessage(status: number): string {
  switch (status) {
    case 401:
      return 'the session is not authenticated; sign in again';
    case 403:
      return 'this action is not permitted for the current role';
    case 404:
      return 'the requested item does not exist';
    case 429:
      return 'too many requests; wait a moment and retry';
    default:
      return status >= 500
        ? 'the engine reported an internal error'
        : `the request was rejected (http ${status})`;
  }
}
