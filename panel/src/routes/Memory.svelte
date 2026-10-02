<script lang="ts">
  import { getApi, type MemoryRecord, type MemoryScope, type MemoryState } from '$lib/api';
  import { useResource } from '$lib/resource.svelte';
  import { relativeTime, shortSha } from '$lib/format';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import DataState from '$lib/components/DataState.svelte';
  import Card from '$lib/components/Card.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import Tabs from '$lib/components/Tabs.svelte';

  const api = getApi();
  const res = useResource(() => api.listMemory());
  const tasks = useResource(() => api.listTasks());

  let tab = $state('proposals');
  let scope = $state<MemoryScope | 'all'>('all');
  let busy = $state<string | undefined>();
  let notice = $state<{ kind: 'ok' | 'error'; text: string } | undefined>();

  const SCOPES: (MemoryScope | 'all')[] = [
    'all',
    'organization',
    'workspace',
    'project',
    'task',
    'user'
  ];
  const stateTone = (s: MemoryState) =>
    s === 'accepted'
      ? 'ok'
      : s === 'proposed'
        ? 'info'
        : s === 'stale'
          ? 'warn'
          : s === 'rejected'
            ? 'danger'
            : 'neutral';

  function filter(all: MemoryRecord[], t: string): MemoryRecord[] {
    const inScope = all.filter((m) => scope === 'all' || m.scope === scope);
    switch (t) {
      case 'proposals':
        return inScope.filter((m) => m.state === 'proposed');
      case 'conflicts':
        return inScope.filter((m) => (m.conflictsWith?.length ?? 0) > 0);
      case 'stale':
        return inScope.filter((m) => m.state === 'stale');
      default:
        return inScope;
    }
  }
  const EMPTY: Record<string, string> = {
    proposals:
      'No proposals are waiting. Agent findings and model descriptions arrive here as drafts and never become rules without review.',
    conflicts: 'No two records disagree. Conflicts are surfaced here rather than merged silently.',
    stale:
      'No record has stale evidence. When the source a record cites changes, it moves here for re-evaluation.',
    all: 'No memory records match this scope. Records are written by people, by the engine (observations) and by agents through write_memory.'
  };

  async function decide(m: MemoryRecord, action: 'accept' | 'reject') {
    busy = m.id;
    notice = undefined;
    try {
      await api.decideMemory({ id: m.id, action });
      notice = {
        kind: 'ok',
        text: `${action === 'accept' ? 'Accepted' : 'Rejected'} "${m.title}".`
      };
      await res.reload();
    } catch (e) {
      notice = { kind: 'error', text: e instanceof Error ? e.message : 'decision failed' };
    } finally {
      busy = undefined;
    }
  }
</script>

<PageHeader
  title="Memory"
  lead="Decisions, notes and findings by scope and state. Agent remarks are drafts until a person accepts them. Secrets cannot enter memory."
/>

