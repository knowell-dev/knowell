/**
 * Fictional fixtures for the mock client: a generic e-commerce workspace. Nothing here comes
 * from a real codebase; all names, paths and snippets are invented.
 */
import type {
  AdminOverview,
  AnalysisCoverage,
  ArchRule,
  Domain,
  DeadLetter,
  EmbeddingProfile,
  EngineHealth,
  EvalReport,
  GlossaryTerm,
  GraphEdge,
  GraphInsight,
  GraphNode,
  IndexesOverview,
  IndexGeneration,
  IndexView,
  Job,
  ProjectSummary,
  RefPolicy,
  Setting,
  IntegrationsStatus,
  MemoryRecord,
  ProjectDetail,
  SearchResult,
  TaskRecord,
  UsageReport,
  WorkspaceDetail
} from './types';

const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;
export const ago = (ms: number): string => new Date(Date.now() - ms).toISOString();
const ahead = (ms: number): string => new Date(Date.now() + ms).toISOString();

export const WS_ID = 'ws-shopfront';
export const WS_NAME = 'shopfront';

const ref = (kind: RefPolicy['kind'], name: string): RefPolicy => ({
  kind,
  name,
  text: kind === 'worktree-head' ? 'worktree' : `${kind === 'sha' ? 'commit' : kind}:${name}`
});

export function workspace(): WorkspaceDetail {
  return {
    id: WS_ID,
    name: 'shopfront',
    description: 'Generic e-commerce platform: storefront, mobile app and backend services.',
    projectCount: 9,
    memberCount: 3,
    trackedRef: ref('branch', 'development'),
    embeddingProfileId: 'prof-gemini-768',
    dataPolicy: 'cloud',
    createdAt: ago(120 * DAY),
    projectIds: projectSeeds.map((p) => p.id),
    members: [
      { name: 'ada', role: 'owner' },
      { name: 'linus', role: 'maintainer' },
      { name: 'grace', role: 'reader' }
    ]
  };
}

export function workspaceList(): WorkspaceDetail[] {
  const main = workspace();
  const scratch: WorkspaceDetail = {
    id: 'ws-playground',
    name: 'playground',
    description: 'Empty workspace created for experiments.',
    projectCount: 0,
    memberCount: 1,
    trackedRef: ref('branch', 'main'),
    embeddingProfileId: 'prof-local-bge',
    dataPolicy: 'local-only',
    createdAt: ago(9 * DAY),
    projectIds: [],
    members: [{ name: 'ada', role: 'owner' }]
  };
  return [main, scratch];
}

interface ProjectSeed {
  id: string;
  name: string;
  kind: string;
  languages: string[];
  files: number;
  indexed: boolean;
  remote: string;
  root: string;
  ref?: { kind: 'branch' | 'tag'; name: string; origin: 'workspace' | 'project' };
  excludes?: string[];
  scip?: boolean;
}

const projectSeeds: ProjectSeed[] = [
  {
    id: 'p-web',
    name: 'storefront-web',
    kind: 'web',
    languages: ['TypeScript', 'CSS'],
    files: 1840,
    indexed: true,
    remote: 'git@git.example.test:shop/storefront-web.git',
    root: '.',
    scip: true
  },
  {
    id: 'p-mobile',
    name: 'mobile-app',
    kind: 'mobile',
    languages: ['Dart'],
    files: 960,
    indexed: true,
    remote: 'git@git.example.test:shop/mobile-app.git',
    root: '.'
  },
  {
    id: 'p-billing',
    name: 'billing-api',
    kind: 'api',
    languages: ['TypeScript'],
    files: 520,
    indexed: true,
    remote: 'git@git.example.test:shop/billing-api.git',
    root: 'src',
    scip: true
  },
  {
    id: 'p-orders',
    name: 'orders-service',
    kind: 'service',
    languages: ['Go'],
    files: 410,
    indexed: true,
    remote: 'git@git.example.test:shop/orders-service.git',
    root: '.',
    ref: { kind: 'branch', name: 'release/2.x', origin: 'project' },
    scip: true
  },
  {
    id: 'p-ledger',
    name: 'ledger-service',
    kind: 'service',
    languages: ['Rust'],
    files: 280,
    indexed: true,
    remote: 'git@git.example.test:shop/ledger-service.git',
    root: '.',
    scip: true
  },
  {
    id: 'p-notify',
    name: 'notification-worker',
    kind: 'worker',
    languages: ['Python'],
    files: 190,
    indexed: true,
    remote: 'git@git.example.test:shop/notification-worker.git',
    root: 'app'
  },
  {
    id: 'p-contracts',
    name: 'contracts',
    kind: 'contracts',
    languages: ['Proto', 'OpenAPI'],
    files: 74,
    indexed: true,
    remote: 'git@git.example.test:shop/contracts.git',
    root: '.',
    ref: { kind: 'tag', name: 'v3.4.0', origin: 'project' }
  },
  {
    id: 'p-migrations',
    name: 'migrations',
    kind: 'migrations',
    languages: ['SQL'],
    files: 132,
    indexed: true,
    remote: 'git@git.example.test:shop/migrations.git',
    root: 'sql',
    excludes: ['sql/seeds/**']
  },
  {
    id: 'p-infra',
    name: 'infra',
    kind: 'infra',
    languages: ['Terraform', 'YAML'],
    files: 0,
    indexed: false,
    remote: 'git@git.example.test:shop/infra.git',
    root: '.'
  }
];

export function projectSummaries(): ProjectSummary[] {
  return projectSeeds.map((p) => ({
    id: p.id,
    workspaceId: WS_ID,
    workspaceName: WS_NAME,
    name: p.name,
    kind: p.kind,
    languages: p.languages,
    indexed: p.indexed,
    fileCount: p.files,
    lastIndexedAt: p.indexed ? ago((Number(p.id.length) + (p.files % 7)) * 11 * MIN) : null
  }));
}

