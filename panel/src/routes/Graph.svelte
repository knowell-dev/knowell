<script lang="ts">
  import {
    getApi,
    type EvidenceType,
    type GraphEdge,
    type GraphNode,
    type ResolutionStatus
  } from '$lib/api';
  import { edgePath, layoutGraph, NODE_H, NODE_W } from '$lib/graphLayout';
  import { useResource } from '$lib/resource.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import DataState from '$lib/components/DataState.svelte';
  import Card from '$lib/components/Card.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import Tabs from '$lib/components/Tabs.svelte';

  const api = getApi();

  let mode = $state<'hierarchy' | 'contracts'>('hierarchy');
  let trail = $state<{ id: string; label: string }[]>([]);
  let selectedNode = $state<string | undefined>();
  let selectedEdge = $state<string | undefined>();
  const parentId = $derived(trail.at(-1)?.id);

  const slice = useResource(() => api.getGraph({ mode, parentId }));
  const insights = useResource(() => api.getGraphInsights());
  $effect(() => {
    void mode;
    void parentId;
    selectedNode = undefined;
    selectedEdge = undefined;
    void slice.load();
  });

  const EVIDENCE: Record<EvidenceType, { label: string; color: string }> = {
    'semantically-resolved': { label: 'Semantically resolved', color: 'var(--ok)' },
    'contract-derived': { label: 'Contract-derived', color: 'var(--accent)' },
    syntactic: { label: 'Syntactic observation', color: 'var(--info)' },
    heuristic: { label: 'Heuristic match', color: 'var(--warn)' },
    'model-suggestion': { label: 'Model suggestion', color: 'var(--danger)' },
    'runtime-observation': { label: 'Runtime observation', color: 'var(--text-muted)' }
  };
  const DASH: Record<ResolutionStatus, string> = {
    resolved: '',
    ambiguous: '7 4',
    unresolved: '2 5'
  };
  const KIND_FILL: Record<string, string> = {
    service: 'var(--accent-bg)',
    endpoint: 'var(--info-bg)',
    event: 'var(--warn-bg)',
    table: 'var(--ok-bg)'
  };

  function setMode(m: string) {
    mode = m as 'hierarchy' | 'contracts';
    trail = [];
  }
  function drill(n: GraphNode) {
    if (mode === 'hierarchy' && (n.kind === 'service' || n.kind === 'module')) {
      trail = [...trail, { id: n.id, label: n.label }];
    }
  }
  function nodeKey(e: KeyboardEvent, n: GraphNode) {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      selectedNode = n.id;
      selectedEdge = undefined;
    }
  }

  const trim = (s: string) => (s.length > 24 ? s.slice(0, 23) + '…' : s);
</script>

<PageHeader
  title="Code graph"
  lead="Drill from services into modules and symbols, or view the contract map. Edge colour is the evidence type; line style is the resolution status. They are never merged into one score."
/>

<Tabs
  label="Graph mode"
  tabs={[
    { id: 'hierarchy', label: 'Service, module, symbol' },
    { id: 'contracts', label: 'Contract map' }
  ]}
  bind:active={() => mode, setMode}
/>

