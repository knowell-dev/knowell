/**
 * Turns the rich mock fixtures into what the real server reports today: every field the server
 * "cannot know yet" is `null`. The mock uses this when created with `shape: 'server'`, so the
 * screens can be exercised against the same nulls the Rust server sends.
 */
import type {
  EngineHealth,
  IndexesOverview,
  ProjectDetail,
  ProjectSummary,
  WorkspaceDetail,
  WorkspaceSummary
} from './types';

export function serverHealth(h: EngineHealth): EngineHealth {
  return { ...h, freshness: null, recentErrors: null, resources: null };
}

export function serverWorkspace<T extends WorkspaceSummary>(w: T, index = 0): T {
  const base = {
    ...w,
    description: null,
    memberCount: null,
    embeddingProfileId: null,
    // Every second workspace pretends its knowell.toml could not be loaded.
    ...(index % 2 === 1
      ? {
          trackedRef: null,
          dataPolicy: null,
          settingsError:
            'the workspace configuration could not be loaded; run `know doctor` for details'
        }
      : {})
  };
  return 'members' in base ? ({ ...base, members: null } as unknown as T) : (base as T);
}

export function serverWorkspaceDetail(w: WorkspaceDetail, index = 0): WorkspaceDetail {
  return serverWorkspace(w, index);
}

export function serverProjectSummary(p: ProjectSummary): ProjectSummary {
  return { ...p, kind: null, languages: null, fileCount: null };
}

export function serverProject(p: ProjectDetail): ProjectDetail {
  return {
    ...serverProjectSummary(p),
    source: p.source,
    root: { value: p.root.value, origin: p.root.origin === 'project' ? 'project' : 'builtin' },
    trackedRef: p.trackedRef,
    excludes: p.excludes,
    embedding: p.embedding,
    embeddingProfileId: null,
    dataPolicy: p.dataPolicy,
    analysis: null,
    worktrees: null,
    sensitiveExcludedCount: null,
    views: p.views,
    indexed: p.indexed,
    lastIndexedAt: p.lastIndexedAt
  } as ProjectDetail;
}

export function serverIndexes(i: IndexesOverview, orgWide: boolean): IndexesOverview {
  return {
    views: i.views.map((v) => ({
      ...v,
      tiers: null,
      analysis: null,
      generations: v.generations.map((g) => ({
        ...g,
        profileId: null,
        chunkCount: null,
        progress: null
      }))
    })),
    jobs: orgWide ? (i.jobs?.map((j) => ({ ...j, progress: null })) ?? null) : null,
    deadLetters: orgWide ? i.deadLetters : null,
    migrations: null
  };
}