export function projectDetail(id: string): ProjectDetail | undefined {
  const p = projectSeeds.find((x) => x.id === id);
  if (!p) return undefined;
  const sum = projectSummaries().find((s) => s.id === id);
  if (!sum) return undefined;
  const ws = workspace();
  return {
    ...sum,
    source: { type: 'git', remote: p.remote },
    views: [
      {
        id: `view-${p.id}`,
        trackTarget: p.ref
          ? ref(p.ref.kind, p.ref.name)
          : (ws.trackedRef ?? ref('branch', 'development')),
        activeIndexCommit: p.indexed ? 'e3b0c44' : null,
        lastSeenCommit: p.indexed ? 'e3b0c44' : null
      }
    ],
    root:
      p.root === '.'
        ? { value: '.', origin: 'builtin' }
        : { value: p.root, origin: 'project', originNote: 'project settings' },
    trackedRef: p.ref
      ? {
          value: ref(p.ref.kind, p.ref.name),
          origin: 'project',
          originNote: 'project settings'
        }
      : {
          value: ws.trackedRef ?? ref('branch', 'development'),
          origin: 'workspace',
          originNote: `workspace "${ws.name}"`
        },
    excludes: [
      ...(p.excludes ?? []).map((x): Setting<string> => ({
        value: x,
        origin: 'project',
        originNote: 'project settings'
      })),
      ...(p.excludes
        ? ['node_modules/**', 'vendor/**']
        : ['node_modules/**', 'vendor/**', 'dist/**', '**/*.min.js']
      ).map((x): Setting<string> => ({ value: x, origin: 'builtin' }))
    ],
    embedding: {
      provider: { value: 'gemini', origin: 'workspace', originNote: `workspace "${ws.name}"` },
      model: null,
      preset: { value: 'balanced', origin: 'builtin' },
      dimensions: { value: 768, origin: 'workspace', originNote: `workspace "${ws.name}"` }
    },
    embeddingProfileId: {
      value: ws.embeddingProfileId ?? 'prof-gemini-768',
      origin: 'workspace',
      originNote: `workspace "${ws.name}"`
    },
    dataPolicy: { value: 'cloud', origin: 'workspace', originNote: `workspace "${ws.name}"` },
    analysis: p.scip
      ? { value: 'tree-sitter+scip', origin: 'project', originNote: 'project settings' }
      : { value: 'tree-sitter', origin: 'builtin' },
    worktrees:
      p.id === 'p-orders'
        ? [
            {
              path: '~/work/orders-service',
              branch: 'release/2.x',
              head: 'a41c9e2',
              dirtyFiles: 0
            },
            {
              path: '~/work/.worktree/refunds/orders-service',
              branch: 'feature/refunds',
              head: '9be03d1',
              dirtyFiles: 4,
              taskGroup: 'refunds'
            }
          ]
        : p.id === 'p-billing'
          ? [
              {
                path: '~/work/.worktree/refunds/billing-api',
                branch: 'feature/refunds',
                head: '31d77ab',
                dirtyFiles: 2,
                taskGroup: 'refunds'
              }
            ]
          : [],
    sensitiveExcludedCount: p.indexed ? 3 + (p.files % 5) : 0
  };
}

// ---------------------------------------------------------------------------------------------

const analysis = (
  language: string,
  files: number,
  syn: number,
  sem: number | null,
  note?: string
): AnalysisCoverage => ({
  language,
  files,
  syntactic: syn,
  semantic: sem,
  note
});

const job = (
  j: Pick<Job, 'id' | 'kind' | 'state' | 'attempts' | 'enqueuedAt'> & Partial<Job>
): Job => ({
  projectName: null,
  workspaceName: WS_NAME,
  maxAttempts: 5,
  updatedAt: j.enqueuedAt,
  progress: null,
  error: null,
  ...j
});

export function indexes(): IndexesOverview {
  const commits = [
    'e3b0c44',
    '8f14e45',
    'c9f0f89',
    'a87ff67',
    '1679091',
    '45c48cc',
    'd3d9446',
    '6512bd4'
  ];
  const sums = projectSummaries().filter((p) => p.indexed);
  const views = sums.map((p, i): IndexView => {
    const track = projectDetail(p.id)?.trackedRef?.value ?? ref('branch', 'development');
    const behind = i === 3 || i === 5;
    const files = p.fileCount ?? 0;
    const generation = (
      g: Pick<IndexGeneration, 'id' | 'state' | 'commit' | 'createdAt'> & Partial<IndexGeneration>
    ): IndexGeneration => ({
      profileId: 'prof-gemini-768',
      activatedAt: null,
      finishedAt: null,
      chunkCount: null,
      progress: null,
      error: null,
      ...g
    });
    return {
      id: `view-${p.id}`,
      projectId: p.id,
      projectName: p.name,
      workspaceName: WS_NAME,
      trackTarget: track,
      lastSeenCommit: behind ? 'f00dbab' : (commits[i] ?? null),
      activeIndexCommit: commits[i] ?? null,
      state: behind ? 'building' : 'ready',
      stateNote: behind
        ? 'new commit seen; next generation is building while the last ready view serves'
        : null,
      generations: [
        ...(behind
          ? [
              generation({
                id: 8 + i,
                state: 'building',
                commit: 'f00dbab',
                createdAt: ago(4 * MIN),
                chunkCount: 0,
                progress: 0.42
              })
            ]
          : []),
        generation({
          id: 7 + i,
          state: 'active',
          commit: commits[i] ?? 'e3b0c44',
          createdAt: ago((i + 1) * 3 * HOUR),
          activatedAt: ago((i + 1) * 3 * HOUR - 5 * MIN),
          finishedAt: ago((i + 1) * 3 * HOUR - 5 * MIN),
          chunkCount: files * 6
        }),
        generation({
          id: 6 + i,
          state: 'retired',
          commit: '0ddba11',
          createdAt: ago((i + 2) * DAY),
          activatedAt: ago((i + 2) * DAY - 5 * MIN),
          finishedAt: ago((i + 1) * 3 * HOUR - 5 * MIN),
          chunkCount: files * 6 - 41
        })
      ],
      tiers: [
        { tier: 'T0', coverage: 1, filesBehind: 0 },
        { tier: 'T1', coverage: 1, filesBehind: 0 },
        {
          tier: 'T2',
          coverage: behind ? 0.93 : 0.99,
          filesBehind: behind ? Math.round(files * 0.07) : Math.round(files * 0.01)
        },
        {
          tier: 'T3',
          coverage: behind ? 0.8 : 0.96,
          filesBehind: behind ? Math.round(files * 0.2) : Math.round(files * 0.04)
        }
      ],
      analysis: analysisFor(p.name, files)
    };
  });
  const deadLetters: DeadLetter[] = [
    {
      jobId: 'job-4777',
      kind: 'view.reindex',
      projectName: 'mobile-app',
      workspaceName: WS_NAME,
      failedAt: ago(5 * HOUR),
      attempts: 5,
      error: 'provider rate limit exceeded after 5 attempts'
    },
    {
      jobId: 'job-4402',
      kind: 'source.refresh',
      projectName: null,
      workspaceName: null,
      failedAt: ago(2 * DAY),
      attempts: 3,
      error: null
    }
  ];
  return {
    views,
    jobs: [
      job({
        id: 'job-4812',
        kind: 'view.reindex',
        state: 'running',
        projectName: 'orders-service',
        attempts: 1,
        enqueuedAt: ago(4 * MIN),
        updatedAt: ago(1 * MIN),
        progress: 0.42
      }),
      job({
        id: 'job-4813',
        kind: 'view.reindex',
        state: 'queued',
        projectName: 'orders-service',
        attempts: 0,
        enqueuedAt: ago(3 * MIN)
      }),
      job({
        id: 'job-4809',
        kind: 'source.refresh',
        state: 'succeeded',
        attempts: 1,
        workspaceName: null,
        enqueuedAt: ago(22 * MIN),
        updatedAt: ago(20 * MIN)
      }),
      job({
        id: 'job-4801',
        kind: 'view.reindex',
        state: 'failed',
        projectName: 'notification-worker',
        attempts: 2,
        enqueuedAt: ago(41 * MIN),
        updatedAt: ago(38 * MIN),
        error: 'scip indexer for python is not installed; analysis stays syntactic'
      }),
      ...deadLetters.map((x) =>
        job({
          id: x.jobId,
          kind: x.kind,
          state: 'dead',
          projectName: x.projectName,
          workspaceName: x.workspaceName,
          attempts: x.attempts,
          maxAttempts: x.attempts,
          enqueuedAt: x.failedAt,
          error: x.error
        })
      )
    ],
    deadLetters,
    migrations: [
      {
        id: 'mig-12',
        fromProfileId: 'prof-gemini-768',
        toProfileId: 'prof-gemini-1536',
        state: 'building',
        progress: 0.18,
        reversibleUntil: ahead(14 * DAY)
      }
    ]
  };
}

