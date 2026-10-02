<script lang="ts">
  import { getApi, type EvalReport, type EvalSlice } from '$lib/api';
  import { useResource } from '$lib/resource.svelte';
  import { relativeTime } from '$lib/format';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import DataState from '$lib/components/DataState.svelte';
  import Card from '$lib/components/Card.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import Meter from '$lib/components/Meter.svelte';

  const api = getApi();
  const res = useResource(() => api.listEvalReports());
  let picked = $state<string | undefined>();

  const DIMS: { id: EvalSlice['dimension']; title: string }[] = [
    { id: 'retriever', title: 'By retriever' },
    { id: 'kind', title: 'By query kind' },
    { id: 'lang', title: 'By language' }
  ];
  const cur = (all: EvalReport[]) => all.find((r) => r.id === picked) ?? all[0];
</script>

<PageHeader
  title="Quality"
  lead="Evaluation reports: retrieval metrics per retriever, query kind and language. Every number is published with its hardware, dataset and query set."
/>

<DataState
  resource={res}
  isEmpty={(d) => d.length === 0}
  emptyTitle="No evaluation has run"
  emptyWhy="Quality is only shown for measured profiles. Run `know eval` against a query set to produce a report; until then nothing is claimed."
>
  {#snippet children(all)}
    {@const r = cur(all)}
    {#if r}
      <div class="row bar">
        <label class="field inline"
          >Report
          <select bind:value={picked}>
            {#each all as x (x.id)}<option value={x.id}
                >{x.querySet} - {x.profileId} ({relativeTime(x.ranAt)})</option
              >{/each}
          </select>
        </label>
      </div>
      <Card
        title="{r.querySet} on {r.profileId}"
        subtitle="{r.queryCount} queries - {r.dataset} - {r.hardware} - ran {relativeTime(r.ranAt)}"
      >
        <div class="grid cols-4">
          <div>
            <div class="faint small">recall@5</div>
            <strong class="big">{r.overall.recallAt5.toFixed(2)}</strong>
          </div>
          <div>
            <div class="faint small">recall@10</div>
            <strong class="big">{r.overall.recallAt10.toFixed(2)}</strong>
          </div>
          <div>
            <div class="faint small">MRR</div>
            <strong class="big">{r.overall.mrr.toFixed(2)}</strong>
          </div>
          <div>
            <div class="faint small">nDCG@10</div>
            <strong class="big">{r.overall.ndcgAt10.toFixed(2)}</strong>
          </div>
        </div>
      </Card>

      <div class="stack slices">
        {#each DIMS as dim (dim.id)}
          {@const rows = r.slices.filter((s) => s.dimension === dim.id)}
          <Card title={dim.title} flush>
            {#if rows.length === 0}
              <p class="muted pad">This report has no slice by {dim.id}.</p>
            {:else}
              <div class="table-wrap">
                <table class="table">
                  <thead
                    ><tr
                      ><th>{dim.id}</th><th class="num">Queries</th><th class="num">recall@5</th><th
                        >recall@10</th
                      ><th class="num">MRR</th><th class="num">nDCG@10</th></tr
                    ></thead
                  >
                  <tbody>
                    {#each rows as s (s.value)}
                      <tr>
                        <td>{s.value}</td>
                        <td class="num">{s.queries}</td>
                        <td class="num">{s.metrics.recallAt5.toFixed(2)}</td>
                        <td
                          ><Meter
                            value={s.metrics.recallAt10}
                            label="{s.value} recall at 10"
                            tone={s.metrics.recallAt10 < 0.7 ? 'warn' : 'accent'}
                          /></td
                        >
                        <td class="num">{s.metrics.mrr.toFixed(2)}</td>
                        <td class="num">{s.metrics.ndcgAt10.toFixed(2)}</td>
                      </tr>
                    {/each}
                  </tbody>
                </table>
              </div>
            {/if}
          </Card>
        {/each}

        <Card
          title="Bad results"
          subtitle="Queries where the expected file did not come first"
          flush
        >
          {#if r.badResults.length === 0}
            <p class="muted pad">No bad results were recorded for this report.</p>
          {:else}
            <div class="table-wrap">
              <table class="table">
                <thead><tr><th>Query</th><th>Kind</th><th>Expected</th><th>Got</th></tr></thead>
                <tbody>
                  {#each r.badResults as b (b.query)}
                    <tr
                      ><td>{b.query}</td><td><Badge>{b.queryClass}</Badge></td><td class="mono"
                        >{b.expected}</td
                      ><td class="mono">{b.got}</td></tr
                    >
                  {/each}
                </tbody>
              </table>
            </div>
          {/if}
        </Card>
      </div>
    {/if}
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
  .big {
    font-size: 1.5rem;
    font-variant-numeric: tabular-nums;
  }
  .slices {
    margin-top: var(--sp-4);
  }
  .pad {
    padding: var(--sp-4);
  }
</style>