<div class="layout">
  <div class="stack main">
    {#if mode === 'hierarchy'}
      <nav aria-label="Graph path" class="row crumbs">
        <button
          class="btn sm ghost"
          type="button"
          onclick={() => (trail = [])}
          aria-current={trail.length === 0 ? 'true' : undefined}>All services</button
        >
        {#each trail as t, i (t.id)}
          <span aria-hidden="true">/</span>
          <button class="btn sm ghost" type="button" onclick={() => (trail = trail.slice(0, i + 1))}
            >{t.label}</button
          >
        {/each}
      </nav>
    {/if}

    <DataState
      resource={slice}
      isEmpty={(d) => d.nodes.length === 0}
      emptyTitle="Nothing to draw"
      emptyWhy={parentId
        ? 'This node has no children in the analysed view. It may be a leaf, or its language is only chunked as text (no symbols extracted).'
        : 'The graph is empty: no project has been indexed past the symbol tier (T1), so there are no nodes yet.'}
    >
      {#snippet children(g)}
        {@const lay = layoutGraph(g.nodes, g.edges)}
        <Card flush>
          <div class="canvas">
            <!-- Shrinks to the card instead of scrolling, but never below a legible size. -->
            <svg
              viewBox="0 0 {lay.width} {lay.height}"
              style:max-width="{lay.width}px"
              style:min-width="{Math.min(lay.width, 640)}px"
              role="group"
              aria-label="Graph canvas. Use the table below for an accessible list of edges."
            >
              <defs>
                {#each Object.entries(EVIDENCE) as [k, v] (k)}
                  <marker
                    id="arrow-{k}"
                    viewBox="0 0 10 10"
                    refX="9"
                    refY="5"
                    markerWidth="7"
                    markerHeight="7"
                    orient="auto-start-reverse"
                  >
                    <path d="M0 0L10 5L0 10z" fill={v.color} />
                  </marker>
                {/each}
              </defs>
              {#each g.edges as e (e.id)}
                {@const a = lay.placed.get(e.from)}
                {@const b = lay.placed.get(e.to)}
                {#if a && b}
                  <path
                    class="edge"
                    class:sel={selectedEdge === e.id}
                    d={edgePath(a.x, a.y, b.x, b.y)}
                    fill="none"
                    stroke={EVIDENCE[e.evidence].color}
                    stroke-width={selectedEdge === e.id ? 3 : 1.6}
                    stroke-dasharray={DASH[e.status]}
                    marker-end="url(#arrow-{e.evidence})"
                    pointer-events="none"
                  />
                {/if}
              {/each}
              {#each [...lay.placed.values()] as p (p.node.id)}
                <g
                  transform="translate({p.x} {p.y})"
                  role="button"
                  tabindex="0"
                  aria-label="{p.node.kind} {p.node.label}"
                  aria-pressed={selectedNode === p.node.id}
                  class="node"
                  class:sel={selectedNode === p.node.id}
                  onclick={() => {
                    selectedNode = p.node.id;
                    selectedEdge = undefined;
                  }}
                  ondblclick={() => drill(p.node)}
                  onkeydown={(e) => nodeKey(e, p.node)}
                >
                  <title>{p.node.kind}: {p.node.label}</title>
                  <!-- Opaque base: the kind tints are translucent and edges must not show through. -->
                  <rect class="base" width={NODE_W} height={NODE_H} rx="8" />
                  <rect
                    width={NODE_W}
                    height={NODE_H}
                    rx="8"
                    fill={KIND_FILL[p.node.kind] ?? 'var(--surface-2)'}
                  />
                  <text x="8" y="14" class="k">{p.node.kind}</text>
                  <text x="8" y="28" class="l">{trim(p.node.label)}</text>
                </g>
              {/each}
            </svg>
          </div>
        </Card>

        <Card title="Edges" subtitle="Accessible list of what the canvas shows" flush>
          {#if g.edges.length === 0}
            <p class="muted pad">No edges between these nodes in the analysed view.</p>
          {:else}
            <div class="table-wrap">
              <table class="table">
                <thead
                  ><tr
                    ><th>From</th><th>Relation</th><th>To</th><th>Evidence type</th><th>Status</th
                    ><th></th></tr
                  ></thead
                >
                <tbody>
                  {#each g.edges as e (e.id)}
                    <tr class:sel={selectedEdge === e.id}>
                      <td>{g.nodes.find((n) => n.id === e.from)?.label ?? e.from}</td>
                      <td><code>{e.kind}</code></td>
                      <td>{g.nodes.find((n) => n.id === e.to)?.label ?? e.to}</td>
                      <td
                        ><span class="dot" style:background={EVIDENCE[e.evidence].color}></span>
                        {EVIDENCE[e.evidence].label}</td
                      >
                      <td
                        ><Badge
                          tone={e.status === 'resolved'
                            ? 'ok'
                            : e.status === 'ambiguous'
                              ? 'warn'
                              : 'danger'}>{e.status}</Badge
                        ></td
                      >
                      <td
                        ><button
                          class="btn sm ghost"
                          type="button"
                          aria-pressed={selectedEdge === e.id}
                          onclick={() => {
                            selectedEdge = e.id;
                            selectedNode = undefined;
                          }}>Details</button
                        ></td
                      >
                    </tr>
                  {/each}
                </tbody>
              </table>
            </div>
          {/if}
        </Card>

        {@const node = g.nodes.find((n) => n.id === selectedNode)}
        {@const edge = g.edges.find((x: GraphEdge) => x.id === selectedEdge)}
        {#if node}
          <Card title={node.label} subtitle={node.kind}>
            {#if node.detail}<p class="muted">{node.detail}</p>{/if}
            {#if mode === 'hierarchy' && (node.kind === 'service' || node.kind === 'module')}
              <button class="btn" type="button" onclick={() => drill(node)}
                >Drill into {node.label}</button
              >
            {/if}
          </Card>
        {:else if edge}
          <Card title={edge.kind} subtitle="{EVIDENCE[edge.evidence].label} - {edge.status}">
            {#if edge.site}
              <p class="mono">
                {edge.site.path}:{edge.site.lineStart}-{edge.site.lineEnd} @ {edge.site.commit.slice(
                  0,
                  7
                )}
              </p>
            {:else}
              <p class="muted">
                No source site: this relation is derived from a contract or a manifest.
              </p>
            {/if}
          </Card>
        {/if}
      {/snippet}
    </DataState>
  </div>

  <aside class="stack side" aria-label="Legend and insights">
    <Card title="Legend">
      <h3>Evidence type (colour)</h3>
      <ul class="legend">
        {#each Object.entries(EVIDENCE) as [k, v] (k)}
          <li><span class="swatch" style:background={v.color}></span>{v.label}</li>
        {/each}
      </ul>
      <h3>Resolution status (line)</h3>
      <ul class="legend">
        <li>
          <svg width="30" height="8" aria-hidden="true"
            ><line x1="0" y1="4" x2="30" y2="4" stroke="currentColor" stroke-width="2" /></svg
          >resolved
        </li>
        <li>
          <svg width="30" height="8" aria-hidden="true"
            ><line
              x1="0"
              y1="4"
              x2="30"
              y2="4"
              stroke="currentColor"
              stroke-width="2"
              stroke-dasharray="7 4"
            /></svg
          >ambiguous
        </li>
        <li>
          <svg width="30" height="8" aria-hidden="true"
            ><line
              x1="0"
              y1="4"
              x2="30"
              y2="4"
              stroke="currentColor"
              stroke-width="2"
              stroke-dasharray="2 5"
            /></svg
          >unresolved
        </li>
      </ul>
      <p class="faint small">Double-click a service or module to drill down.</p>
      <p class="faint small">
        This view uses a lightweight SVG layout for small graphs. A sigma.js (WebGL) renderer will
        replace it for large graphs.
      </p>
    </Card>
    <Card title="Graph insights" flush>
      <DataState
        resource={insights}
        isEmpty={(d) => d.length === 0}
        emptyTitle="No findings"
        emptyWhy="The rules found no gaps, or no contracts have been extracted yet."
      >
        {#snippet children(list)}
          <ul class="insights">
            {#each list as i (i.id)}
              <li>
                <div>{i.title}</div>
                <div class="row">
                  <Badge tone="info">{i.kind}</Badge><Badge>{EVIDENCE[i.evidence].label}</Badge
                  ><Badge tone={i.status === 'resolved' ? 'ok' : 'warn'}>{i.status}</Badge>
                </div>
              </li>
            {/each}
          </ul>
        {/snippet}
      </DataState>
    </Card>
  </aside>
</div>

<style>
  .layout {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 19rem;
    gap: var(--sp-4);
    margin-top: var(--sp-4);
    align-items: start;
  }
  @media (max-width: 1100px) {
    .layout {
      grid-template-columns: minmax(0, 1fr);
    }
  }
  .main {
    min-width: 0;
  }
  .canvas {
    overflow: auto;
    background: var(--bg-sunken);
    border-radius: var(--radius-lg);
  }
  .canvas svg {
    display: block;
    width: 100%;
    height: auto;
  }
  .edge {
    opacity: 0.8;
  }
  .edge.sel {
    opacity: 1;
  }
  .node rect.base {
    fill: var(--surface);
  }
  .node {
    cursor: pointer;
    outline: none;
  }
  .node rect {
    stroke: var(--border-strong);
    stroke-width: 1;
  }
  .node:focus-visible rect,
  .node.sel rect {
    stroke: var(--accent);
    stroke-width: 2.5;
  }
  .node text {
    fill: var(--text);
    font-family: var(--font-sans);
  }
  .node .k {
    font-size: 9px;
    fill: var(--text-muted);
    text-transform: uppercase;
  }
  .node .l {
    font-size: 12px;
    font-weight: 600;
  }
  .crumbs {
    gap: var(--sp-1);
  }
  .dot,
  .swatch {
    display: inline-block;
    width: 0.65rem;
    height: 0.65rem;
    border-radius: 50%;
    margin-right: var(--sp-2);
  }
  .legend {
    list-style: none;
    padding: 0;
    margin: var(--sp-2) 0 var(--sp-3);
    display: flex;
    flex-direction: column;
    gap: var(--sp-1);
    font-size: var(--fs-sm);
  }
  .legend svg {
    margin-right: var(--sp-2);
    vertical-align: middle;
  }
  .insights {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .insights li {
    padding: var(--sp-3) var(--sp-4);
    border-bottom: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    gap: var(--sp-2);
    font-size: var(--fs-sm);
  }
  .pad {
    padding: var(--sp-4);
  }
</style>