function analysisFor(name: string, files: number): AnalysisCoverage[] {
  switch (name) {
    case 'storefront-web':
      return [
        analysis('TypeScript', Math.round(files * 0.8), 1, 0.97),
        analysis('CSS', Math.round(files * 0.2), 1, null, 'text + chunking only')
      ];
    case 'mobile-app':
      return [analysis('Dart', files, 1, null, 'no SCIP indexer configured; structural tier')];
    case 'orders-service':
      return [analysis('Go', files, 1, 0.91)];
    case 'ledger-service':
      return [analysis('Rust', files, 1, 0.99)];
    case 'notification-worker':
      return [analysis('Python', files, 1, null, 'scip-python not installed')];
    case 'contracts':
      return [analysis('Proto', 52, 1, 1), analysis('OpenAPI', 22, 1, 1)];
    case 'migrations':
      return [analysis('SQL', files, 0.98, null, 'schema files parsed; no semantic level')];
    default:
      return [analysis('TypeScript', files, 1, 0.95)];
  }
}

// ---------------------------------------------------------------------------------------------

export function health(): EngineHealth {
  return {
    status: 'degraded',
    version: '0.1.0-dev',
    role: 'standalone',
    uptimeMs: 3 * DAY + 4 * HOUR,
    bindAddress: '127.0.0.1:7420',
    components: [
      { name: 'PostgreSQL (managed)', status: 'ok', detail: 'pgvector 0.8, 3 connections' },
      {
        name: 'Lexical index (Tantivy)',
        status: 'ok',
        detail: '9 projects, 4 commits behind head on 2'
      },
      {
        name: 'Embedding provider',
        status: 'degraded',
        detail: 'gemini: rate limited 2 times in the last hour'
      },
      { name: 'File watcher', status: 'ok', detail: 'watching 3 worktrees' },
      { name: 'MCP server', status: 'ok', detail: 'stdio + streamable HTTP' }
    ],
    queue: { queued: 6, running: 2, failed: 1, deadLetter: 2, oldestQueuedMs: 3 * MIN },
    freshness: [
      { tier: 'T0', coverage: 1, medianLagMs: 1_800 },
      { tier: 'T1', coverage: 0.998, medianLagMs: 4_200 },
      { tier: 'T2', coverage: 0.971, medianLagMs: 6 * MIN },
      { tier: 'T3', coverage: 0.934, medianLagMs: 19 * MIN }
    ],
    recentErrors: [
      {
        at: ago(5 * HOUR),
        code: 'provider_rate_limited',
        message: 'embedding provider rate limit exceeded after 5 attempts',
        projectId: 'p-mobile',
        jobId: 'job-4777'
      },
      {
        at: ago(41 * MIN),
        code: 'scip_missing',
        message: 'scip indexer for python is not installed',
        projectId: 'p-notify',
        jobId: 'job-4801'
      },
      {
        at: ago(2 * DAY),
        code: 'budget_exhausted',
        message: 'description budget for this month is exhausted',
        projectId: 'p-ledger'
      }
    ],
    resources: {
      cpuPercent: 23,
      memoryBytes: 1.4 * 1024 ** 3,
      memoryLimitBytes: 4 * 1024 ** 3,
      diskIndexBytesEstimated: 2.6 * 1024 ** 3,
      diskIndexBytesActual: 2.9 * 1024 ** 3,
      diskLimitBytes: 20 * 1024 ** 3,
      openConnections: 7
    }
  };
}

// ---------------------------------------------------------------------------------------------
// Search corpus

const c = (
  r: Omit<SearchResult, 'score' | 'reasons' | 'tier' | 'commit'> &
    Partial<Pick<SearchResult, 'tier'>>
): SearchResult => ({
  tier: 'T2',
  commit: 'e3b0c44',
  score: { fused: 0, bm25: null, vector: null, graph: null, rerank: null },
  reasons: [],
  ...r
});

