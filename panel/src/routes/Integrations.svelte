<script lang="ts">
  import { getApi } from '$lib/api';
  import { useResource } from '$lib/resource.svelte';
  import { relativeTime } from '$lib/format';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import DataState from '$lib/components/DataState.svelte';
  import Card from '$lib/components/Card.svelte';
  import Badge from '$lib/components/Badge.svelte';

  const api = getApi();
  const res = useResource(() => api.getIntegrations());
  const present = (s: string) =>
    s === 'present'
      ? 'ok'
      : s === 'outdated'
        ? 'warn'
        : s === 'not-applicable'
          ? 'neutral'
          : 'danger';
</script>

<PageHeader
  title="Integrations"
  lead="MCP connection, the result of `know connect` for each agent, and connection diagnostics. Connecting MCP alone does not guarantee that memory is read; the instruction file, hook and server instructions work together."
/>

<DataState resource={res}>
  {#snippet children(i)}
    <div class="stack">
      <Card title="MCP server">
        <div class="row">
          <Badge
            tone={i.mcp.status === 'connected'
              ? 'ok'
              : i.mcp.status === 'idle'
                ? 'neutral'
                : 'danger'}>{i.mcp.status}</Badge
          >
          <span>transport <code>{i.mcp.transport}</code></span>
          <span>endpoint <code>{i.mcp.endpoint}</code></span>
          <span class="muted small">last call {relativeTime(i.mcp.lastCallAt)}</span>
        </div>
      </Card>

      <div class="grid cols-3">
        {#each i.agents as a (a.agent)}
          <Card
            title={a.agent}
            subtitle={a.configured ? 'configured by know connect' : 'not connected yet'}
          >
            <dl class="kv">
              <dt>MCP config</dt>
              <dd><Badge tone={present(a.mcpConfig)}>{a.mcpConfig}</Badge></dd>
              <dt>Instruction file</dt>
              <dd><Badge tone={present(a.instructionFile)}>{a.instructionFile}</Badge></dd>
              <dt>Start-up hook</dt>
              <dd><Badge tone={present(a.sessionStartHook)}>{a.sessionStartHook}</Badge></dd>
            </dl>
            <h3>Diagnostics</h3>
            <ul class="diag">
              {#each a.diagnostics as d (d.check)}
                <li>
                  <Badge tone={d.ok ? 'ok' : 'danger'}>{d.ok ? 'pass' : 'fail'}</Badge>
                  {d.check}
                  <div class="faint small">{d.detail}</div>
                </li>
              {/each}
            </ul>
            {#if !a.configured || a.mcpConfig !== 'present'}
              <p class="small">
                Run <code>know connect {a.agent}</code> in the repository to fix this.
              </p>
            {/if}
          </Card>
        {/each}
      </div>

      <Card
        title="Webhooks"
        subtitle="Push signals from git hosts. Polling still runs as a fallback."
        flush
      >
        <div class="table-wrap">
          <table class="table">
            <thead><tr><th>Provider</th><th>Status</th><th>Last delivery</th></tr></thead>
            <tbody>
              {#each i.webhooks as w (w.provider)}
                <tr
                  ><td>{w.provider}</td><td
                    ><Badge tone={w.enabled ? 'ok' : 'neutral'}
                      >{w.enabled ? 'enabled' : 'disabled'}</Badge
                    ></td
                  ><td>{w.enabled ? relativeTime(w.lastDeliveryAt) : '-'}</td></tr
                >
              {/each}
            </tbody>
          </table>
        </div>
      </Card>
    </div>
  {/snippet}
</DataState>

<style>
  .kv {
    display: grid;
    grid-template-columns: 8rem 1fr;
    gap: var(--sp-1) var(--sp-3);
    margin: 0 0 var(--sp-3);
  }
  dt {
    color: var(--text-muted);
  }
  dd {
    margin: 0;
  }
  h3 {
    margin-bottom: var(--sp-2);
  }
  .diag {
    list-style: none;
    padding: 0;
    margin: 0 0 var(--sp-3);
    display: flex;
    flex-direction: column;
    gap: var(--sp-2);
  }
</style>
