<script lang="ts">
  import { getApi, type ProjectDetail, type ProjectSummary } from '$lib/api';
  import { useResource } from '$lib/resource.svelte';
  import { relativeTime, formatNumber, shortSha } from '$lib/format';
  import { router, navigate, href } from '$lib/router.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import DataState from '$lib/components/DataState.svelte';
  import Card from '$lib/components/Card.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import OriginBadge from '$lib/components/OriginBadge.svelte';
  import EmptyState from '$lib/components/EmptyState.svelte';
  import NotReported from '$lib/components/NotReported.svelte';

  const api = getApi();
  const list = useResource(() => api.listProjects());
  const selectedId = $derived(router.current.query.get('id') ?? undefined);

  const detail = useResource<ProjectDetail | undefined>(async () =>
    selectedId ? api.getProject(selectedId) : undefined
  );
  $effect(() => {
    void selectedId;
    void detail.load();
  });

  function select(p: ProjectSummary) {
    navigate('/projects', { id: p.id });
  }
</script>

<PageHeader
  title="Projects"
  lead="Source, root, tracked ref, worktrees and exclusions, with the layer each setting comes from."
/>

<DataState
  resource={list}
  isEmpty={(d) => d.length === 0}
  emptyTitle="No projects registered"
  emptyWhy="Projects are added to a workspace with `know project add <path-or-url>`. Nothing can be indexed until a project exists."
