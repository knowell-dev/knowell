<script lang="ts">
  import { onMount } from 'svelte';
  import {
    ApiError,
    getApi,
    type IndexView,
    type GenerationState,
    type ProgressEvent,
    type ViewState
  } from '$lib/api';
  import { useResource } from '$lib/resource.svelte';
  import { formatNumber, formatPercent, relativeTime, shortSha } from '$lib/format';
  import { href } from '$lib/router.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import DataState from '$lib/components/DataState.svelte';
  import Card from '$lib/components/Card.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import Meter from '$lib/components/Meter.svelte';
  import Tabs from '$lib/components/Tabs.svelte';
  import JobsTable from '$lib/components/JobsTable.svelte';
  import NotReported from '$lib/components/NotReported.svelte';
  import EmptyState from '$lib/components/EmptyState.svelte';

  const api = getApi();
  const res = useResource(() => api.getIndexes());
  let tab = $state('views');
  let notice = $state<{ kind: 'ok' | 'error'; text: string } | undefined>();
  let busy = $state<string | undefined>();
  let expanded = $state<string | undefined>();
  let live = $state<'connecting' | 'live' | 'interrupted'>('connecting');
  let progress = $state<Record<string, number>>({});

  onMount(() =>
    api.subscribeProgress(
      (e: ProgressEvent) => {
        live = 'live';
        if (e.type === 'resync') {
          // The stream lost events: refetch everything instead of trusting partial state.
          void res.reload();
        } else if (e.type === 'job') {
          if (e.job.progress !== null) progress[e.job.id] = e.job.progress;
          const known = res.data?.jobs?.find((j) => j.id === e.job.id);
          if (!known || known.state !== e.job.state) void res.reload();
        } else if (e.type === 'generation') {
          const view = res.data?.views.find((v) => v.id === e.viewId);
          const known = view?.generations.find((g) => g.id === e.generation.id);
          if (!known || known.state !== e.generation.state) void res.reload();
          else if (e.generation.progress !== null)
            progress[`gen:${e.viewId}:${e.generation.id}`] = e.generation.progress;
        }
      },
      () => (live = 'interrupted')
    )
  );

  const problem = (e: unknown, fallback: string) => (e instanceof Error ? e.message : fallback);

  async function reindex(v: IndexView, scope: 'changed' | 'full') {
    busy = v.id + scope;
    notice = undefined;
    try {
      const r = await api.reindex({ viewId: v.id, scope });
      const what = scope === 'full' ? 'full' : 'incremental';
      notice = {
        kind: 'ok',
        text: r.created
          ? `Queued ${what} reindex of ${v.projectName} as ${r.jobId}.`
          : `A ${what} reindex of ${v.projectName} was already queued as ${r.jobId}.`
      };
      await res.reload();
    } catch (e) {
      notice = { kind: 'error', text: problem(e, 'reindex failed') };
    } finally {
      busy = undefined;
    }
  }

  async function retry(jobId: string) {
    busy = jobId;
    notice = undefined;
    try {
      await api.retryDeadLetter(jobId);
      notice = { kind: 'ok', text: `Re-queued ${jobId}.` };
      await res.reload();
    } catch (e) {
      if (e instanceof ApiError && e.code === 'not_dead') {
        notice = {
          kind: 'error',
          text: `${jobId} is no longer dead-lettered (someone may have retried it already). The list was refreshed.`
        };
        await res.reload();
      } else if (e instanceof ApiError && e.status === 404) {
        notice = { kind: 'error', text: `${jobId} no longer exists. The list was refreshed.` };
        await res.reload();
      } else {
        notice = { kind: 'error', text: problem(e, 'retry failed') };
      }
    } finally {
      busy = undefined;
    }
  }

  const genTone = (s: GenerationState) =>
    s === 'active' ? 'ok' : s === 'failed' ? 'danger' : s === 'building' ? 'accent' : 'neutral';
  const viewTone = (s: ViewState) =>
    s === 'ready'
      ? 'ok'
      : s === 'building'
        ? 'accent'
        : s === 'stale' || s === 'not-indexed'
          ? 'warn'
          : 'danger';
