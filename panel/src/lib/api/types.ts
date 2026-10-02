/**
 * REST API types for the Knowell panel.
 *
 * TODO(codegen): these types are hand-written for now. They will be generated from the Rust
 * structs (ts-rs / specta) and this file will be replaced by the generated output. Keep every
 * wire type in this single file so the swap is mechanical. Conventions: timestamps are ISO-8601
 * UTC strings, durations are milliseconds, sizes are bytes, money is USD micro-units only where
 * the field name says so, scores are unitless floats.
 *
 * Secrets never appear here: credentials are always a `SecretRef` (a reference such as
 * `env:GEMINI_API_KEY`), never a value.
 */

// ---------------------------------------------------------------------------------------------
// Common
// ---------------------------------------------------------------------------------------------

export type IsoTime = string;

/** Where an effective setting value comes from. */
export type Origin = 'builtin' | 'workspace' | 'project';

/** A setting together with the layer that provides it. */
export interface Setting<T> {
  value: T;
  origin: Origin;
  /** Human readable pointer, e.g. `workspace "shopfront"` or `project.toml`. */
  originNote?: string;
}

/** Reference to a secret. The value is never sent to the panel. */
export type SecretRef = `env:${string}` | `file:${string}` | `keyring:${string}`;

export type Role = 'standalone' | 'hub' | 'worker' | 'edge';
export type HealthStatus = 'ok' | 'degraded' | 'down';

/** Freshness tiers (ARCHITECTURE 6.4): T0 text, T1 symbols, T2 embeddings, T3 relations. */
export type FreshnessTier = 'T0' | 'T1' | 'T2' | 'T3';

export interface ApiErrorBody {
  /** Stable machine code, e.g. `ref_not_found`. */
  code: string;
  /** Lowercase, actionable, free of secret values. */
  message: string;
  requestId?: string;
}

// ---------------------------------------------------------------------------------------------
// Overview / health
// ---------------------------------------------------------------------------------------------

export interface ComponentHealth {
  name: string;
  status: HealthStatus;
  detail: string;
}

export interface QueueStats {
  queued: number;
  running: number;
  failed: number;
  deadLetter: number;
  /** Oldest queued job age. */
  /** `null` when nothing is queued. */
  oldestQueuedMs: number | null;
}

export interface TierFreshness {
  tier: FreshnessTier;
  /** Share of files (0..1) whose current content is covered by this tier. */
  coverage: number;
  /** Median lag from change to availability. */
  medianLagMs: number;
}

export interface ErrorEntry {
  at: IsoTime;
  code: string;
  message: string;
  projectId?: string;
  jobId?: string;
}

export interface ResourceUse {
  cpuPercent: number;
  memoryBytes: number;
  memoryLimitBytes: number;
  diskIndexBytesEstimated: number;
  diskIndexBytesActual: number;
  diskLimitBytes: number;
  openConnections: number;
}

export interface EngineHealth {
  status: HealthStatus;
  version: string;
  role: Role;
  uptimeMs: number;
  bindAddress: string;
  components: ComponentHealth[];
  /** `null` without a database. */
  queue: QueueStats | null;
  /** `null` unless the engine reports it. */
  freshness: TierFreshness[] | null;
  /** `null` unless the engine reports it. */
  recentErrors: ErrorEntry[] | null;
  /** `null` unless the engine reports it. */
  resources: ResourceUse | null;
}

// ---------------------------------------------------------------------------------------------
// Workspaces / projects
// ---------------------------------------------------------------------------------------------

export type RefKind = 'branch' | 'remote' | 'tag' | 'sha' | 'worktree-head';

export interface RefPolicy {
  kind: RefKind;
  /** Branch, `remote/branch`, tag or commit id; empty for `worktree-head`. */
  name: string;
  /** Canonical text form, e.g. `branch:main`. */
  text: string;
}

export type DataPolicy = 'local-only' | 'cloud';

export interface WorkspaceMember {
  name: string;
  role: 'owner' | 'maintainer' | 'reader';
}

export interface WorkspaceSummary {
  id: string;
  name: string;
  /** `null` when `knowell.toml` is unknown. */
  description: string | null;
  projectCount: number;
  /** `null`: not tracked yet. */
  memberCount: number | null;
  trackedRef: RefPolicy | null;
  embeddingProfileId: string | null;
  dataPolicy: DataPolicy | null;
  createdAt: IsoTime;
  /** Set when `knowell.toml` is configured but could not be loaded. */
  settingsError?: string;
}

