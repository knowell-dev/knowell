<script lang="ts">
  import { onMount } from 'svelte';
  import { getApi, type JobState, type ProgressEvent } from '$lib/api';
  import { useResource } from '$lib/resource.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import DataState from '$lib/components/DataState.svelte';
  import Card from '$lib/components/Card.svelte';
  import JobsTable from '$lib/components/JobsTable.svelte';

  const api = getApi();
  const STATES: (JobState | '')[] = [
    '',
    'queued',
    'running',
    'failed',
    'dead',
    'succeeded',
    'cancelled'
  ];
  let stateFilter = $state<JobState | ''>('');
  let limit = $state(100);
  let live = $state<'connecting' | 'live' | 'interrupted'>('connecting');
  let progress = $state<Record<string, number>>({});

  const res = useResource(() =>
    api.listJobs({ state: stateFilter === '' ? undefined : stateFilter, limit })
  );

  onMount(() =>
    api.subscribeProgress(
      (e: ProgressEvent) => {
        live = 'live';
        if (e.type === 'resync') void res.reload();
        else if (e.type === 'job' && e.job.progress !== null) progress[e.job.id] = e.job.progress;
      },
      () => (live = 'interrupted')
    )
  );
</script>

<PageHeader
  title="Jobs"
  lead="The durable job queue: reindex requests, repository refreshes from webhooks and their outcome. Dead-lettered jobs can be retried from Indexes."
>
  {#snippet actions()}
    <span class="small muted" aria-live="polite">Progress stream: {live}</span>
    <button class="btn" type="button" onclick={() => res.reload()} disabled={res.loading}
      >Refresh</button
    >
  {/snippet}
</PageHeader>

<div class="filters">
  <label>
    State
    <select bind:value={stateFilter} onchange={() => res.load()}>
      {#each STATES as s (s)}<option value={s}>{s === '' ? 'all states' : s}</option>{/each}
    </select>
  </label>
  <label>
    Show at most
    <select bind:value={limit} onchange={() => res.load()}>
      {#each [25, 100, 200] as n (n)}<option value={n}>{n} jobs</option>{/each}
    </select>
  </label>
</div>

<DataState
  resource={res}
  isEmpty={(d) => d.length === 0}
  emptyTitle="No jobs"
  emptyWhy="No job matches this filter. Jobs appear when a reindex is requested or a webhook reports a push."
>
  {#snippet children(jobs)}
    <Card flush>
      <JobsTable {jobs} {progress} />
    </Card>
  {/snippet}
</DataState>

<style>
  .filters {
    display: flex;
    gap: var(--sp-4);
    margin-bottom: var(--sp-3);
    flex-wrap: wrap;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: var(--sp-1);
    font-size: var(--fs-sm);
    color: var(--text-muted);
  }
</style>
