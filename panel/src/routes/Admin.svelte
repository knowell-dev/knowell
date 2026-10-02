<script lang="ts">
  import { getApi } from '$lib/api';
  import { useResource } from '$lib/resource.svelte';
  import { relativeTime } from '$lib/format';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import DataState from '$lib/components/DataState.svelte';
  import Card from '$lib/components/Card.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import EmptyState from '$lib/components/EmptyState.svelte';

  const api = getApi();
  const res = useResource(() => api.getAdmin());
</script>

<PageHeader
  title="Administration"
  lead="Users, roles, API tokens and the audit log. Available only when the engine runs in the hub role."
/>

<DataState resource={res}>
  {#snippet children(a)}
    {#if !a.available}
      <EmptyState
        title="Available in the hub role only"
        why="This engine runs as '{a.role}'. Users, roles, tokens and the audit log exist only on a hub (team or company server). A standalone engine has a single local user and no remote access."
      >
        <p class="small muted">
          Start a hub with <code>know serve --role hub</code> and sign in there.
        </p>
      </EmptyState>
    {:else}
      <div class="stack">
        <Card title="Users" flush>
          <div class="table-wrap">
            <table class="table">
              <thead
                ><tr
                  ><th>Name</th><th>E-mail</th><th>Role</th><th>Last sign-in</th><th>Status</th></tr
                ></thead
              >
              <tbody>
                {#each a.users as u (u.id)}
                  <tr
                    ><td>{u.name}</td><td>{u.email}</td><td><Badge>{u.role}</Badge></td><td
                      >{relativeTime(u.lastLoginAt)}</td
                    ><td
                      ><Badge tone={u.disabled ? 'danger' : 'ok'}
                        >{u.disabled ? 'disabled' : 'active'}</Badge
                      ></td
                    ></tr
                  >
                {/each}
              </tbody>
            </table>
          </div>
        </Card>
        <Card
          title="API tokens"
          subtitle="Only a non-secret prefix is shown. A token value is shown once, when created, and never again."
          flush
        >
          {#if a.tokens.length === 0}
            <p class="muted pad">No tokens have been issued.</p>
          {:else}
            <div class="table-wrap">
              <table class="table">
                <thead
                  ><tr
                    ><th>Name</th><th>Prefix</th><th>Scopes</th><th>Created</th><th>Expires</th></tr
                  ></thead
                >
                <tbody>
                  {#each a.tokens as t (t.id)}
                    <tr
                      ><td>{t.name}</td><td class="mono">{t.prefix}...</td><td
                        >{t.scopes.join(', ')}</td
                      ><td>{relativeTime(t.createdAt)}</td><td
                        >{t.expiresAt ? relativeTime(t.expiresAt) : 'never'}</td
                      ></tr
                    >
                  {/each}
                </tbody>
              </table>
            </div>
          {/if}
        </Card>
        <Card title="Audit log" flush>
          {#if a.audit.length === 0}
            <p class="muted pad">No audited actions yet.</p>
          {:else}
            <div class="table-wrap">
              <table class="table">
                <thead
                  ><tr><th>When</th><th>Actor</th><th>Action</th><th>Target</th><th>Outcome</th></tr
                  ></thead
                >
                <tbody>
                  {#each a.audit as e (e.id)}
                    <tr
                      ><td>{relativeTime(e.at)}</td><td>{e.actor}</td><td
                        ><code>{e.action}</code></td
                      ><td class="mono">{e.target}</td><td
                        ><Badge tone={e.outcome === 'success' ? 'ok' : 'danger'}>{e.outcome}</Badge
                        ></td
                      ></tr
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
  .pad {
    padding: var(--sp-4);
  }
</style>
