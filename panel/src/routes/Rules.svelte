<script lang="ts">
  import { getApi, type Severity } from '$lib/api';
  import { useResource } from '$lib/resource.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import DataState from '$lib/components/DataState.svelte';
  import Card from '$lib/components/Card.svelte';
  import Badge from '$lib/components/Badge.svelte';

  const api = getApi();
  const rules = useResource(() => api.listRules());
  const sev = (s: Severity) => (s === 'error' ? 'danger' : s === 'warning' ? 'warn' : 'info');
</script>

<PageHeader
  title="Rules"
  lead="Architecture rules, approved examples and current violations. `know check` evaluates these deterministically, without embeddings or an API key."
/>

<DataState
  resource={rules}
  isEmpty={(d) => d.length === 0}
  emptyTitle="No rules defined"
  emptyWhy="No rule pack is enabled and no rule has been accepted. Enable a rule pack or accept a proposed rule from Memory to start checking."
>
  {#snippet children(list)}
    <div class="stack">
      {#each list as r (r.id)}
        <Card title={r.name} subtitle={r.description}>
          {#snippet actions()}
            <Badge tone={sev(r.severity)}>{r.severity}</Badge>
            <Badge
              tone={r.state === 'accepted' ? 'ok' : r.state === 'proposed' ? 'info' : 'neutral'}
              >{r.state}</Badge
            >
            {#if r.pack}<Badge mono>pack: {r.pack}</Badge>{/if}
          {/snippet}
          <div class="grid cols-2">
            <div>
              <h3>Violations ({r.violations.length})</h3>
              {#if r.violations.length === 0}
                <p class="muted small">No violations in the current views.</p>
              {:else}
                <ul>
                  {#each r.violations as v (v.path + v.lineStart)}
                    <li>
                      <span class="mono small">{v.projectName}: {v.path}:{v.lineStart}</span><br
                      />{v.message}
                    </li>
                  {/each}
                </ul>
              {/if}
            </div>
            <div>
              <h3>Approved examples ({r.approvedExamples.length})</h3>
              {#if r.approvedExamples.length === 0}
                <p class="muted small">
                  No approved examples yet. Examples show agents what a correct implementation looks
                  like.
                </p>
              {:else}
                <ul>
                  {#each r.approvedExamples as e (e.path)}
                    <li><span class="mono small">{e.path}</span><br />{e.note}</li>
                  {/each}
                </ul>
              {/if}
            </div>
          </div>
        </Card>
      {/each}
    </div>
  {/snippet}
</DataState>

<style>
  h3 {
    margin-bottom: var(--sp-2);
  }
  ul {
    margin: 0;
    padding-left: 1.1rem;
    display: flex;
    flex-direction: column;
    gap: var(--sp-2);
  }
</style>