{#if notice}
  <div class="notice {notice.kind}" role={notice.kind === 'error' ? 'alert' : 'status'}>
    {notice.text}
  </div>
{/if}

<DataState
  resource={res}
  isEmpty={(d) => d.length === 0}
  emptyTitle="Memory is empty"
  emptyWhy="No record has been written yet. Observations appear after indexing; decisions and findings appear when people or connected agents write them."
>
  {#snippet children(all)}
    <div class="row bar">
      <label class="field inline"
        >Scope
        <select bind:value={scope}>
          {#each SCOPES as s (s)}<option value={s}>{s}</option>{/each}
        </select>
      </label>
    </div>
    <Tabs
      label="Memory views"
      bind:active={tab}
      tabs={[
        { id: 'proposals', label: 'Proposal queue', count: filter(all, 'proposals').length },
        { id: 'conflicts', label: 'Conflicts', count: filter(all, 'conflicts').length },
        { id: 'stale', label: 'Stale', count: filter(all, 'stale').length },
        { id: 'all', label: 'All records', count: filter(all, 'all').length },
        { id: 'tasks', label: 'Tasks', count: tasks.data?.length }
      ]}
    />
    <div class="list">
      {#if tab === 'tasks'}
        <DataState
          resource={tasks}
          isEmpty={(d) => d.length === 0}
          emptyTitle="No tasks"
          emptyWhy="Tasks are created by agents through save_checkpoint or by people. None exist yet."
        >
          {#snippet children(list)}
            <div class="stack">
              {#each list as t (t.id)}
                <Card title={t.title} subtitle={t.goal}>
                  {#snippet actions()}<Badge
                      tone={t.status === 'blocked'
                        ? 'danger'
                        : t.status === 'done'
                          ? 'ok'
                          : 'accent'}>{t.status}</Badge
                    >{/snippet}
                  <p class="small muted">
                    Projects: {t.projectNames.join(', ')} - updated {relativeTime(t.updatedAt)}
                  </p>
                  {#if t.changedSinceCheckpoint > 0}<p>
                      <Badge tone="warn"
                        >{t.changedSinceCheckpoint} sources changed since the last checkpoint</Badge
                      >
                    </p>{/if}
                  {#if t.progress.length}<h3>Progress</h3>
                    <ul>
                      {#each t.progress as p (p)}<li>{p}</li>{/each}
                    </ul>{/if}
                  {#if t.openQuestions.length}<h3>Open questions</h3>
                    <ul>
                      {#each t.openQuestions as q (q)}<li>{q}</li>{/each}
                    </ul>{/if}
                </Card>
              {/each}
            </div>
          {/snippet}
        </DataState>
      {:else}
        {@const rows = filter(all, tab)}
        {#if rows.length === 0}
          <div class="empty" role="status">
            <h3>Nothing in this view</h3>
            <p class="muted">{EMPTY[tab]}</p>
          </div>
        {:else}
          <div class="stack">
            {#each rows as m (m.id)}
              <Card>
                <div class="row">
                  <strong>{m.title}</strong>
                  <Badge tone={stateTone(m.state)}>{m.state}</Badge>
                  <Badge>{m.scope}: {m.scopeName}</Badge>
                  <Badge title="Record kind">{m.kind}</Badge>
                  {#if m.pinned}<Badge tone="accent">pinned</Badge>{/if}
                  <span class="spacer"></span>
                  <span class="faint small"
                    >v{m.version} - {m.author.type}
                    {m.author.name}{m.author.session ? ` (${m.author.session})` : ''} - {relativeTime(
                      m.createdAt
                    )}</span
                  >
                </div>
                <p class="body">{m.body}</p>
                {#if m.staleReason}<p class="warnbox">Stale: {m.staleReason}</p>{/if}
                {#if m.supersededBy}<p class="faint small">
                    Superseded by <code>{m.supersededBy}</code>; kept for history, not shown as a
                    current rule.
                  </p>{/if}
                {#if m.conflictsWith?.length}
                  <p class="warnbox">
                    Conflicts with
                    {#each m.conflictsWith as c (c)}
                      <code>{c}</code> ({all.find((x) => x.id === c)?.title ?? 'unknown record'})
                    {/each}. Resolve by accepting one and rejecting the other; nothing is merged
                    automatically.
                  </p>
                {/if}
                {#if m.evidence.length}
                  <ul class="evidence">
                    {#each m.evidence as ev (ev.path + ev.lineStart)}
                      <li class="mono small">
                        {ev.path}:{ev.lineStart}-{ev.lineEnd} @ {shortSha(ev.commit)}
                        <Badge tone={ev.stillValid ? 'ok' : 'warn'}
                          >{ev.stillValid ? 'still matches' : 'changed'}</Badge
                        >
                      </li>
                    {/each}
                  </ul>
                {:else}
                  <p class="faint small">No evidence attached.</p>
                {/if}
                {#if m.state === 'proposed' || m.state === 'stale'}
                  <div class="row">
                    <button
                      class="btn primary sm"
                      type="button"
                      disabled={busy !== undefined}
                      onclick={() => decide(m, 'accept')}>Accept</button
                    >
                    <button
                      class="btn danger sm"
                      type="button"
                      disabled={busy !== undefined}
                      onclick={() => decide(m, 'reject')}>Reject</button
                    >
                  </div>
                {/if}
              </Card>
            {/each}
          </div>
        {/if}
      {/if}
    </div>
  {/snippet}
</DataState>

<style>
  .bar {
    margin-bottom: var(--sp-3);
  }
  .field.inline {
    flex-direction: row;
    align-items: center;
    gap: var(--sp-2);
  }
  .list {
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
  .body {
    margin: var(--sp-2) 0;
  }
  .warnbox {
    border: 1px solid var(--warn);
    background: var(--warn-bg);
    border-radius: var(--radius);
    padding: var(--sp-2) var(--sp-3);
    margin: var(--sp-2) 0;
  }
  .evidence {
    list-style: none;
    padding: 0;
    margin: var(--sp-2) 0;
  }
  .empty {
    border: 1px dashed var(--border-strong);
    border-radius: var(--radius);
    padding: var(--sp-5);
    text-align: center;
  }
  h3 {
    margin-top: var(--sp-2);
  }
</style>