export interface WorkspaceDetail extends WorkspaceSummary {
  projectIds: string[];
  /** `null`: not tracked yet. */
  members: WorkspaceMember[] | null;
}

export interface Worktree {
  path: string;
  branch: string;
  head: string;
  /** Files differing from the tracked view (the personal layer). */
  dirtyFiles: number;
  /** Task-view group when several worktrees share a feature name. */
  taskGroup?: string;
}

export interface ProjectSummary {
  id: string;
  workspaceId: string;
  workspaceName: string;
  name: string;
  /** Free text (for example `web`, `service`); `null`: not tracked yet. */
  kind: string | null;
  languages: string[] | null;
  indexed: boolean;
  fileCount: number | null;
  /** `null` until a generation was activated. */
  lastIndexedAt: IsoTime | null;
}

/** Effective embedding settings. `provider` is `null` when no layer names one. */
export interface EmbeddingSettings {
  provider: Setting<string> | null;
  /** `null` means the provider's default model. */
  model: Setting<string> | null;
  preset: Setting<string>;
  dimensions: Setting<number>;
}

export interface ProjectViewRef {
  id: string;
  trackTarget: RefPolicy;
  activeIndexCommit: string | null;
  lastSeenCommit: string | null;
}

export interface ProjectDetail extends ProjectSummary {
  /** For git sources `remote` is a clone URL or a local path. */
  source: { type: 'git'; remote: string } | { type: 'local'; path: string };
  root: Setting<string>;
  /** `null` when the workspace file is unknown. */
  trackedRef: Setting<RefPolicy> | null;
  /** One setting per pattern, each with its own origin; `null` when unknown. */
  excludes: Setting<string>[] | null;
  embedding: EmbeddingSettings | null;
  /** `null`: not tracked yet. */
  embeddingProfileId: Setting<string> | null;
  dataPolicy: Setting<DataPolicy> | null;
  /** `null`: not tracked yet. */
  analysis: Setting<string> | null;
  /** `null`: not tracked yet. */
  worktrees: Worktree[] | null;
  /** Paths excluded by the secret/sensitivity policy; `null`: not tracked yet. */
  sensitiveExcludedCount: number | null;
  views: ProjectViewRef[];
  settingsError?: string;
}

// ---------------------------------------------------------------------------------------------
// Indexes
// ---------------------------------------------------------------------------------------------

export type GenerationState = 'building' | 'active' | 'retired' | 'failed';
export type ViewState = 'ready' | 'building' | 'stale' | 'not-indexed';

/** One view = a project at a tracked ref. The three commits are deliberately separate. */
export interface IndexView {
  id: string;
  projectId: string;
  projectName: string;
  workspaceName: string;
  trackTarget: RefPolicy;
  /** Latest commit seen at the tracking target; may be ahead of the active index. */
  lastSeenCommit: string | null;
  /** Commit the active index was built from. */
  activeIndexCommit: string | null;
  state: ViewState;
  stateNote: string | null;
  generations: IndexGeneration[];
  /** `null`: not tracked yet. */
  tiers: TierStatus[] | null;
  /** `null`: not tracked yet. */
  analysis: AnalysisCoverage[] | null;
}

export interface IndexGeneration {
  id: number;
  state: GenerationState;
  /** `null` for directory sources. */
  commit: string | null;
  /** `null`: not tracked per generation. */
  profileId: string | null;
  createdAt: IsoTime;
  activatedAt: IsoTime | null;
  finishedAt: IsoTime | null;
  chunkCount: number | null;
  /** 0..1 while building, when the indexer reports it. */
  progress: number | null;
  /** Why a failed generation failed. */
  error: string | null;
}

export interface TierStatus {
  tier: FreshnessTier;
  coverage: number;
  filesBehind: number;
}

/** The two analysis levels are shown separately (ARCHITECTURE 7). */
export interface AnalysisCoverage {
  language: string;
  files: number;
  syntactic: number;
  /** SCIP / language tool coverage; `null` = not configured, never shown as 0%. */
  semantic: number | null;
  note?: string;
}

export type JobState = 'queued' | 'running' | 'succeeded' | 'failed' | 'cancelled' | 'dead';
/** Free string, for example `view.reindex` or `source.refresh`. */
export type JobKind = string;

