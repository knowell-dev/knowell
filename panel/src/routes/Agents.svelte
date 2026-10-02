<script lang="ts">
  import { getApi } from '$lib/api';
  import { useResource } from '$lib/resource.svelte';
  import { formatCompact, formatNumber, formatUsd, relativeTime } from '$lib/format';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import DataState from '$lib/components/DataState.svelte';
  import Card from '$lib/components/Card.svelte';
  import Stat from '$lib/components/Stat.svelte';
  import BarChart from '$lib/components/BarChart.svelte';

  const api = getApi();
  let days = $state(14);
  const res = useResource(() => api.getUsage(days));
  function setDays(d: number) {
    days = d;
    void res.load();
  }
</script>

<PageHeader
  title="Agents and usage"
  lead="MCP tool calls, tokens returned to agents, latency and spend."
>
  {#snippet actions()}
    <div class="row" role="group" aria-label="Period">
      {#each [7, 14, 30] as d (d)}
        <button
          class="btn sm"
          type="button"
          aria-pressed={days === d}
          class:primary={days === d}
          onclick={() => setDays(d)}>{d} days</button
        >
      {/each}
    </div>
  {/snippet}
</PageHeader>

<DataState
  resource={res}
  isEmpty={(d) => d.tools.length === 0}
  emptyTitle="No agent activity yet"
  emptyWhy="No agent has called the MCP server in this period. Connect one with `know connect claude|codex|cursor` (see Integrations)."
>
  {#snippet children(u)}
    {@const calls = u.tools.reduce((s, t) => s + t.calls, 0)}
    {@const tokens = u.tools.reduce((s, t) => s + t.tokensReturned, 0)}
    {@const errors = u.tools.reduce((s, t) => s + t.errors, 0)}
    {@const spend = u.tools.reduce((s, t) => s + t.spendUsdMicros, 0)}
    <div class="stack">
      <div class="grid cols-4">
        <Stat label="MCP calls" value={formatNumber(calls)} hint="last {u.periodDays} days" />
        <Stat
          label="Tokens returned"
          value={formatCompact(tokens)}
          hint="to agents, after budgeting"
        />
        <Stat
          label="Errors"
          value={formatNumber(errors)}
          tone={errors > 0 ? 'warn' : 'ok'}
          hint="{((errors / Math.max(1, calls)) * 100).toFixed(2)}% of calls"
        />
        <Stat label="Spend" value={formatUsd(spend)} hint="provider cost attributed to calls" />
      </div>

      <Card title="Calls per day">
        <BarChart
          values={u.daily.map((d) => d.calls)}
          labels={u.daily.map((d) => d.date)}
          title="MCP calls per day"
        />
        <div class="row axis small faint">
          <span>{u.daily[0]?.date}</span><span class="spacer"></span><span
            >{u.daily.at(-1)?.date}</span
          >
        </div>
      </Card>

      <div class="grid cols-2">
        <Card title="Tools" flush>
          <div class="table-wrap">
            <table class="table">
              <thead
                ><tr
                  ><th>Tool</th><th class="num">Calls</th><th class="num">Errors</th><th class="num"
                    >Tokens</th
                  ><th class="num">p50</th><th class="num">p95</th><th class="num">Spend</th></tr
                ></thead
              >
              <tbody>
                {#each u.tools as t (t.tool)}
                  <tr>
                    <td><code>{t.tool}</code></td><td class="num">{formatNumber(t.calls)}</td><td
                      class="num">{t.errors}</td
                    >
                    <td class="num">{formatCompact(t.tokensReturned)}</td><td class="num"
                      >{t.p50Ms} ms</td
                    ><td class="num">{t.p95Ms} ms</td><td class="num"
                      >{formatUsd(t.spendUsdMicros)}</td
                    >
                  </tr>
                {/each}
              </tbody>
            </table>
          </div>
        </Card>
        <Card title="Agents" flush>
          <div class="table-wrap">
            <table class="table">
              <thead
                ><tr
                  ><th>Agent</th><th class="num">Sessions</th><th class="num">Calls</th><th
                    class="num">Tokens</th
                  ><th>Last seen</th></tr
                ></thead
              >
              <tbody>
                {#each u.agents as a (a.agent)}
                  <tr
                    ><td>{a.agent}</td><td class="num">{a.sessions}</td><td class="num"
                      >{formatNumber(a.calls)}</td
                    ><td class="num">{formatCompact(a.tokensReturned)}</td><td
                      >{relativeTime(a.lastSeen)}</td
                    ></tr
                  >
                {/each}
              </tbody>
            </table>
          </div>
        </Card>
      </div>
    </div>
  {/snippet}
</DataState>