</script>

<PageHeader
  title="Indexes"
  lead="Views (a project at a tracked ref), index generations, freshness tiers, analysis coverage, the job queue and profile migrations."
>
  {#snippet actions()}
    <span class="small muted" aria-live="polite">Progress stream: {live}</span>
    <button class="btn" type="button" onclick={() => res.reload()} disabled={res.loading}
      >Refresh</button
    >
  {/snippet}
</PageHeader>

{#if notice}
  <div class="notice {notice.kind}" role={notice.kind === 'error' ? 'alert' : 'status'}>
    {notice.text}
  </div>
{/if}

<DataState
  resource={res}
  isEmpty={(d) => d.views.length === 0 && (d.jobs?.length ?? 0) === 0}
  emptyTitle="No indexes yet"
  emptyWhy="No project has been indexed. Add a project to a workspace and run `know index`, or start a reindex from here once a view exists."
>
  {#snippet children(d)}
    <Tabs
      label="Index sections"
      bind:active={tab}
      tabs={[
        { id: 'views', label: 'Views', count: d.views.length },
        { id: 'jobs', label: 'Jobs', count: d.jobs?.length },
        { id: 'dlq', label: 'Dead-letter queue', count: d.deadLetters?.length },
        { id: 'migrations', label: 'Profile migrations', count: d.migrations?.length }
      ]}
    />
    <div class="panel">
      {#if tab === 'views'}
        <div class="stack">
          {#each d.views as v (v.id)}
            <Card
              title={v.projectName}
              subtitle="{v.workspaceName} - tracking {v.trackTarget.kind} {v.trackTarget.name ||
                v.trackTarget.text}"
            >
              {#snippet actions()}
                <Badge tone={viewTone(v.state)}>{v.state}</Badge>
                <button
                  class="btn sm"
                  type="button"
                  disabled={busy !== undefined}
                  onclick={() => reindex(v, 'changed')}>Reindex changed</button
                >
                <button
                  class="btn sm"
                  type="button"
                  disabled={busy !== undefined}
                  onclick={() => reindex(v, 'full')}>Full reindex</button
                >
              {/snippet}
              <dl class="commits">
                <div>
                  <dt>Track target</dt>
                  <dd><code>{v.trackTarget.text}</code></dd>
                </div>
                <div>
                  <dt>Last seen commit</dt>
                  <dd class="mono">{shortSha(v.lastSeenCommit)}</dd>
                </div>
                <div>
                  <dt>Active index commit</dt>
                  <dd class="mono">
                    {shortSha(v.activeIndexCommit)}
                    {#if v.lastSeenCommit && v.activeIndexCommit && v.lastSeenCommit !== v.activeIndexCommit}
                      <Badge tone="warn">behind tracked head</Badge>
                    {/if}
                  </dd>
                </div>
              </dl>
              {#if v.stateNote}<p class="muted small">{v.stateNote}</p>{/if}

              <div class="grid cols-2 inner">
                <div>
                  <h3>Freshness tiers</h3>
                  {#if v.tiers === null}
                    <p class="muted small"><NotReported /> - tier coverage is not tracked yet.</p>
                  {:else}
                    {#each v.tiers as t (t.tier)}
                      <div class="trow">
                        <span class="mono">{t.tier}</span><Meter
                          value={t.coverage}
                          label="{v.projectName} {t.tier} coverage"
                          tone={t.coverage >= 0.98 ? 'ok' : 'warn'}
                        /><span class="small muted num">{t.filesBehind} behind</span>
                      </div>
                    {/each}
                  {/if}
                </div>
                <div>
                  <h3>Analysis coverage</h3>
                  {#if v.analysis === null}
                    <p class="muted small">
                      <NotReported /> - per-language analysis coverage is not tracked yet.
                    </p>
                  {:else}
                    <table class="table">
                      <thead
                        ><tr
                          ><th>Language</th><th class="num">Files</th><th class="num">Syntactic</th
                          ><th class="num">Semantic (SCIP)</th></tr
                        ></thead
                      >
                      <tbody>
                        {#each v.analysis as a (a.language)}
                          <tr>
                            <td>{a.language}</td>
                            <td class="num">{formatNumber(a.files)}</td>
                            <td class="num">{formatPercent(a.syntactic)}</td>
                            <td class="num"
                              >{#if a.semantic === null}<Badge tone="neutral" title={a.note}
                                  >not configured</Badge
                                >{:else}{formatPercent(a.semantic)}{/if}</td
                            >
                          </tr>
                        {/each}
                      </tbody>
                    </table>
                  {/if}
                </div>
              </div>

              <button
                class="btn ghost sm"
                type="button"
                aria-expanded={expanded === v.id}
                onclick={() => (expanded = expanded === v.id ? undefined : v.id)}
              >
                {expanded === v.id ? 'Hide' : 'Show'} generations ({v.generations.length})
              </button>
              {#if expanded === v.id}
                {#if v.generations.length === 0}
                  <p class="muted small">No generation has been built for this view yet.</p>
                {:else}
                  <div class="table-wrap">
                    <table class="table">
                      <thead
                        ><tr
                          ><th>Gen</th><th>State</th><th>Commit</th><th>Profile</th><th class="num"
                            >Chunks</th
                          ><th>Created</th><th>Activated</th><th>Finished</th></tr
                        ></thead
                      >
                      <tbody>
                        {#each v.generations as g (g.id)}
                          {@const pr = progress[`gen:${v.id}:${g.id}`] ?? g.progress}
                          <tr>
                            <td class="mono">#{g.id}</td>
                            <td
                              ><Badge tone={genTone(g.state)}>{g.state}</Badge
                              >{#if g.state === 'building'}
                                {#if pr !== null && pr !== undefined}
                                  <span class="small muted">{formatPercent(pr)}</span>
                                {:else}
                                  <span class="small faint">progress not reported yet</span>
                                {/if}
                              {/if}{#if g.error}<div class="small danger-text">
                                  {g.error}
                                </div>{/if}</td
                            >
                            <td class="mono"
                              >{#if g.commit}{shortSha(g.commit)}{:else}<span
                                  class="faint"
                                  title="Directory sources have no commit">none</span
                                >{/if}</td
                            >
                            <td
                              >{#if g.profileId}<code>{g.profileId}</code>{:else}<NotReported
                                />{/if}</td
                            >
                            <td class="num"
                              >{#if g.chunkCount !== null}{formatNumber(
                                  g.chunkCount
                                )}{:else}<NotReported />{/if}</td
                            >
                            <td>{relativeTime(g.createdAt)}</td>
                            <td>{g.activatedAt ? relativeTime(g.activatedAt) : '-'}</td>
                            <td>{g.finishedAt ? relativeTime(g.finishedAt) : '-'}</td>
                          </tr>
                        {/each}
                      </tbody>
                    </table>
                  </div>
                {/if}
              {/if}
            </Card>
          {/each}
          {#if d.views.length === 0}
            <p class="muted">No views yet: no project has been seen at its tracked ref.</p>
          {/if}
        </div>
      {:else if tab === 'jobs'}
        {#if d.jobs === null}
          <EmptyState
            title="Job queue not visible"
            why="Listing jobs needs organization-wide read access, which this session does not have. Views and generations above are still shown for the projects you can see."
          />
        {:else}
          <Card flush>
            {#if d.jobs.length === 0}
              <p class="muted pad">
                The queue is empty. Jobs appear here when files change or a reindex is requested.
              </p>
            {:else}
              <JobsTable jobs={d.jobs} {progress} />
              <p class="small muted pad">
                Filter and page through all jobs on the <a href={href('/jobs')}>Jobs</a> screen.
              </p>
            {/if}
          </Card>
        {/if}
      {:else if tab === 'dlq'}
        {#if d.deadLetters === null}
          <EmptyState
            title="Dead-letter queue not visible"
            why="The dead-letter queue needs organization-wide read access, which this session does not have."
          />
        {:else}
          <Card
            title="Dead-letter queue"
            subtitle="Jobs that exhausted their retries. Nothing here is retried automatically."
            flush
          >
            {#if d.deadLetters.length === 0}
              <p class="muted pad">
                No dead letters. Jobs land here only after every retry failed.
              </p>
            {:else}
              <div class="table-wrap">
                <table class="table">
                  <thead
                    ><tr
                      ><th>Job</th><th>Kind</th><th>Workspace / project</th><th>Failed</th><th
                        class="num">Attempts</th
                      ><th>Error</th><th></th></tr
                    ></thead
                  >
                  <tbody>
                    {#each d.deadLetters as x (x.jobId)}
                      <tr>
                        <td class="mono">{x.jobId}</td>
                        <td><code>{x.kind}</code></td>
                        <td>
                          {#if x.projectName}{x.workspaceName
                              ? `${x.workspaceName} / `
                              : ''}{x.projectName}{:else if x.workspaceName}{x.workspaceName}{:else}<span
                              class="faint"
                              title="The job payload names no project">not named</span
                            >{/if}
                        </td>
                        <td>{relativeTime(x.failedAt)}</td>
                        <td class="num">{x.attempts}</td>
                        <td
                          >{#if x.error}{x.error}{:else}<span class="faint"
                              >no error message recorded</span
                            >{/if}</td
                        >
                        <td
                          ><button
                            class="btn sm"
                            type="button"
                            disabled={busy !== undefined}
                            onclick={() => retry(x.jobId)}>Retry</button
                          ></td
                        >
                      </tr>
                    {/each}
                  </tbody>
                </table>
              </div>
            {/if}
          </Card>
        {/if}
      {:else if d.migrations === null}
        <EmptyState
          title="Profile migrations are reported by the engine"
          why="The server does not track profile migrations itself. They appear here once the engine reports them."
        />
      {:else}
        <div class="stack">
          {#if d.migrations.length === 0}
            <p class="muted">
              No profile migration is running. Switch profiles from <a href={href('/models')}
                >Model profiles</a
              >.
            </p>
          {/if}
          {#each d.migrations as m (m.id)}
            <Card
              title="{m.fromProfileId} to {m.toProfileId}"
              subtitle="Blue-green: the old index keeps serving until the new one passes quality and coverage checks."
            >
              <div class="row">
                <Badge tone="accent">{m.state}</Badge>
                <div class="grow"><Meter value={m.progress} label="Migration progress" /></div>
              </div>
              {#if m.reversibleUntil}<p class="muted small">
                  Reversible until {new Date(m.reversibleUntil).toLocaleDateString()}.
                </p>{/if}
            </Card>
          {/each}
        </div>
      {/if}
    </div>
  {/snippet}
</DataState>

<style>
  .panel {
    margin-top: var(--sp-4);
  }
  .notice {
    padding: var(--sp-2) var(--sp-3);
    border-radius: var(--radius);
    border: 1px solid var(--ok);
    background: var(--ok-bg);
    margin-bottom: var(--sp-3);
  }
  .notice.error {
    border-color: var(--danger);
    background: var(--danger-bg);
  }
  .commits {
    display: flex;
    gap: var(--sp-5);
    flex-wrap: wrap;
    margin: 0 0 var(--sp-3);
  }
  dt {
    color: var(--text-muted);
    font-size: var(--fs-xs);
    text-transform: uppercase;
  }
  dd {
    margin: 0;
  }
  .inner {
    margin: var(--sp-3) 0;
  }
  h3 {
    margin-bottom: var(--sp-2);
  }
  .trow {
    display: grid;
    grid-template-columns: 2rem 1fr 6rem;
    gap: var(--sp-2);
    align-items: center;
    margin-bottom: var(--sp-1);
  }
  .pad {
    padding: var(--sp-4);
  }
  .danger-text {
    color: var(--danger);
  }
  .grow {
    flex: 1;
    min-width: 10rem;
  }
</style>