export const corpus: SearchResult[] = [
  c({
    id: 'c1',
    projectId: 'p-orders',
    projectName: 'orders-service',
    path: 'internal/orders/place.go',
    lineStart: 41,
    lineEnd: 78,
    language: 'Go',
    symbol: 'PlaceOrder',
    snippet:
      'func (s *Service) PlaceOrder(ctx context.Context, req PlaceOrderRequest) (Order, error) {\n\tif err := req.Validate(); err != nil {\n\t\treturn Order{}, err\n\t}\n\torder := newOrder(req)\n\tif err := s.store.Insert(ctx, order); err != nil {\n\t\treturn Order{}, fmt.Errorf("insert order: %w", err)\n\t}\n\ts.bus.Publish(ctx, "orders.order_placed", order.Event())\n\treturn order, nil\n}',
    preparedText:
      'orders-service | internal/orders/place.go | Service.PlaceOrder | func PlaceOrder(ctx, req PlaceOrderRequest) (Order, error)\n// validate, persist and publish orders.order_placed'
  }),
  c({
    id: 'c2',
    projectId: 'p-web',
    projectName: 'storefront-web',
    path: 'src/checkout/submitOrder.ts',
    lineStart: 12,
    lineEnd: 40,
    language: 'TypeScript',
    symbol: 'submitOrder',
    snippet:
      "export async function submitOrder(cart: Cart): Promise<OrderConfirmation> {\n  const res = await http.post('/v1/orders', toPayload(cart));\n  if (!res.ok) throw new CheckoutError(res.status);\n  return parseConfirmation(await res.json());\n}",
    preparedText:
      'storefront-web | src/checkout/submitOrder.ts | submitOrder | calls POST /v1/orders'
  }),
  c({
    id: 'c3',
    projectId: 'p-billing',
    projectName: 'billing-api',
    path: 'src/payments/capture.controller.ts',
    lineStart: 22,
    lineEnd: 61,
    language: 'TypeScript',
    symbol: 'PaymentsController.capture',
    snippet:
      "@Post('/v1/payments/:id/capture')\nasync capture(@Param('id') id: string, @Body() dto: CaptureDto) {\n  const payment = await this.payments.require(id);\n  const result = await this.provider.capture(payment, dto.amount);\n  await this.ledger.post(payment.id, result);\n  return result;\n}"
  }),
  c({
    id: 'c4',
    projectId: 'p-ledger',
    projectName: 'ledger-service',
    path: 'src/entries/post.rs',
    lineStart: 9,
    lineEnd: 52,
    language: 'Rust',
    symbol: 'post_entry',
    snippet:
      'pub async fn post_entry(db: &Db, entry: NewEntry) -> Result<EntryId, LedgerError> {\n    entry.check_balanced()?;\n    let id = db.insert_entry(&entry).await?;\n    metrics::counter!("ledger_entries_posted").increment(1);\n    Ok(id)\n}'
  }),
  c({
    id: 'c5',
    projectId: 'p-notify',
    projectName: 'notification-worker',
    path: 'app/handlers/order_placed.py',
    lineStart: 5,
    lineEnd: 33,
    language: 'Python',
    symbol: 'handle_order_placed',
    tier: 'T1',
    snippet:
      '@subscribe("orders.order_placed")\ndef handle_order_placed(event: OrderPlaced) -> None:\n    template = templates.get("order_confirmation", event.locale)\n    mailer.send(event.customer_email, template.render(order=event))'
  }),
  c({
    id: 'c6',
    projectId: 'p-mobile',
    projectName: 'mobile-app',
    path: 'lib/checkout/order_repository.dart',
    lineStart: 18,
    lineEnd: 44,
    language: 'Dart',
    symbol: 'OrderRepository.submit',
    tier: 'T1',
    snippet:
      "Future<Order> submit(Cart cart) async {\n  final response = await _dio.post('/v1/orders', data: cart.toJson());\n  return Order.fromJson(response.data as Map<String, dynamic>);\n}"
  }),
  c({
    id: 'c7',
    projectId: 'p-migrations',
    projectName: 'migrations',
    path: 'sql/0042_add_refund_reason.sql',
    lineStart: 1,
    lineEnd: 14,
    language: 'SQL',
    snippet:
      'ALTER TABLE orders ADD COLUMN refund_reason text;\nCREATE INDEX orders_refund_reason_idx ON orders (refund_reason)\n  WHERE refund_reason IS NOT NULL;'
  }),
  c({
    id: 'c8',
    projectId: 'p-contracts',
    projectName: 'contracts',
    path: 'proto/orders/v1/events.proto',
    lineStart: 3,
    lineEnd: 21,
    language: 'Proto',
    symbol: 'OrderPlaced',
    snippet:
      'message OrderPlaced {\n  string order_id = 1;\n  string customer_email = 2;\n  string locale = 3;\n  int64 total_minor_units = 4;\n  string currency = 5;\n}'
  }),
  c({
    id: 'c9',
    projectId: 'p-orders',
    projectName: 'orders-service',
    path: 'internal/orders/place_test.go',
    lineStart: 14,
    lineEnd: 39,
    language: 'Go',
    symbol: 'TestPlaceOrderPublishesEvent',
    snippet:
      'func TestPlaceOrderPublishesEvent(t *testing.T) {\n\tbus := &fakeBus{}\n\tsvc := newTestService(t, bus)\n\t_, err := svc.PlaceOrder(ctx, validRequest())\n\trequire.NoError(t, err)\n\trequire.Equal(t, "orders.order_placed", bus.Last().Topic)\n}'
  }),
  c({
    id: 'c10',
    projectId: 'p-web',
    projectName: 'storefront-web',
    path: 'src/i18n/en.json',
    lineStart: 31,
    lineEnd: 36,
    language: 'JSON',
    symbol: 'checkout.pay_now',
    snippet:
      '"checkout": {\n  "pay_now": "Pay now",\n  "order_failed": "We could not place your order"\n}'
  })
];

// ---------------------------------------------------------------------------------------------
// Graph

const n = (
  id: string,
  kind: GraphNode['kind'],
  label: string,
  extra: Partial<GraphNode> = {}
): GraphNode => ({ id, kind, label, ...extra });

export const graphNodes: GraphNode[] = [
  n('svc:web', 'service', 'storefront-web', {
    projectId: 'p-web',
    detail: 'TypeScript, 1840 files'
  }),
  n('svc:mobile', 'service', 'mobile-app', { projectId: 'p-mobile', detail: 'Dart, 960 files' }),
  n('svc:billing', 'service', 'billing-api', {
    projectId: 'p-billing',
    detail: 'TypeScript, 520 files'
  }),
  n('svc:orders', 'service', 'orders-service', { projectId: 'p-orders', detail: 'Go, 410 files' }),
  n('svc:ledger', 'service', 'ledger-service', {
    projectId: 'p-ledger',
    detail: 'Rust, 280 files'
  }),
  n('svc:notify', 'service', 'notification-worker', {
    projectId: 'p-notify',
    detail: 'Python, 190 files'
  }),
  n('mod:orders/api', 'module', 'internal/orders/api', {
    parentId: 'svc:orders',
    projectId: 'p-orders'
  }),
  n('mod:orders/domain', 'module', 'internal/orders', {
    parentId: 'svc:orders',
    projectId: 'p-orders'
  }),
  n('mod:orders/store', 'module', 'internal/store', {
    parentId: 'svc:orders',
    projectId: 'p-orders'
  }),
  n('sym:PlaceOrder', 'symbol', 'Service.PlaceOrder', {
    parentId: 'mod:orders/domain',
    projectId: 'p-orders',
    detail: 'func, internal/orders/place.go:41'
  }),
  n('sym:CancelOrder', 'symbol', 'Service.CancelOrder', {
    parentId: 'mod:orders/domain',
    projectId: 'p-orders',
    detail: 'func, internal/orders/cancel.go:18'
  }),
  n('sym:Insert', 'symbol', 'Store.Insert', {
    parentId: 'mod:orders/store',
    projectId: 'p-orders',
    detail: 'method, internal/store/orders.go:30'
  }),
  n('sym:Handler', 'symbol', 'Handler.postOrder', {
    parentId: 'mod:orders/api',
    projectId: 'p-orders',
    detail: 'handler, internal/orders/api/http.go:12'
  }),
  n('mod:billing/payments', 'module', 'src/payments', {
    parentId: 'svc:billing',
    projectId: 'p-billing'
  }),
  n('mod:billing/refunds', 'module', 'src/refunds', {
    parentId: 'svc:billing',
    projectId: 'p-billing'
  }),
  n('ep:post-orders', 'endpoint', 'POST /v1/orders', { detail: 'contracts/openapi/orders.yaml' }),
  n('ep:capture', 'endpoint', 'POST /v1/payments/{id}/capture', {
    detail: 'contracts/openapi/payments.yaml'
  }),
  n('ep:legacy-ship', 'endpoint', 'GET /v1/shipping/quote', {
    detail: 'contracts/openapi/shipping.yaml'
  }),
  n('ev:order-placed', 'event', 'orders.order_placed', { detail: 'proto/orders/v1/events.proto' }),
  n('ev:entry-posted', 'event', 'ledger.entry_posted', { detail: 'proto/ledger/v1/events.proto' }),
  n('tb:orders', 'table', 'orders', { detail: 'migrations/sql' }),
  n('tb:ledger', 'table', 'ledger_entries', { detail: 'migrations/sql' }),
  n('env:provider', 'env', 'PAYMENT_PROVIDER_URL', { detail: 'name only; value never indexed' }),
  n('i18n:pay', 'i18n-key', 'checkout.pay_now', { detail: 'en present, tr missing' })
];