export interface Job {
  id: string;
  kind: JobKind;
  state: JobState;
  projectName: string | null;
  workspaceName: string | null;
  attempts: number;
  maxAttempts: number;
  enqueuedAt: IsoTime;
  updatedAt: IsoTime;
  /** 0..1 when the worker reports progress. */
  progress: number | null;
  error: string | null;
}

export interface DeadLetter {
  jobId: string;
  kind: JobKind;
  projectName: string | null;
  workspaceName: string | null;
  failedAt: IsoTime;
  attempts: number;
  error: string | null;
}

export interface JobsQuery {
  state?: JobState;
  limit?: number;
}

export interface ProfileMigration {
  id: string;
  fromProfileId: string;
  toProfileId: string;
  state: 'building' | 'checking' | 'ready-to-activate' | 'active' | 'rolled-back';
  progress: number;
  /** Old index stays reversible until this time. */
  reversibleUntil?: IsoTime;
}

export interface IndexesOverview {
  views: IndexView[];
  /** `null` without organization-wide read access. */
  jobs: Job[] | null;
  /** `null` like `jobs`. */
  deadLetters: DeadLetter[] | null;
  /** `null`: reported by the engine only. */
  migrations: ProfileMigration[] | null;
}

export interface ReindexRequest {
  viewId: string;
  scope: 'changed' | 'full';
}
/** `202 Accepted`. `created` is false when an `Idempotency-Key` matched an existing job. */
export interface ReindexResult {
  jobId: string;
  created: boolean;
}

/** Body of `202` from a profile switch; the engine defines it. */
export type SwitchStarted = Record<string, unknown> | null;

/** Server-sent progress event (`/api/v1/events`). */
export type ProgressEvent =
  | { type: 'job'; job: Job }
  | { type: 'generation'; viewId: string; generation: IndexGeneration }
  | { type: 'heartbeat'; at: IsoTime }
  /** The subscriber fell behind: refetch state. */
  | { type: 'resync'; missed: number };

// ---------------------------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------------------------

export type QueryClass =
  'exact-symbol' | 'endpoint' | 'error-trace' | 'behavior' | 'impact' | 'why';

export interface SearchRequest {
  query: string;
  projectIds?: string[];
  languages?: string[];
  pathPrefix?: string;
  /** Include graph expansion (callers, tests, contracts). */
  expandGraph: boolean;
  rerank: boolean;
  limit: number;
}

/** Per-signal contribution to the fused score; `null` = signal did not run (not zero). */
export interface ScoreBreakdown {
  fused: number;
  bm25: number | null;
  vector: number | null;
  graph: number | null;
  rerank: number | null;
}

export type MatchReason =
  | { type: 'exact-symbol'; symbol: string }
  | { type: 'lexical'; terms: string[] }
  | { type: 'semantic'; similarity: number }
  | { type: 'graph'; path: string[]; edge: EvidenceType }
  | { type: 'test-reference'; target: string };

export interface SearchResult {
  id: string;
  projectId: string;
  projectName: string;
  path: string;
  lineStart: number;
  lineEnd: number;
  language: string;
  symbol?: string;
  /** Source text. Rendered as text only; never as HTML. */
  snippet: string;
  /** What the engine actually sent to the embedding model (after redaction). */
  preparedText?: string;
  /** Tier the result comes from (ARCHITECTURE 6.4). */
  tier: FreshnessTier;
  commit: string;
  score: ScoreBreakdown;
  reasons: MatchReason[];
}

export interface SearchResponse {
  queryClass: QueryClass;
  results: SearchResult[];
  tookMs: number;
  /** Set when the result is empty or partial; explains why. */
  emptyReason?: { code: string; message: string };
  /** Projects that could not contribute and why. */
  skipped: { projectName: string; reason: string }[];
  tokensReturned: number;
}

// ---------------------------------------------------------------------------------------------
// Graph
// ---------------------------------------------------------------------------------------------

export type NodeKind =
  'service' | 'module' | 'symbol' | 'endpoint' | 'event' | 'table' | 'package' | 'env' | 'i18n-key';

/** ARCHITECTURE 7.1. Never collapsed into a single confidence number. */
export type EvidenceType =
  | 'semantically-resolved'
  | 'contract-derived'
  | 'syntactic'
  | 'heuristic'
  | 'model-suggestion'
  | 'runtime-observation';

export type ResolutionStatus = 'resolved' | 'ambiguous' | 'unresolved';

export type EdgeKind =
  | 'calls'
  | 'imports'
  | 'defines'
  | 'http-call'
  | 'publishes'
  | 'consumes'
  | 'reads-table'
  | 'writes-table'
  | 'depends-on'
  | 'reads-env'
  | 'uses-i18n'
  | 'tested-by';