>
  {#snippet children(projects)}
    <div class="split">
      <Card title="Projects" flush>
        <div class="table-wrap">
          <table class="table">
            <thead
              ><tr
                ><th>Name</th><th>Workspace</th><th>Kind</th><th>Languages</th><th class="num"
                  >Files</th
                ><th>Index</th></tr
              ></thead
            >
            <tbody>
              {#each projects as p (p.id)}
                <tr class:sel={selectedId === p.id}>
                  <td
                    ><a
                      href={href('/projects', { id: p.id })}
                      aria-current={selectedId === p.id ? 'true' : undefined}
                      onclick={(e) => {
                        e.preventDefault();
                        select(p);
                      }}>{p.name}</a
                    ></td
                  >
                  <td class="muted">{p.workspaceName}</td>
                  <td
                    >{#if p.kind !== null}{p.kind}{:else}<NotReported />{/if}</td
                  >
                  <td class="muted">
                    {#if p.languages === null}<NotReported
                      />{:else if p.languages.length === 0}-{:else}{p.languages.join(', ')}{/if}
                  </td>
                  <td class="num"
                    >{#if p.fileCount !== null}{formatNumber(p.fileCount)}{:else}<NotReported
                      />{/if}</td
                  >
                  <td>
                    {#if p.indexed}<Badge tone="ok">{relativeTime(p.lastIndexedAt)}</Badge
                      >{:else}<Badge tone="warn">not indexed</Badge>{/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      </Card>

      <div class="stack">
        {#if !selectedId}
          <EmptyState
            title="Select a project"
            why="Pick a project on the left to see where each of its settings comes from."
          />
        {:else}
          <DataState resource={detail}>
            {#snippet children(p)}
              {#if p}
                <Card
                  title={p.name}
                  subtitle={[`workspace ${p.workspaceName}`, p.kind, p.languages?.join(', ')]
                    .filter((x): x is string => !!x)
                    .join(' - ')}
                >
                  {#if p.settingsError}
                    <div class="note" role="alert">
                      <strong>Workspace settings could not be loaded.</strong>
                      {p.settingsError}
                    </div>
                  {/if}
                  {#if !p.indexed}
                    <div class="note" role="status">
                      <strong>This project has not been indexed yet.</strong>
                      Search and graph results exclude it until the first index finishes. Start one from
                      <a href={href('/indexes')}>Indexes</a>.
                    </div>
                  {/if}
                  <dl class="kv">
                    <dt>Source</dt>
                    <dd>
                      {#if p.source.type === 'git'}git <code>{p.source.remote}</code>{:else}local <code
                          >{p.source.path}</code
                        >{/if}
                    </dd>
                  </dl>
                  {#if p.views.length > 0}
                    <h3 class="vh">Views</h3>
                    <ul class="plain">
                      {#each p.views as v (v.id)}
                        <li>
                          <code>{v.trackTarget.text}</code>
                          <span class="muted small"
                            >active {shortSha(v.activeIndexCommit)} - last seen {shortSha(
                              v.lastSeenCommit
                            )}</span
                          >
                        </li>
                      {/each}
                    </ul>
                  {/if}
                </Card>

                <Card
                  title="Effective settings"
                  subtitle="Each value shows the layer that provides it: builtin default, workspace, or project."
                  flush
                >
                  <div class="table-wrap">
                    <table class="table">
                      <thead><tr><th>Setting</th><th>Value</th><th>Comes from</th></tr></thead>
                      <tbody>
                        <tr>
                          <td>Root</td>
                          <td><code>{p.root.value || '(source root)'}</code></td>
                          <td><OriginBadge origin={p.root.origin} note={p.root.originNote} /></td>
                        </tr>
                        <tr>
                          <td>Tracked ref</td>
                          {#if p.trackedRef}
                            <td
                              ><code>{p.trackedRef.value.name || p.trackedRef.value.text}</code>
                              <Badge>{p.trackedRef.value.kind}</Badge></td
                            >
                            <td
                              ><OriginBadge
                                origin={p.trackedRef.origin}
                                note={p.trackedRef.originNote}
                              /></td
                            >
                          {:else}
                            <td colspan="2"
                              ><NotReported why="The workspace settings file is unknown" /></td
                            >
                          {/if}
                        </tr>
                        {#if p.excludes === null}
                          <tr>
                            <td>Exclusions</td>
                            <td colspan="2"><NotReported /></td>
                          </tr>
                        {:else if p.excludes.length === 0}
                          <tr>
                            <td>Exclusions</td>
                            <td colspan="2" class="muted">No exclude patterns.</td>
                          </tr>
                        {:else}
                          {#each p.excludes as x, i (x.value + i)}
                            <tr>
                              <td>{i === 0 ? 'Exclusions' : ''}</td>
                              <td><code>{x.value}</code></td>
                              <td><OriginBadge origin={x.origin} note={x.originNote} /></td>
                            </tr>
                          {/each}
                        {/if}
                        {#if p.embedding}
                          {@const e = p.embedding}
                          <tr>
                            <td>Embedding provider</td>
                            {#if e.provider}
                              <td><code>{e.provider.value}</code></td>
                              <td
                                ><OriginBadge
                                  origin={e.provider.origin}
                                  note={e.provider.originNote}
                                /></td
                              >
                            {:else}
                              <td colspan="2" class="muted"
                                >None configured: lexical and graph search only.</td
                              >
                            {/if}
                          </tr>
                          <tr>
                            <td>Embedding model</td>
                            {#if e.model}
                              <td><code>{e.model.value}</code></td>
                              <td
                                ><OriginBadge
                                  origin={e.model.origin}
                                  note={e.model.originNote}
                                /></td
                              >
                            {:else}
                              <td colspan="2" class="muted">The provider default model.</td>
                            {/if}
                          </tr>
                          <tr>
                            <td>Embedding preset</td>
                            <td><code>{e.preset.value}</code></td>
                            <td
                              ><OriginBadge
                                origin={e.preset.origin}
                                note={e.preset.originNote}
                              /></td
                            >
                          </tr>
                          <tr>
                            <td>Vector dimensions</td>
                            <td><code>{e.dimensions.value}</code></td>
                            <td
                              ><OriginBadge
                                origin={e.dimensions.origin}
                                note={e.dimensions.originNote}
                              /></td
                            >
                          </tr>
                        {:else}
                          <tr>
                            <td>Embedding</td>
                            <td colspan="2"
                              ><NotReported why="The workspace settings file is unknown" /></td
                            >
                          </tr>
                        {/if}
                        <tr>
                          <td>Embedding profile</td>
                          {#if p.embeddingProfileId}
                            <td
                              ><a href={href('/models')}
                                ><code>{p.embeddingProfileId.value}</code></a
                              ></td
                            >
                            <td
                              ><OriginBadge
                                origin={p.embeddingProfileId.origin}
                                note={p.embeddingProfileId.originNote}
                              /></td
                            >
                          {:else}
                            <td colspan="2"><NotReported /></td>
                          {/if}
                        </tr>
                        <tr>
                          <td>Data policy</td>
                          {#if p.dataPolicy}
                            <td><code>{p.dataPolicy.value}</code></td>
                            <td
                              ><OriginBadge
                                origin={p.dataPolicy.origin}
                                note={p.dataPolicy.originNote}
                              /></td
                            >
                          {:else}
                            <td colspan="2"
                              ><NotReported why="The workspace settings file is unknown" /></td
                            >
                          {/if}
                        </tr>
                        <tr>
                          <td>Analysis</td>
                          {#if p.analysis}
                            <td><code>{p.analysis.value}</code></td>
                            <td
                              ><OriginBadge
                                origin={p.analysis.origin}
                                note={p.analysis.originNote}
                              /></td
                            >
                          {:else}
                            <td colspan="2"><NotReported /></td>
                          {/if}
                        </tr>
                      </tbody>
                    </table>
                  </div>
                </Card>

                <Card
                  title="Sensitive files"
                  subtitle="Excluded by path before any content was read"
                >
                  {#if p.sensitiveExcludedCount === null}
                    <p>
                      <NotReported /> - the count of files skipped by the sensitive-path policy is not
                      tracked yet. Such files (for example <code>.env*</code>, keys, credentials)
                      are excluded by path before their content is read.
                    </p>
                  {:else}
                    <p>
                      <strong>{p.sensitiveExcludedCount}</strong> files matched the sensitive-path
                      policy (for example <code>.env*</code>, keys, credentials). Their content was
                      never read, indexed or embedded, and their paths are not listed here.
                    </p>
                  {/if}
                </Card>

                <Card
                  title="Worktrees"
                  subtitle="Each worktree is a personal layer on top of the tracked view"
                  flush
                >
                  {#if p.worktrees === null}
                    <p class="muted pad">
                      <NotReported /> - worktree discovery is not reported yet.
                    </p>
                  {:else if p.worktrees.length === 0}
                    <p class="muted pad">
                      No worktrees discovered. Worktrees of this repository are found automatically
                      once they exist.
                    </p>
                  {:else}
                    <div class="table-wrap">
                      <table class="table">
                        <thead
                          ><tr
                            ><th>Path</th><th>Branch</th><th>HEAD</th><th class="num"
                              >Changed files</th
                            ><th>Task view</th></tr
                          ></thead
                        >
                        <tbody>
                          {#each p.worktrees as w (w.path)}
                            <tr>
                              <td class="mono">{w.path}</td>
                              <td><code>{w.branch}</code></td>
                              <td class="mono">{w.head}</td>
                              <td class="num">{w.dirtyFiles}</td>
                              <td
                                >{#if w.taskGroup}<Badge tone="accent">{w.taskGroup}</Badge
                                  >{:else}<span class="faint">-</span>{/if}</td
                              >
                            </tr>
                          {/each}
                        </tbody>
                      </table>
                    </div>
                  {/if}
                </Card>
              {/if}
            {/snippet}
          </DataState>
        {/if}
      </div>
    </div>
  {/snippet}
</DataState>

<style>
  .split {
    display: grid;
    grid-template-columns: minmax(20rem, 2fr) minmax(0, 3fr);
    gap: var(--sp-4);
    align-items: start;
  }
  @media (max-width: 1100px) {
    .split {
      grid-template-columns: minmax(0, 1fr);
    }
  }
  .kv {
    display: grid;
    grid-template-columns: 6rem 1fr;
    gap: var(--sp-2);
    margin: 0;
  }
  dt {
    color: var(--text-muted);
  }
  dd {
    margin: 0;
    word-break: break-all;
  }
  .note {
    border: 1px solid var(--warn);
    background: var(--warn-bg);
    border-radius: var(--radius);
    padding: var(--sp-3);
    margin-bottom: var(--sp-3);
  }
  .vh {
    margin: var(--sp-3) 0 var(--sp-1);
  }
  ul.plain {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .pad {
    padding: var(--sp-4);
  }
</style>