const e = (
  id: string,
  from: string,
  to: string,
  kind: GraphEdge['kind'],
  evidence: GraphEdge['evidence'],
  status: GraphEdge['status'] = 'resolved',
  site?: GraphEdge['site']
): GraphEdge => ({ id, from, to, kind, evidence, status, site });
const site = (path: string, a: number, b: number): GraphEdge['site'] => ({
  path,
  lineStart: a,
  lineEnd: b,
  commit: 'e3b0c44'
});

export const graphEdges: GraphEdge[] = [
  // service-level
  e('svc-1', 'svc:web', 'svc:orders', 'http-call', 'contract-derived'),
  e('svc-2', 'svc:mobile', 'svc:orders', 'http-call', 'heuristic', 'ambiguous'),
  e('svc-3', 'svc:orders', 'svc:billing', 'http-call', 'syntactic'),
  e('svc-4', 'svc:billing', 'svc:ledger', 'http-call', 'semantically-resolved'),
  e('svc-5', 'svc:orders', 'svc:notify', 'publishes', 'contract-derived'),
  // contract-level
  e(
    'x1',
    'svc:web',
    'ep:post-orders',
    'http-call',
    'syntactic',
    'resolved',
    site('src/checkout/submitOrder.ts', 14, 14)
  ),
  e(
    'x2',
    'svc:mobile',
    'ep:post-orders',
    'http-call',
    'heuristic',
    'ambiguous',
    site('lib/checkout/order_repository.dart', 20, 20)
  ),
  e(
    'x3',
    'svc:orders',
    'ep:post-orders',
    'defines',
    'contract-derived',
    'resolved',
    site('internal/orders/api/http.go', 12, 30)
  ),
  e(
    'x4',
    'svc:billing',
    'ep:capture',
    'defines',
    'semantically-resolved',
    'resolved',
    site('src/payments/capture.controller.ts', 22, 22)
  ),
  e(
    'x5',
    'svc:orders',
    'ep:capture',
    'http-call',
    'model-suggestion',
    'unresolved',
    site('internal/orders/pay.go', 55, 62)
  ),
  e(
    'x6',
    'svc:orders',
    'ev:order-placed',
    'publishes',
    'contract-derived',
    'resolved',
    site('internal/orders/place.go', 70, 70)
  ),
  e(
    'x7',
    'svc:notify',
    'ev:order-placed',
    'consumes',
    'syntactic',
    'resolved',
    site('app/handlers/order_placed.py', 5, 5)
  ),
  e('x8', 'svc:ledger', 'ev:entry-posted', 'publishes', 'contract-derived', 'resolved'),
  e(
    'x9',
    'svc:orders',
    'tb:orders',
    'writes-table',
    'semantically-resolved',
    'resolved',
    site('internal/store/orders.go', 30, 44)
  ),
  e('x10', 'svc:ledger', 'tb:ledger', 'writes-table', 'semantically-resolved', 'resolved'),
  e(
    'x11',
    'svc:billing',
    'env:provider',
    'reads-env',
    'syntactic',
    'resolved',
    site('src/config.ts', 8, 8)
  ),
  e(
    'x12',
    'svc:web',
    'i18n:pay',
    'uses-i18n',
    'syntactic',
    'resolved',
    site('src/checkout/PayButton.tsx', 9, 9)
  ),
  e('x13', 'svc:web', 'ep:legacy-ship', 'http-call', 'runtime-observation', 'resolved'),
  // hierarchy inside orders
  e(
    'h1',
    'sym:Handler',
    'sym:PlaceOrder',
    'calls',
    'semantically-resolved',
    'resolved',
    site('internal/orders/api/http.go', 24, 24)
  ),
  e(
    'h2',
    'sym:PlaceOrder',
    'sym:Insert',
    'calls',
    'semantically-resolved',
    'resolved',
    site('internal/orders/place.go', 48, 48)
  ),
  e(
    'h3',
    'sym:CancelOrder',
    'sym:Insert',
    'calls',
    'syntactic',
    'ambiguous',
    site('internal/orders/cancel.go', 30, 30)
  ),
  e('h4', 'mod:orders/api', 'mod:orders/domain', 'imports', 'semantically-resolved'),
  e('h5', 'mod:orders/domain', 'mod:orders/store', 'imports', 'semantically-resolved'),
  e('h6', 'mod:billing/refunds', 'mod:billing/payments', 'imports', 'syntactic')
];

export const insights: GraphInsight[] = [
  {
    id: 'i1',
    kind: 'endpoint-without-client',
    title:
      'GET /v1/shipping/quote has no client in indexed projects except one runtime observation',
    nodeIds: ['ep:legacy-ship'],
    evidence: 'runtime-observation',
    status: 'ambiguous'
  },
  {
    id: 'i2',
    kind: 'missing-i18n-key',
    title: 'checkout.pay_now is missing in the tr locale',
    nodeIds: ['i18n:pay'],
    evidence: 'syntactic',
    status: 'resolved'
  },
  {
    id: 'i3',
    kind: 'contract-drift',
    title: 'billing-api consumes ledger.entry_posted v1 but contracts is at v2',
    nodeIds: ['ev:entry-posted', 'svc:billing'],
    evidence: 'contract-derived',
    status: 'resolved'
  },
  {
    id: 'i4',
    kind: 'client-parity-gap',
    title: 'mobile-app does not call POST /v1/payments/{id}/capture (web does)',
    nodeIds: ['svc:mobile', 'ep:capture'],
    evidence: 'heuristic',
    status: 'ambiguous'
  }
];

// ---------------------------------------------------------------------------------------------

export const domains: Domain[] = [
  {
    id: 'd-checkout',
    name: 'Checkout',
    description: 'Cart to confirmed order, including payment capture.',
    projectIds: ['p-web', 'p-mobile', 'p-orders', 'p-billing'],
    symbolCount: 412
  },
  {
    id: 'd-ledger',
    name: 'Ledger',
    description: 'Double-entry bookkeeping for payments and refunds.',
    projectIds: ['p-ledger', 'p-billing', 'p-migrations'],
    symbolCount: 187
  },
  {
    id: 'd-notify',
    name: 'Notifications',
    description: 'Customer e-mails and push notifications.',
    projectIds: ['p-notify', 'p-mobile'],
    symbolCount: 96
  }
];