export interface GraphNode {
  id: string;
  kind: NodeKind;
  label: string;
  projectId?: string;
  parentId?: string;
  detail?: string;
}

export interface GraphEdge {
  id: string;
  from: string;
  to: string;
  kind: EdgeKind;
  evidence: EvidenceType;
  status: ResolutionStatus;
  /** Source evidence: file + range in a view. */
  site?: { path: string; lineStart: number; lineEnd: number; commit: string };
}

export interface GraphSlice {
  nodes: GraphNode[];
  edges: GraphEdge[];
  truncated: boolean;
}

export type GraphInsightKind =
  | 'endpoint-without-client'
  | 'event-without-consumer'
  | 'table-never-read'
  | 'contract-drift'
  | 'missing-i18n-key'
  | 'client-parity-gap';

export interface GraphInsight {
  id: string;
  kind: GraphInsightKind;
  title: string;
  nodeIds: string[];
  evidence: EvidenceType;
  status: ResolutionStatus;
}

export interface GraphQuery {
  /** `undefined` = top-level service map. */
  parentId?: string;
  mode: 'hierarchy' | 'contracts';
}

// ---------------------------------------------------------------------------------------------
// Domains & glossary
// ---------------------------------------------------------------------------------------------

export type ApprovalStatus = 'approved' | 'suggested' | 'rejected';

export interface Domain {
  id: string;
  name: string;
  description: string;
  projectIds: string[];
  symbolCount: number;
}

export interface GlossaryTerm {
  id: string;
  term: string;
  definition: string;
  domainId: string;
  /** Maps query-language words to code names. */
  synonyms: { text: string; status: ApprovalStatus; origin: 'human' | 'auto' }[];
  codeNames: string[];
}

// ---------------------------------------------------------------------------------------------
// Memory / tasks / rules
// ---------------------------------------------------------------------------------------------

export type MemoryScope = 'organization' | 'workspace' | 'project' | 'task' | 'user';
export type MemoryState = 'proposed' | 'accepted' | 'rejected' | 'stale' | 'superseded';
export type MemoryKind = 'observed' | 'human' | 'agent-finding';

export interface Evidence {
  path: string;
  lineStart: number;
  lineEnd: number;
  commit: string;
  /** Whether the evidence still matches the current view. */
  stillValid: boolean;
}

export interface MemoryRecord {
  id: string;
  scope: MemoryScope;
  scopeName: string;
  kind: MemoryKind;
  state: MemoryState;
  title: string;
  body: string;
  author: { type: 'human' | 'agent' | 'engine'; name: string; session?: string };
  createdAt: IsoTime;
  version: number;
  pinned: boolean;
  evidence: Evidence[];
  supersededBy?: string;
  conflictsWith?: string[];
  staleReason?: string;
}

export interface MemoryDecision {
  id: string;
  action: 'accept' | 'reject';
  note?: string;
}

export type TaskStatus = 'open' | 'in-progress' | 'blocked' | 'done';

export interface TaskRecord {
  id: string;
  title: string;
  status: TaskStatus;
  goal: string;
  projectNames: string[];
  progress: string[];
  openQuestions: string[];
  /** Sources changed since the last checkpoint. */
  changedSinceCheckpoint: number;
  updatedAt: IsoTime;
}

export type Severity = 'error' | 'warning' | 'info';

export interface ArchRule {
  id: string;
  name: string;
  description: string;
  state: 'accepted' | 'proposed' | 'disabled';
  severity: Severity;
  pack?: string;
  approvedExamples: { path: string; note: string }[];
  violations: { path: string; lineStart: number; message: string; projectName: string }[];
}

// ---------------------------------------------------------------------------------------------
// Model profiles
// ---------------------------------------------------------------------------------------------

export type LocalityPolicy = 'local-only' | 'cloud-allowed' | 'cloud-required-hub';

export interface QualityMeasurement {
  /** recall@10 on the named query set. */
  recallAt10: number;
  mrr: number;
  querySet: string;
  measuredAt: IsoTime;
  hardware?: string;
}

