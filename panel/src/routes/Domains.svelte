<script lang="ts">
  import { getApi, type ApprovalStatus } from '$lib/api';
  import { useResource } from '$lib/resource.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import DataState from '$lib/components/DataState.svelte';
  import Card from '$lib/components/Card.svelte';
  import Badge from '$lib/components/Badge.svelte';

  const api = getApi();
  const domains = useResource(() => api.listDomains());
  const glossary = useResource(() => api.listGlossary());
  const tone = (s: ApprovalStatus) =>
    s === 'approved' ? 'ok' : s === 'rejected' ? 'danger' : 'warn';
</script>

<PageHeader
  title="Domains and glossary"
  lead="Business concepts and the words people use for them. Search uses approved synonyms to map a question to code names; automatic suggestions are kept apart until a human approves them."
/>

<div class="stack">
  <Card title="Domains" flush>
    <DataState
      resource={domains}
      isEmpty={(d) => d.length === 0}
      emptyTitle="No domains yet"
      emptyWhy="Domains are proposed after the first index completes and the engine has enough symbols to cluster. Nothing is invented before that."
    >
      {#snippet children(list)}
        <div class="table-wrap">
          <table class="table">
            <thead
              ><tr
                ><th>Domain</th><th>Description</th><th class="num">Projects</th><th class="num"
                  >Symbols</th
                ></tr
              ></thead
            >
            <tbody>
              {#each list as d (d.id)}
                <tr
                  ><td><strong>{d.name}</strong></td><td class="muted">{d.description}</td><td
                    class="num">{d.projectIds.length}</td
                  ><td class="num">{d.symbolCount}</td></tr
                >
              {/each}
            </tbody>
          </table>
        </div>
      {/snippet}
    </DataState>
  </Card>

  <Card title="Glossary" subtitle="Query-language words mapped to code names" flush>
    <DataState
      resource={glossary}
      isEmpty={(d) => d.length === 0}
      emptyTitle="The glossary is empty"
      emptyWhy="No term has been added or approved. Without synonyms, non-English or business-language queries rely on semantic search alone."
    >
      {#snippet children(list)}
        <div class="table-wrap">
          <table class="table">
            <thead
              ><tr><th>Term</th><th>Definition</th><th>Code names</th><th>Synonyms</th></tr></thead
            >
            <tbody>
              {#each list as t (t.id)}
                <tr>
                  <td><strong>{t.term}</strong></td>
                  <td>{t.definition}</td>
                  <td
                    >{#each t.codeNames as c (c)}<code class="chip">{c}</code>{/each}</td
                  >
                  <td>
                    {#if t.synonyms.length === 0}<span class="faint">none</span>{/if}
                    {#each t.synonyms as s (s.text)}
                      <span class="syn"
                        ><Badge
                          tone={tone(s.status)}
                          title={s.origin === 'auto'
                            ? 'Suggested automatically'
                            : 'Entered by a person'}
                          >{s.text}: {s.status}{s.origin === 'auto' ? ' (auto)' : ''}</Badge
                        ></span
                      >
                    {/each}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/snippet}
    </DataState>
  </Card>
</div>

<style>
  .chip {
    display: inline-block;
    margin: 0 var(--sp-1) var(--sp-1) 0;
  }
  .syn {
    display: inline-block;
    margin: 0 var(--sp-1) var(--sp-1) 0;
  }
</style>