export const glossary: GlossaryTerm[] = [
  {
    id: 'g1',
    term: 'order',
    definition: 'A confirmed purchase request with a payment attached.',
    domainId: 'd-checkout',
    codeNames: ['Order', 'orders', 'order_id'],
    synonyms: [
      { text: 'siparis', status: 'approved', origin: 'human' },
      { text: 'purchase', status: 'suggested', origin: 'auto' }
    ]
  },
  {
    id: 'g2',
    term: 'capture',
    definition: 'Collect funds that were authorised earlier.',
    domainId: 'd-checkout',
    codeNames: ['capture', 'PaymentsController.capture'],
    synonyms: [
      { text: 'charge', status: 'approved', origin: 'human' },
      { text: 'settle', status: 'suggested', origin: 'auto' }
    ]
  },
  {
    id: 'g3',
    term: 'ledger entry',
    definition: 'One balanced line group in the books.',
    domainId: 'd-ledger',
    codeNames: ['NewEntry', 'ledger_entries', 'post_entry'],
    synonyms: [{ text: 'journal line', status: 'rejected', origin: 'auto' }]
  },
  {
    id: 'g4',
    term: 'dead letter',
    definition: 'A job that exhausted its retries.',
    domainId: 'd-notify',
    codeNames: ['DeadLetter', 'dlq'],
    synonyms: []
  }
];

// ---------------------------------------------------------------------------------------------

export function memory(): MemoryRecord[] {
  const ev = (path: string, a: number, b: number, valid = true) => ({
    path,
    lineStart: a,
    lineEnd: b,
    commit: 'e3b0c44',
    stillValid: valid
  });
  return [
    {
      id: 'm1',
      scope: 'workspace',
      scopeName: 'shopfront',
      kind: 'human',
      state: 'accepted',
      title: 'Money is stored as integer minor units',
      body: 'All services exchange amounts as integer minor units plus an ISO currency code. Floats are never used for money.',
      author: { type: 'human', name: 'ada' },
      createdAt: ago(60 * DAY),
      version: 3,
      pinned: true,
      evidence: [ev('proto/orders/v1/events.proto', 3, 21)]
    },
    {
      id: 'm2',
      scope: 'project',
      scopeName: 'orders-service',
      kind: 'human',
      state: 'accepted',
      title: 'Orders publish events after the database commit',
      body: 'PlaceOrder inserts first, then publishes orders.order_placed. Consumers must be idempotent on order_id.',
      author: { type: 'human', name: 'linus' },
      createdAt: ago(31 * DAY),
      version: 1,
      pinned: false,
      evidence: [ev('internal/orders/place.go', 41, 78)]
    },
    {
      id: 'm3',
      scope: 'project',
      scopeName: 'billing-api',
      kind: 'agent-finding',
      state: 'proposed',
      title: 'Capture endpoint needs no authentication',
      body: 'Observed no auth guard on PaymentsController.capture. Proposed as a rule by an agent session.',
      author: { type: 'agent', name: 'claude', session: 'sess-7f3a' },
      createdAt: ago(2 * HOUR),
      version: 1,
      pinned: false,
      evidence: [ev('src/payments/capture.controller.ts', 22, 30)],
      conflictsWith: ['m6']
    },
    {
      id: 'm4',
      scope: 'workspace',
      scopeName: 'shopfront',
      kind: 'agent-finding',
      state: 'proposed',
      title: 'Refund flow touches three services',
      body: 'Refunds start in orders-service, call billing-api and finish with a ledger entry in ledger-service.',
      author: { type: 'agent', name: 'codex', session: 'sess-19bc' },
      createdAt: ago(5 * HOUR),
      version: 1,
      pinned: false,
      evidence: [
        ev('internal/orders/refund.go', 10, 70),
        ev('src/refunds/refund.service.ts', 5, 60)
      ]
    },
    {
      id: 'm5',
      scope: 'project',
      scopeName: 'ledger-service',
      kind: 'observed',
      state: 'stale',
      title: 'Entries are posted through post_entry only',
      body: 'Extracted from call sites of post_entry.',
      author: { type: 'engine', name: 'analysis' },
      createdAt: ago(20 * DAY),
      version: 2,
      pinned: false,
      evidence: [ev('src/entries/post.rs', 9, 52, false)],
      staleReason: 'evidence changed: src/entries/post.rs was rewritten in commit 8f14e45'
    },
    {
      id: 'm6',
      scope: 'workspace',
      scopeName: 'shopfront',
      kind: 'human',
      state: 'accepted',
      title: 'All payment endpoints require a service token',
      body: 'Every endpoint under /v1/payments requires a service-to-service token.',
      author: { type: 'human', name: 'ada' },
      createdAt: ago(45 * DAY),
      version: 1,
      pinned: false,
      evidence: [],
      conflictsWith: ['m3']
    },
    {
      id: 'm7',
      scope: 'organization',
      scopeName: 'acme-shop',
      kind: 'human',
      state: 'superseded',
      title: 'Use REST polling for order status',
      body: 'Replaced by the event-driven status updates.',
      author: { type: 'human', name: 'ada' },
      createdAt: ago(200 * DAY),
      version: 1,
      pinned: false,
      evidence: [],
      supersededBy: 'm2'
    },
    {
      id: 'm8',
      scope: 'user',
      scopeName: 'ada',
      kind: 'human',
      state: 'rejected',
      title: 'Skip e2e tests on docs-only changes',
      body: 'Personal note; rejected for sharing.',
      author: { type: 'human', name: 'ada' },
      createdAt: ago(12 * DAY),
      version: 1,
      pinned: false,
      evidence: []
    },
    {
      id: 'm9',
      scope: 'task',
      scopeName: 'refunds',
      kind: 'agent-finding',
      state: 'proposed',
      title: 'Partial refunds need a new ledger entry kind',
      body: 'The ledger has no reversal kind for partial refunds; propose adding one.',
      author: { type: 'agent', name: 'claude', session: 'sess-7f3a' },
      createdAt: ago(40 * MIN),
      version: 1,
      pinned: false,
      evidence: [ev('src/entries/kind.rs', 1, 20)]
    }
  ];
}