export interface EmbeddingProfile {
  id: string;
  name: string;
  provider: 'gemini' | 'openai-compatible' | 'ollama' | 'onnx-local';
  model: string;
  /** Vector dimensions: configuration, not quality. */
  dimensions: number;
  storage: 'fp32' | 'halfvec';
  locality: LocalityPolicy;
  apiKey?: SecretRef;
  budgets: { monthlyUsdMicros?: number; tokensPerMinute?: number; diskBytes?: number };
  spentThisMonthUsdMicros: number;
  /** Measured separately; `null` = never measured, shown as "not measured", never as a score. */
  measured: QualityMeasurement | null;
  active: boolean;
  /** Blue-green state when a switch to this profile is running. */
  switch?: { state: ProfileMigration['state']; progress: number };
}

export interface SwitchEstimate {
  fromProfileId: string;
  toProfileId: string;
  affectedProjects: string[];
  chunksToRegenerate: number;
  needsReembedding: boolean;
  estimatedTokens: number;
  estimatedCostUsdMicros: number;
  estimatedDiskBytes: number;
  estimatedDurationMs: number;
  warnings: string[];
}

export interface SwitchRequest {
  toProfileId: string;
}

// ---------------------------------------------------------------------------------------------
// Quality (evaluation)
// ---------------------------------------------------------------------------------------------

export interface MetricSet {
  recallAt5: number;
  recallAt10: number;
  mrr: number;
  ndcgAt10: number;
}

export interface EvalSlice {
  /** `retriever`, `kind` (query class) or `lang`. */
  dimension: 'retriever' | 'kind' | 'lang';
  value: string;
  queries: number;
  metrics: MetricSet;
}

export interface BadResult {
  query: string;
  expected: string;
  got: string;
  queryClass: QueryClass;
}

export interface EvalReport {
  id: string;
  querySet: string;
  profileId: string;
  ranAt: IsoTime;
  hardware: string;
  dataset: string;
  queryCount: number;
  overall: MetricSet;
  slices: EvalSlice[];
  badResults: BadResult[];
}

// ---------------------------------------------------------------------------------------------
// Agents & usage / integrations / admin
// ---------------------------------------------------------------------------------------------

export interface ToolUsage {
  tool: string;
  calls: number;
  errors: number;
  tokensReturned: number;
  p50Ms: number;
  p95Ms: number;
  spendUsdMicros: number;
}

export interface AgentUsage {
  agent: string;
  sessions: number;
  calls: number;
  tokensReturned: number;
  lastSeen: IsoTime;
}

export interface UsageDay {
  date: string;
  calls: number;
  tokensReturned: number;
  spendUsdMicros: number;
}

export interface UsageReport {
  periodDays: number;
  tools: ToolUsage[];
  agents: AgentUsage[];
  daily: UsageDay[];
}

export interface McpConnection {
  transport: 'stdio' | 'streamable-http';
  endpoint: string;
  status: 'connected' | 'idle' | 'error';
  lastCallAt?: IsoTime;
}

export interface AgentConnectStatus {
  agent: 'claude' | 'codex' | 'cursor';
  /** Result of `know connect <agent>`. */
  configured: boolean;
  mcpConfig: 'present' | 'missing' | 'outdated';
  instructionFile: 'present' | 'missing';
  sessionStartHook: 'present' | 'missing' | 'not-applicable';
  diagnostics: { check: string; ok: boolean; detail: string }[];
}

export interface IntegrationsStatus {
  mcp: McpConnection;
  agents: AgentConnectStatus[];
  webhooks: {
    provider: 'github' | 'gitlab' | 'gitea';
    enabled: boolean;
    lastDeliveryAt?: IsoTime;
  }[];
}

export interface AdminUser {
  id: string;
  name: string;
  email: string;
  role: 'admin' | 'maintainer' | 'reader';
  lastLoginAt?: IsoTime;
  disabled: boolean;
}

export interface AdminToken {
  id: string;
  name: string;
  /** Only the non-secret prefix is ever shown; the value is shown once at creation by the server. */
  prefix: string;
  scopes: string[];
  createdAt: IsoTime;
  expiresAt?: IsoTime;
}

export interface AuditEntry {
  id: string;
  at: IsoTime;
  actor: string;
  action: string;
  target: string;
  outcome: 'success' | 'denied';
}

export interface AdminOverview {
  /** Administration exists only in the `hub` role. */
  available: boolean;
  role: Role;
  users: AdminUser[];
  tokens: AdminToken[];
  audit: AuditEntry[];
}

export interface Session {
  /** `user:<uuid>`. */
  user: string;
  role: Role;
  csrfToken: string;
  /** When the session ends at the latest. */
  expiresAt: IsoTime;
}
