<script lang="ts">
  import { getApi } from '$lib/api';
  import { useResource } from '$lib/resource.svelte';
  import { relativeTime } from '$lib/format';
  import { href } from '$lib/router.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import DataState from '$lib/components/DataState.svelte';
  import Card from '$lib/components/Card.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import NotReported from '$lib/components/NotReported.svelte';

  const api = getApi();
  const list = useResource(() => api.listWorkspaces());
  let selected = $state<string | undefined>(undefined);
  const detail = useResource(async () => {
    const id = selected ?? (await api.listWorkspaces())[0]?.id;
    return id ? api.getWorkspace(id) : undefined;
  });
  function pick(id: string) {
    selected = id;
    void detail.load();
  }
  const POLICY = {
    'local-only': 'Nothing leaves this machine',
    cloud: 'Redacted text may go to configured cloud providers'
  } as const;
</script>

<PageHeader
  title="Workspaces"
  lead="A workspace groups projects and holds the shared settings they inherit: tracked ref, embedding profile and data policy."
/>

<DataState
  resource={list}
  isEmpty={(d) => d.length === 0}
  emptyTitle="No workspaces yet"
  emptyWhy="A workspace is created with `know init` or `know workspace add`. Until one exists there is nothing to index or search."
>
  {#snippet children(ws)}
    <div class="grid cols-2 split">
      <Card title="All workspaces" flush>
        <div class="table-wrap">
          <table class="table">
            <thead
              ><tr
                ><th>Name</th><th class="num">Projects</th><th>Tracked ref</th><th>Profile</th></tr
              ></thead
            >
            <tbody>
              {#each ws as w (w.id)}
                <tr class:sel={(selected ?? ws[0]?.id) === w.id}>
                  <td
                    ><button
                      class="linklike"
                      type="button"
                      onclick={() => pick(w.id)}
                      aria-pressed={(selected ?? ws[0]?.id) === w.id}>{w.name}</button
                    ></td
                  >
                  <td class="num">{w.projectCount}</td>
                  <td>
                    {#if w.trackedRef}<code>{w.trackedRef.name || w.trackedRef.text}</code
                      >{:else}<NotReported />{/if}
                  </td>
                  <td>
                    {#if w.embeddingProfileId}<code>{w.embeddingProfileId}</code>{:else}<NotReported
                      />{/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      </Card>

      <DataState resource={detail}>
        {#snippet children(w)}
          {#if w}
            <Card title={w.name} subtitle={w.description ?? undefined}>
              {#if w.settingsError}
                <div class="note" role="alert">
                  <strong>Workspace settings could not be loaded.</strong>
                  {w.settingsError}
                </div>
              {/if}
              <dl class="kv">
                <dt>Tracked ref policy</dt>
                <dd>
                  {#if w.trackedRef}
                    <code>{w.trackedRef.name || w.trackedRef.text}</code>
                    <Badge>{w.trackedRef.kind}</Badge>
                    <span class="faint small">Projects inherit this unless they override it.</span>
                  {:else}
                    <NotReported
                      why="The workspace settings file is unknown or could not be loaded"
                    />
                  {/if}
                </dd>
                <dt>Embedding profile</dt>
                <dd>
                  {#if w.embeddingProfileId}<a href={href('/models')}
                      ><code>{w.embeddingProfileId}</code></a
                    >{:else}<NotReported />{/if}
                </dd>
                <dt>Data policy</dt>
                <dd>
                  {#if w.dataPolicy}
                    <Badge tone={w.dataPolicy === 'local-only' ? 'ok' : 'warn'}
                      >{w.dataPolicy}</Badge
                    >
                    <span class="muted small">{POLICY[w.dataPolicy]}</span>
                  {:else}
                    <NotReported
                      why="The workspace settings file is unknown or could not be loaded"
                    />
                  {/if}
                </dd>
                <dt>Created</dt>
                <dd>{relativeTime(w.createdAt)}</dd>
                <dt>Projects</dt>
                <dd>
                  {#if w.projectIds.length === 0}
                    <span class="muted"
                      >No projects: add one with <code>know project add</code>.</span
                    >
                  {:else}
                    <a href={href('/projects')}>{w.projectIds.length} projects</a>
                  {/if}
                </dd>
                <dt>Access</dt>
                <dd>
                  {#if w.members === null}
                    <NotReported why="Workspace membership is not tracked yet" />
                    {#if w.memberCount !== null}<span class="muted small"
                        >({w.memberCount} members)</span
                      >{/if}
                  {:else}
                    <ul class="plain">
                      {#each w.members as m (m.name)}<li>
                          {m.name}
                          <Badge>{m.role}</Badge>
                        </li>{/each}
                    </ul>
                  {/if}
                </dd>
              </dl>
            </Card>
          {/if}
        {/snippet}
      </DataState>
    </div>
  {/snippet}
</DataState>

<style>
  .kv {
    display: grid;
    grid-template-columns: 9rem 1fr;
    gap: var(--sp-2) var(--sp-3);
    margin: 0;
  }
  dt {
    color: var(--text-muted);
  }
  dd {
    margin: 0;
  }
  ul.plain {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .note {
    border: 1px solid var(--warn);
    background: var(--warn-bg);
    border-radius: var(--radius);
    padding: var(--sp-3);
    margin-bottom: var(--sp-3);
  }
  .linklike {
    background: none;
    border: 0;
    padding: 0;
    color: var(--accent);
    cursor: pointer;
    font-weight: 600;
  }
  .linklike:hover {
    text-decoration: underline;
  }
</style>