export const tasks: TaskRecord[] = [
  {
    id: 't1',
    title: 'Refund flow',
    status: 'in-progress',
    goal: 'Support full and partial refunds across orders, billing and ledger.',
    projectNames: ['orders-service', 'billing-api', 'ledger-service'],
    progress: [
      'orders-service: refund endpoint drafted',
      'billing-api: provider refund call added'
    ],
    openQuestions: ['Which ledger entry kind records a partial refund?'],
    changedSinceCheckpoint: 3,
    updatedAt: ago(40 * MIN)
  },
  {
    id: 't2',
    title: 'Turkish locale for checkout',
    status: 'open',
    goal: 'Add missing tr keys in storefront-web and mobile-app.',
    projectNames: ['storefront-web', 'mobile-app'],
    progress: [],
    openQuestions: [],
    changedSinceCheckpoint: 0,
    updatedAt: ago(3 * DAY)
  },
  {
    id: 't3',
    title: 'Upgrade event schema to v2',
    status: 'blocked',
    goal: 'Move consumers to ledger.entry_posted v2.',
    projectNames: ['billing-api', 'contracts'],
    progress: ['contracts: v2 tagged'],
    openQuestions: ['Can notification-worker keep reading v1 for one more release?'],
    changedSinceCheckpoint: 1,
    updatedAt: ago(1 * DAY)
  }
];

export const rules: ArchRule[] = [
  {
    id: 'r1',
    name: 'Clients call services through generated clients',
    description: 'storefront-web and mobile-app must not build /v1 URLs by hand.',
    state: 'accepted',
    severity: 'warning',
    pack: 'openapi',
    approvedExamples: [{ path: 'src/api/generated/orders.ts', note: 'generated client call' }],
    violations: [
      {
        path: 'src/checkout/submitOrder.ts',
        lineStart: 14,
        message: "hand-written URL '/v1/orders'",
        projectName: 'storefront-web'
      }
    ]
  },
  {
    id: 'r2',
    name: "Services never read another service's tables",
    description: 'A table is written and read only by its owning service.',
    state: 'accepted',
    severity: 'error',
    approvedExamples: [],
    violations: []
  },
  {
    id: 'r3',
    name: 'Events are published after commit',
    description: 'Publish calls must follow the transaction commit.',
    state: 'proposed',
    severity: 'info',
    approvedExamples: [{ path: 'internal/orders/place.go', note: 'insert then publish' }],
    violations: []
  },
  {
    id: 'r4',
    name: 'No floating point money',
    description: 'Amounts use integer minor units.',
    state: 'accepted',
    severity: 'error',
    approvedExamples: [],
    violations: [
      {
        path: 'src/refunds/refund.service.ts',
        lineStart: 42,
        message: 'amount computed with Number division',
        projectName: 'billing-api'
      }
    ]
  }
];

// ---------------------------------------------------------------------------------------------

export function profiles(): EmbeddingProfile[] {
  return [
    {
      id: 'prof-gemini-768',
      name: 'Gemini 768',
      provider: 'gemini',
      model: 'gemini-embedding-2',
      dimensions: 768,
      storage: 'halfvec',
      locality: 'cloud-allowed',
      apiKey: 'env:GEMINI_API_KEY',
      budgets: { monthlyUsdMicros: 25_000_000, tokensPerMinute: 800_000, diskBytes: 6 * 1024 ** 3 },
      spentThisMonthUsdMicros: 4_310_000,
      measured: {
        recallAt10: 0.86,
        mrr: 0.71,
        querySet: 'shopfront-synthetic-v1',
        measuredAt: ago(6 * DAY),
        hardware: 'laptop, 8 cores, 32 GB'
      },
      active: true
    },
    {
      id: 'prof-gemini-1536',
      name: 'Gemini 1536',
      provider: 'gemini',
      model: 'gemini-embedding-2',
      dimensions: 1536,
      storage: 'halfvec',
      locality: 'cloud-allowed',
      apiKey: 'env:GEMINI_API_KEY',
      budgets: { monthlyUsdMicros: 25_000_000, diskBytes: 10 * 1024 ** 3 },
      spentThisMonthUsdMicros: 650_000,
      measured: null,
      active: false,
      switch: { state: 'building', progress: 0.18 }
    },
    {
      id: 'prof-local-bge',
      name: 'Local BGE small',
      provider: 'onnx-local',
      model: 'bge-small-en-v1.5',
      dimensions: 384,
      storage: 'fp32',
      locality: 'local-only',
      budgets: { diskBytes: 3 * 1024 ** 3 },
      spentThisMonthUsdMicros: 0,
      measured: {
        recallAt10: 0.74,
        mrr: 0.58,
        querySet: 'shopfront-synthetic-v1',
        measuredAt: ago(6 * DAY),
        hardware: 'laptop, 8 cores, 32 GB'
      },
      active: false
    },
    {
      id: 'prof-ollama',
      name: 'Ollama nomic',
      provider: 'ollama',
      model: 'nomic-embed-text',
      dimensions: 768,
      storage: 'fp32',
      locality: 'local-only',
      budgets: {},
      spentThisMonthUsdMicros: 0,
      measured: null,
      active: false
    }
  ];
}

export function evalReports(): EvalReport[] {
  const m = (r5: number, r10: number, mrr: number, ndcg: number) => ({
    recallAt5: r5,
    recallAt10: r10,
    mrr,
    ndcgAt10: ndcg
  });
  return [
    {
      id: 'ev-1',
      querySet: 'shopfront-synthetic-v1',
      profileId: 'prof-gemini-768',
      ranAt: ago(6 * DAY),
      hardware: 'laptop, 8 cores, 32 GB',
      dataset: 'synthetic fixture, 9 projects, 4.3k files',
      queryCount: 240,
      overall: m(0.79, 0.86, 0.71, 0.77),
      slices: [
        { dimension: 'retriever', value: 'bm25', queries: 240, metrics: m(0.66, 0.74, 0.58, 0.63) },
        { dimension: 'retriever', value: 'vector', queries: 240, metrics: m(0.7, 0.8, 0.62, 0.69) },
        {
          dimension: 'retriever',
          value: 'bm25+vector (RRF)',
          queries: 240,
          metrics: m(0.77, 0.84, 0.69, 0.75)
        },
        {
          dimension: 'retriever',
          value: 'fused + graph + rerank',
          queries: 240,
          metrics: m(0.79, 0.86, 0.71, 0.77)
        },
        {
          dimension: 'kind',
          value: 'exact-symbol',
          queries: 60,
          metrics: m(0.95, 0.97, 0.9, 0.92)
        },
        { dimension: 'kind', value: 'behavior', queries: 90, metrics: m(0.68, 0.79, 0.58, 0.66) },
        { dimension: 'kind', value: 'impact', queries: 50, metrics: m(0.74, 0.82, 0.63, 0.7) },
        { dimension: 'kind', value: 'why', queries: 40, metrics: m(0.7, 0.8, 0.6, 0.67) },
        { dimension: 'lang', value: 'TypeScript', queries: 80, metrics: m(0.82, 0.88, 0.74, 0.8) },
        { dimension: 'lang', value: 'Go', queries: 60, metrics: m(0.8, 0.87, 0.72, 0.78) },
        { dimension: 'lang', value: 'Rust', queries: 40, metrics: m(0.78, 0.85, 0.7, 0.76) },
        { dimension: 'lang', value: 'Dart', queries: 35, metrics: m(0.71, 0.8, 0.63, 0.7) },
        {
          dimension: 'lang',
          value: 'non-English query (tr)',
          queries: 25,
          metrics: m(0.64, 0.75, 0.52, 0.6)
        }
      ],
      badResults: [
        {
          query: 'where do we send the confirmation mail',
          expected: 'app/handlers/order_placed.py',
          got: 'src/email/templates.ts',
          queryClass: 'behavior'
        },
        {
          query: 'siparis iptali nerede',
          expected: 'internal/orders/cancel.go',
          got: 'internal/orders/place.go',
          queryClass: 'behavior'
        }
      ]
    },
    {
      id: 'ev-2',
      querySet: 'shopfront-synthetic-v1',
      profileId: 'prof-local-bge',
      ranAt: ago(6 * DAY),
      hardware: 'laptop, 8 cores, 32 GB',
      dataset: 'synthetic fixture, 9 projects, 4.3k files',
      queryCount: 240,
      overall: m(0.66, 0.74, 0.58, 0.64),
      slices: [
        { dimension: 'retriever', value: 'bm25', queries: 240, metrics: m(0.66, 0.74, 0.58, 0.63) },
        {
          dimension: 'retriever',
          value: 'vector',
          queries: 240,
          metrics: m(0.58, 0.69, 0.5, 0.56)
        },
        { dimension: 'kind', value: 'behavior', queries: 90, metrics: m(0.55, 0.66, 0.47, 0.54) },
        {
          dimension: 'lang',
          value: 'non-English query (tr)',
          queries: 25,
          metrics: m(0.4, 0.52, 0.33, 0.4)
        }
      ],
      badResults: []
    }
  ];
}

