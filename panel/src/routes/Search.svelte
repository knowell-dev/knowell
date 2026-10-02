<script lang="ts">
  import {
    ApiError,
    getApi,
    type MatchReason,
    type ScoreBreakdown,
    type SearchResponse,
    type SearchResult
  } from '$lib/api';
  import { useResource } from '$lib/resource.svelte';
  import { shortSha } from '$lib/format';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import Card from '$lib/components/Card.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import CodeBlock from '$lib/components/CodeBlock.svelte';
  import EmptyState from '$lib/components/EmptyState.svelte';

  const api = getApi();
  const projects = useResource(() => api.listProjects());

  let query = $state('');
  let projectIds = $state<string[]>([]);
  let language = $state('');
  let pathPrefix = $state('');
  let expandGraph = $state(true);
  let rerank = $state(false);
  let limit = $state(8);

  let loading = $state(false);
  let error = $state<ApiError | undefined>();
  let response = $state<SearchResponse | undefined>();
  let open = $state<Record<string, boolean>>({});

  const languages = $derived(
    [...new Set((projects.data ?? []).flatMap((p) => p.languages ?? []))].sort()
  );

  async function run(e: Event) {
    e.preventDefault();
    loading = true;
    error = undefined;
    try {
      response = await api.search({
        query,
        projectIds: projectIds.length ? projectIds : undefined,
        languages: language ? [language] : undefined,
        pathPrefix: pathPrefix || undefined,
        expandGraph,
        rerank,
        limit
      });
    } catch (err) {
      error = err instanceof ApiError ? err : new ApiError('unknown', 'search failed unexpectedly');
      response = undefined;
    } finally {
      loading = false;
    }
  }

  const SIGNALS: { key: keyof Omit<ScoreBreakdown, 'fused'>; label: string }[] = [
    { key: 'bm25', label: 'BM25 (lexical)' },
    { key: 'vector', label: 'Vector (semantic)' },
    { key: 'graph', label: 'Graph expansion' },
    { key: 'rerank', label: 'Rerank' }
  ];

  function why(r: MatchReason): string {
    switch (r.type) {
      case 'exact-symbol':
        return `Exact symbol match: ${r.symbol}`;
      case 'lexical':
        return `Lexical match on: ${r.terms.join(', ')}`;
      case 'semantic':
        return `Semantically similar to the query (cosine ${r.similarity})`;
      case 'test-reference':
        return `A test that references ${r.target}`;
      case 'graph':
        return `Reached by graph path ${r.path.join(' > ')} (${r.edge} evidence)`;
    }
  }

  const tierNote = (r: SearchResult) =>
    r.tier === 'T0' || r.tier === 'T1'
      ? 'Embeddings for this file are not ready yet, so no vector signal contributed.'
      : '';
</script>

<PageHeader
  title="Search playground"
  lead="Try a query against the workspace. Every result shows its score breakdown, the freshness tier it came from and why it was returned."
/>

