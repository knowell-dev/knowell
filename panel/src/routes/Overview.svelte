<script lang="ts">
  import { getApi, type HealthStatus } from '$lib/api';
  import { useResource } from '$lib/resource.svelte';
  import { formatBytes, formatDuration, formatPercent, relativeTime } from '$lib/format';
  import { href } from '$lib/router.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import DataState from '$lib/components/DataState.svelte';
  import Card from '$lib/components/Card.svelte';
  import Stat from '$lib/components/Stat.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import Meter from '$lib/components/Meter.svelte';
  import NotReported from '$lib/components/NotReported.svelte';

  const api = getApi();
  const health = useResource(() => api.getHealth());

  const tone = (s: HealthStatus) => (s === 'ok' ? 'ok' : s === 'degraded' ? 'warn' : 'danger');
  const TIER_LABEL = {
    T0: 'T0 file text and path',
    T1: 'T1 symbols and imports',
    T2: 'T2 embeddings',
    T3: 'T3 relations and links'
  } as const;
</script>

<PageHeader
  title="Overview"
  lead="Engine health, work queue, index freshness, recent errors and resource use."
>
  {#snippet actions()}
    <button class="btn" type="button" onclick={() => health.reload()} disabled={health.loading}
      >Refresh</button
    >
  {/snippet}
</PageHeader>

<DataState resource={health}>
  {#snippet children(h)}
    <div class="stack">
      <div class="grid cols-4">
        <Stat
          label="Engine"
          value={h.status}
          tone={h.status === 'ok' ? 'ok' : h.status === 'degraded' ? 'warn' : 'danger'}
          hint="v{h.version} - {h.role} - up {formatDuration(h.uptimeMs)}"
        />
        {#if h.queue}
          <Stat
            label="Queue"
            value="{h.queue.queued} queued"
            hint="{h.queue.running} running - {h.queue.oldestQueuedMs === null
              ? 'nothing waiting'
              : `oldest ${formatDuration(h.queue.oldestQueuedMs)}`}"
          />
          <Stat
            label="Failed jobs"
            value={String(h.queue.failed)}
            tone={h.queue.failed > 0 ? 'warn' : 'ok'}
            hint="{h.queue.deadLetter} in the dead-letter queue"
          />
        {:else}
          <Stat
            label="Queue"
            value="not reported yet"
            hint="The job queue needs a database; none is connected."
          />
          <Stat
            label="Failed jobs"
            value="not reported yet"
            hint="Shown together with the queue."
          />
        {/if}
        <Stat
          label="Listening on"
          value={h.bindAddress}
          hint="local only unless the hub role is configured"
        />
      </div>

      <div class="grid cols-2">
        <Card title="Components">
          <ul class="plain">
            {#each h.components as c (c.name)}
              <li class="row">
                <Badge tone={tone(c.status)}>{c.status}</Badge>
                <strong>{c.name}</strong>
                <span class="muted small">{c.detail}</span>
              </li>
            {/each}
          </ul>
        </Card>

        <Card
          title="Freshness tiers"
          subtitle="Share of files whose current content is covered by each tier"
        >
          {#if h.freshness === null}
            <p class="muted">
              <NotReported /> - the engine does not report per-tier freshness yet, so coverage is not
              shown rather than guessed.
            </p>
          {:else}
            <div class="stack-sm">
              {#each h.freshness as f (f.tier)}
                <div class="frow">
                  <span class="small">{TIER_LABEL[f.tier]}</span>
                  <Meter
                    value={f.coverage}
                    label="{TIER_LABEL[f.tier]} coverage"
                    tone={f.coverage > 0.98 ? 'ok' : f.coverage > 0.9 ? 'accent' : 'warn'}
                  />
                  <span class="muted small num">lag {formatDuration(f.medianLagMs)}</span>
                </div>
              {/each}
            </div>
          {/if}
          <p class="faint small tiernote">
            While a new generation builds, the last ready view keeps serving. See <a
              href={href('/indexes')}>Indexes</a
            >.
          </p>
        </Card>
      </div>

      <div class="grid cols-2">
        <Card title="Resource use">
          {#if h.resources === null}
            <p class="muted">
              <NotReported /> - the engine does not report CPU, memory or disk use yet.
            </p>
          {:else}
            {@const r = h.resources}
            <div class="stack-sm">
              <div class="frow">
                <span class="small">CPU</span>
                <Meter value={r.cpuPercent / 100} label="CPU use" />
                <span></span>
              </div>
              <div class="frow">
                <span class="small">Memory</span>
                <Meter value={r.memoryBytes / r.memoryLimitBytes} label="Memory use" />
                <span class="muted small num"
                  >{formatBytes(r.memoryBytes)} / {formatBytes(r.memoryLimitBytes)}</span
                >
              </div>
              <div class="frow">
                <span class="small">Index disk</span>
                <Meter value={r.diskIndexBytesActual / r.diskLimitBytes} label="Index disk use" />
                <span class="muted small num"
                  >{formatBytes(r.diskIndexBytesActual)} / {formatBytes(r.diskLimitBytes)}</span
                >
              </div>
              <p class="muted small">
                Estimated index size {formatBytes(r.diskIndexBytesEstimated)} vs actual
                {formatBytes(r.diskIndexBytesActual)} ({formatPercent(
                  r.diskIndexBytesActual / r.diskIndexBytesEstimated - 1
                )} difference).
                {r.openConnections} open connections.
              </p>
            </div>
          {/if}
        </Card>

        <Card title="Recent errors" flush>
          {#if h.recentErrors === null}
            <p class="muted body-pad">
              <NotReported /> - the engine does not report recent errors yet.
            </p>
          {:else if h.recentErrors.length === 0}
            <p class="muted body-pad">No errors in the retention window.</p>
          {:else}
            <div class="table-wrap">
              <table class="table">
                <thead><tr><th>When</th><th>Code</th><th>Message</th></tr></thead>
                <tbody>
                  {#each h.recentErrors as er (er.at + er.code)}
                    <tr>
                      <td class="nowrap">{relativeTime(er.at)}</td>
                      <td><code>{er.code}</code></td>
                      <td>{er.message}</td>
                    </tr>
                  {/each}
                </tbody>
              </table>
            </div>
          {/if}
        </Card>
      </div>
    </div>
  {/snippet}
</DataState>

<style>
  ul.plain {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--sp-2);
  }
  .frow {
    display: grid;
    grid-template-columns: 11rem minmax(8rem, 1fr) 9rem;
    gap: var(--sp-3);
    align-items: center;
  }
  .body-pad {
    padding: var(--sp-4);
  }
  .tiernote {
    margin-top: var(--sp-3);
  }
  @media (max-width: 700px) {
    .frow {
      grid-template-columns: 1fr;
      gap: var(--sp-1);
    }
  }
</style>