export function usage(days: number): UsageReport {
  const daily = Array.from({ length: days }, (_, i) => {
    const d = new Date(Date.now() - (days - 1 - i) * DAY);
    const wave = 1 + Math.sin(i * 0.9) * 0.4;
    const weekend = d.getDay() === 0 || d.getDay() === 6 ? 0.3 : 1;
    const calls = Math.round(180 * wave * weekend);
    return {
      date: d.toISOString().slice(0, 10),
      calls,
      tokensReturned: calls * 740,
      spendUsdMicros: calls * 1900
    };
  });
  return {
    periodDays: days,
    tools: [
      {
        tool: 'search_code',
        calls: 2410,
        errors: 4,
        tokensReturned: 1_980_000,
        p50Ms: 84,
        p95Ms: 310,
        spendUsdMicros: 2_900_000
      },
      {
        tool: 'open_workspace',
        calls: 310,
        errors: 0,
        tokensReturned: 420_000,
        p50Ms: 120,
        p95Ms: 260,
        spendUsdMicros: 0
      },
      {
        tool: 'find_references',
        calls: 880,
        errors: 11,
        tokensReturned: 520_000,
        p50Ms: 45,
        p95Ms: 180,
        spendUsdMicros: 0
      },
      {
        tool: 'write_memory',
        calls: 96,
        errors: 2,
        tokensReturned: 12_000,
        p50Ms: 30,
        p95Ms: 90,
        spendUsdMicros: 0
      },
      {
        tool: 'resume_task',
        calls: 58,
        errors: 0,
        tokensReturned: 160_000,
        p50Ms: 150,
        p95Ms: 340,
        spendUsdMicros: 0
      }
    ],
    agents: [
      {
        agent: 'claude (sess-7f3a)',
        sessions: 41,
        calls: 2200,
        tokensReturned: 1_700_000,
        lastSeen: ago(40 * MIN)
      },
      {
        agent: 'codex (sess-19bc)',
        sessions: 22,
        calls: 1100,
        tokensReturned: 900_000,
        lastSeen: ago(5 * HOUR)
      },
      { agent: 'cursor', sessions: 9, calls: 450, tokensReturned: 490_000, lastSeen: ago(2 * DAY) }
    ],
    daily
  };
}

export const integrations: IntegrationsStatus = {
  mcp: {
    transport: 'stdio',
    endpoint: 'know mcp (stdio)',
    status: 'connected',
    lastCallAt: ago(40 * MIN)
  },
  agents: [
    {
      agent: 'claude',
      configured: true,
      mcpConfig: 'present',
      instructionFile: 'present',
      sessionStartHook: 'present',
      diagnostics: [
        { check: 'MCP server answers initialize', ok: true, detail: '12 ms' },
        { check: 'instructions field delivered', ok: true, detail: 'present' },
        { check: 'SessionStart hook runs', ok: true, detail: 'last run 40 min ago' }
      ]
    },
    {
      agent: 'codex',
      configured: true,
      mcpConfig: 'outdated',
      instructionFile: 'present',
      sessionStartHook: 'not-applicable',
      diagnostics: [
        { check: 'MCP server answers initialize', ok: true, detail: '14 ms' },
        {
          check: 'config matches this engine version',
          ok: false,
          detail: 'config written by 0.0.9; run `know connect codex`'
        }
      ]
    },
    {
      agent: 'cursor',
      configured: false,
      mcpConfig: 'missing',
      instructionFile: 'missing',
      sessionStartHook: 'not-applicable',
      diagnostics: [
        {
          check: 'MCP configuration exists',
          ok: false,
          detail: 'run `know connect cursor` in the repository'
        }
      ]
    }
  ],
  webhooks: [
    { provider: 'github', enabled: true, lastDeliveryAt: ago(12 * MIN) },
    { provider: 'gitlab', enabled: false },
    { provider: 'gitea', enabled: false }
  ]
};

export function admin(role: 'standalone' | 'hub'): AdminOverview {
  if (role !== 'hub') return { available: false, role, users: [], tokens: [], audit: [] };
  return {
    available: true,
    role,
    users: [
      {
        id: 'u1',
        name: 'ada',
        email: 'ada@example.test',
        role: 'admin',
        lastLoginAt: ago(1 * HOUR),
        disabled: false
      },
      {
        id: 'u2',
        name: 'linus',
        email: 'linus@example.test',
        role: 'maintainer',
        lastLoginAt: ago(1 * DAY),
        disabled: false
      },
      { id: 'u3', name: 'grace', email: 'grace@example.test', role: 'reader', disabled: true }
    ],
    tokens: [
      {
        id: 'tk1',
        name: 'ci-know-check',
        prefix: 'kw_ci_a1b2',
        scopes: ['read'],
        createdAt: ago(30 * DAY),
        expiresAt: ahead(60 * DAY)
      }
    ],
    audit: [
      {
        id: 'a1',
        at: ago(1 * HOUR),
        actor: 'ada',
        action: 'memory.accept',
        target: 'm1',
        outcome: 'success'
      },
      {
        id: 'a2',
        at: ago(3 * HOUR),
        actor: 'grace',
        action: 'profile.switch',
        target: 'prof-gemini-1536',
        outcome: 'denied'
      }
    ]
  };
}