<div class="layout">
  <form class="stack filters" onsubmit={run} aria-label="Search">
    <label class="field">
      Query
      <input
        type="search"
        bind:value={query}
        placeholder="e.g. place order publishes event"
        autocomplete="off"
      />
    </label>
    <fieldset>
      <legend>Projects</legend>
      {#if projects.data}
        {#each projects.data as p (p.id)}
          <label class="check">
            <input type="checkbox" value={p.id} bind:group={projectIds} />
            {p.name}{#if !p.indexed}
              <span class="faint">(not indexed)</span>{/if}
          </label>
        {/each}
      {:else if projects.loading}
        <span class="muted small">Loading projects...</span>
      {:else}
        <span class="muted small">Projects could not be loaded; searching all.</span>
      {/if}
      <p class="faint small">Leave empty to search every project.</p>
    </fieldset>
    <label class="field">
      Language
      <select bind:value={language}>
        <option value="">Any</option>
        {#each languages as l (l)}<option value={l}>{l}</option>{/each}
      </select>
    </label>
    <label class="field">
      Path prefix
      <input
        type="text"
        bind:value={pathPrefix}
        placeholder="internal/orders/"
        autocomplete="off"
      />
    </label>
    <label class="check"
      ><input type="checkbox" bind:checked={expandGraph} /> Expand along the graph (callers, tests, contracts)</label
    >
    <label class="check"
      ><input type="checkbox" bind:checked={rerank} /> Rerank the short list</label
    >
    <label class="field">
      Result limit
      <input
        type="text"
        inputmode="numeric"
        value={limit}
        onchange={(e) => (limit = Math.max(1, Math.min(50, Number(e.currentTarget.value) || 8)))}
      />
    </label>
    <button class="btn primary" type="submit" disabled={loading}
      >{loading ? 'Searching...' : 'Search'}</button
    >
  </form>

  <div class="stack results" aria-live="polite">
    {#if error}
      <div class="error" role="alert">
        <strong>Search failed.</strong>
        {error.message} <span class="faint mono">{error.code}</span>
      </div>
    {:else if loading}
      <p class="muted" role="status">Searching...</p>
    {:else if !response}
      <EmptyState
        title="No search run yet"
        why="Enter a query and press Search. Results are drawn only from indexed projects; unindexed ones are listed as skipped."
      />
    {:else}
      <div class="row summary">
        <Badge tone="info">{response.queryClass}</Badge>
        <span class="muted small"
          >{response.results.length} results in {response.tookMs} ms - about {response.tokensReturned}
          tokens returned</span
        >
      </div>
      {#each response.skipped as s (s.projectName)}
        <div class="skipped" role="status">
          <strong>{s.projectName}</strong> was skipped: {s.reason}.
        </div>
      {/each}
      {#if response.results.length === 0}
        <EmptyState title="No results" why={response.emptyReason?.message ?? 'Nothing matched.'} />
      {/if}
      {#each response.results as r (r.id)}
        <Card>
          <div class="row head">
            <strong>{r.symbol ?? r.path}</strong>
            <Badge tone="neutral">{r.projectName}</Badge>
            <Badge tone="neutral">{r.language}</Badge>
            <Badge
              tone={r.tier === 'T2' || r.tier === 'T3' ? 'ok' : 'warn'}
              title="Freshness tier this result comes from">{r.tier}</Badge
            >
            <span class="spacer"></span>
            <span class="fused" title="Fused score">score {r.score.fused.toFixed(3)}</span>
          </div>
          <p class="mono faint small">{r.path}:{r.lineStart}-{r.lineEnd} @ {shortSha(r.commit)}</p>
          <CodeBlock
            text={r.snippet}
            startLine={r.lineStart}
            label="Source of {r.symbol ?? r.path}"
          />

          <div class="grid cols-2 detail">
            <div>
              <h3>Score breakdown</h3>
              <table class="table" aria-label="Score breakdown">
                <tbody>
                  {#each SIGNALS as s (s.key)}
                    {@const v = r.score[s.key]}
                    <tr>
                      <td>{s.label}</td>
                      <td class="num">{v === null ? 'did not run' : v.toFixed(3)}</td>
                      <td class="barcell">
                        {#if v !== null}<div
                            class="bar"
                            style:width="{Math.min(100, v * 100)}%"
                          ></div>{/if}
                      </td>
                    </tr>
                  {/each}
                </tbody>
              </table>
              {#if tierNote(r)}<p class="faint small">{tierNote(r)}</p>{/if}
            </div>
            <div>
              <h3>Why this result</h3>
              <ul>
                {#each r.reasons as reason, i (i)}<li>{why(reason)}</li>{/each}
                {#if r.reasons.length === 0}<li class="muted">
                    No single dominant reason; combined signals only.
                  </li>{/if}
              </ul>
              {#if r.preparedText}
                <button
                  class="btn ghost sm"
                  type="button"
                  aria-expanded={open[r.id] ?? false}
                  onclick={() => (open[r.id] = !open[r.id])}
                >
                  {open[r.id] ? 'Hide' : 'Show'} text sent for embedding
                </button>
                {#if open[r.id]}<CodeBlock
                    text={r.preparedText}
                    label="Prepared embedding text"
                  />{/if}
              {/if}
            </div>
          </div>
        </Card>
      {/each}
    {/if}
  </div>
</div>

<style>
  .layout {
    display: grid;
    grid-template-columns: 18rem minmax(0, 1fr);
    gap: var(--sp-5);
    align-items: start;
  }
  @media (max-width: 1000px) {
    .layout {
      grid-template-columns: minmax(0, 1fr);
    }
  }
  .filters {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: var(--sp-4);
  }
  fieldset {
    border: 1px solid var(--border);
    border-radius: var(--radius);
    display: flex;
    flex-direction: column;
    gap: var(--sp-1);
    padding: var(--sp-2) var(--sp-3);
    margin: 0;
  }
  legend {
    font-size: var(--fs-sm);
    color: var(--text-muted);
    padding: 0 var(--sp-1);
  }
  .results {
    min-width: 0;
  }
  .error {
    border: 1px solid var(--danger);
    background: var(--danger-bg);
    padding: var(--sp-3);
    border-radius: var(--radius);
  }
  .skipped {
    border: 1px solid var(--warn);
    background: var(--warn-bg);
    padding: var(--sp-2) var(--sp-3);
    border-radius: var(--radius);
  }
  .head {
    margin-bottom: var(--sp-1);
  }
  .fused {
    font-family: var(--font-mono);
    color: var(--accent);
  }
  .detail {
    margin-top: var(--sp-3);
  }
  h3 {
    margin-bottom: var(--sp-2);
  }
  .barcell {
    width: 35%;
  }
  .bar {
    height: 0.45rem;
    background: var(--accent);
    border-radius: 999px;
  }
  ul {
    margin: 0 0 var(--sp-2);
    padding-left: 1.1rem;
  }
</style>
