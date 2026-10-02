<script lang="ts">
  import { getApi, type EmbeddingProfile, type SwitchEstimate } from '$lib/api';
  import { useResource } from '$lib/resource.svelte';
  import { formatBytes, formatDuration, formatNumber, formatUsd, relativeTime } from '$lib/format';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import DataState from '$lib/components/DataState.svelte';
  import Card from '$lib/components/Card.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import Meter from '$lib/components/Meter.svelte';
  import SecretRef from '$lib/components/SecretRef.svelte';

  const api = getApi();
  const res = useResource(() => api.listProfiles());

  let estimate = $state<SwitchEstimate | undefined>();
  let estimating = $state<string | undefined>();
  let notice = $state<{ kind: 'ok' | 'error'; text: string } | undefined>();
  let starting = $state(false);

  async function preview(p: EmbeddingProfile) {
    estimating = p.id;
    notice = undefined;
    estimate = undefined;
    try {
      estimate = await api.estimateSwitch(p.id);
    } catch (e) {
      notice = { kind: 'error', text: e instanceof Error ? e.message : 'estimate failed' };
    } finally {
      estimating = undefined;
    }
  }

  async function start() {
    if (!estimate) return;
    starting = true;
    try {
      await api.startSwitch({ toProfileId: estimate.toProfileId });
      notice = {
        kind: 'ok',
        text: `Blue-green switch to ${estimate.toProfileId} started. The current index keeps serving until checks pass.`
      };
      estimate = undefined;
      await res.reload();
    } catch (e) {
      notice = { kind: 'error', text: e instanceof Error ? e.message : 'switch failed' };
    } finally {
      starting = false;
    }
  }

  const LOCALITY = {
    'local-only': 'Local only: no text leaves this machine',
    'cloud-allowed': 'Cloud allowed: redacted chunks may be sent to the provider',
    'cloud-required-hub': 'Cloud via hub only: provider keys exist only on the hub'
  } as const;
</script>

<PageHeader
  title="Model profiles"
  lead="Embedding profiles: provider, dimensions, locality policy and budgets. Dimensions are configuration; quality is measured separately, and an unmeasured profile has no quality score."
/>

{#if notice}
  <div class="notice {notice.kind}" role={notice.kind === 'error' ? 'alert' : 'status'}>
    {notice.text}
  </div>
{/if}

<DataState
  resource={res}
  isEmpty={(d) => d.length === 0}
  emptyTitle="No embedding profiles"
  emptyWhy="Without a profile the engine can only do lexical search and exact symbol lookup. Add a provider profile (or a local ONNX model) to enable semantic search."
>
  {#snippet children(list)}
    <div class="stack">
      {#each list as p (p.id)}
        <Card title={p.name} subtitle="{p.provider} - {p.model}">
          {#snippet actions()}
            {#if p.active}<Badge tone="ok">active</Badge>{/if}
            {#if p.switch}<Badge tone="accent">blue-green: {p.switch.state}</Badge>{/if}
            {#if !p.active && !p.switch}
              <button
                class="btn sm"
                type="button"
                disabled={estimating !== undefined}
                onclick={() => preview(p)}>Preview switch</button
              >
            {/if}
          {/snippet}
          <div class="grid cols-3">
            <div>
              <h3>Configuration</h3>
              <dl class="kv">
                <dt>Dimensions</dt>
                <dd class="mono">{p.dimensions}</dd>
                <dt>Vector storage</dt>
                <dd>{p.storage}</dd>
                <dt>Locality</dt>
                <dd>
                  <Badge tone={p.locality === 'local-only' ? 'ok' : 'warn'}>{p.locality}</Badge>
                  <div class="faint small">{LOCALITY[p.locality]}</div>
                </dd>
                <dt>API key</dt>
                <dd><SecretRef reference={p.apiKey} /></dd>
              </dl>
            </div>
            <div>
              <h3>Measured quality</h3>
              {#if p.measured}
                <dl class="kv">
                  <dt>recall@10</dt>
                  <dd class="mono">{p.measured.recallAt10.toFixed(2)}</dd>
                  <dt>MRR</dt>
                  <dd class="mono">{p.measured.mrr.toFixed(2)}</dd>
                  <dt>Query set</dt>
                  <dd><code>{p.measured.querySet}</code></dd>
                  <dt>Measured</dt>
                  <dd>
                    {relativeTime(p.measured.measuredAt)}{#if p.measured.hardware}<div
                        class="faint small"
                      >
                        {p.measured.hardware}
                      </div>{/if}
                  </dd>
                </dl>
              {:else}
                <Badge tone="neutral">not measured</Badge>
                <p class="faint small">
                  Run an evaluation on the Quality screen. Dimensions say nothing about quality, so
                  none is shown.
                </p>
              {/if}
            </div>
            <div>
              <h3>Budgets and spend</h3>
              <dl class="kv">
                <dt>This month</dt>
                <dd>
                  {formatUsd(p.spentThisMonthUsdMicros)}{#if p.budgets.monthlyUsdMicros}
                    of {formatUsd(p.budgets.monthlyUsdMicros)}{/if}
                </dd>
                {#if p.budgets.tokensPerMinute}<dt>Rate limit</dt>
                  <dd>{formatNumber(p.budgets.tokensPerMinute)} tokens/min</dd>{/if}
                {#if p.budgets.diskBytes}<dt>Disk budget</dt>
                  <dd>{formatBytes(p.budgets.diskBytes)}</dd>{/if}
              </dl>
              {#if p.budgets.monthlyUsdMicros}
                <Meter
                  value={p.spentThisMonthUsdMicros / p.budgets.monthlyUsdMicros}
                  label="Monthly budget used"
                  tone={p.spentThisMonthUsdMicros / p.budgets.monthlyUsdMicros > 0.8
                    ? 'warn'
                    : 'accent'}
                />
              {/if}
            </div>
          </div>
          {#if p.switch}
            <div class="switch">
              <strong>Blue-green switch:</strong>
              {p.switch.state}. The old index keeps serving until quality and coverage checks pass.
              <Meter value={p.switch.progress} label="Switch progress" />
            </div>
          {/if}
        </Card>
      {/each}

      {#if estimate}
        {@const e = estimate}
        <Card
          title="Switch preview: {e.fromProfileId} to {e.toProfileId}"
          subtitle="Nothing has changed yet. Review the impact first."
        >
          <div class="grid cols-4">
            <div>
              <div class="faint small">Projects affected</div>
              <strong>{e.affectedProjects.length}</strong>
            </div>
            <div>
              <div class="faint small">Chunks to regenerate</div>
              <strong>{formatNumber(e.chunksToRegenerate)}</strong>
            </div>
            <div>
              <div class="faint small">Estimated cost</div>
              <strong>{formatUsd(e.estimatedCostUsdMicros)}</strong>
              <span class="faint small">({formatNumber(e.estimatedTokens)} tokens)</span>
            </div>
            <div>
              <div class="faint small">Estimated time</div>
              <strong>{formatDuration(e.estimatedDurationMs)}</strong>
            </div>
            <div>
              <div class="faint small">Extra disk</div>
              <strong>{formatBytes(e.estimatedDiskBytes)}</strong>
            </div>
            <div>
              <div class="faint small">API calls needed</div>
              <strong
                >{e.needsReembedding ? 'yes (re-embedding)' : 'no (truncate and normalise)'}</strong
              >
            </div>
          </div>
          <p class="small muted">Affected: {e.affectedProjects.join(', ')}</p>
          {#if e.warnings.length}
            <ul class="warnings">
              {#each e.warnings as w (w)}<li>{w}</li>{/each}
            </ul>
          {/if}
          <div class="row">
            <button class="btn primary" type="button" disabled={starting} onclick={start}
              >Start blue-green switch</button
            >
            <button class="btn" type="button" onclick={() => (estimate = undefined)}>Cancel</button>
          </div>
        </Card>
      {/if}
    </div>
  {/snippet}
</DataState>

<style>
  .notice {
    padding: var(--sp-2) var(--sp-3);
    border-radius: var(--radius);
    border: 1px solid var(--ok);
    background: var(--ok-bg);
    margin-bottom: var(--sp-3);
  }
  .notice.error {
    border-color: var(--danger);
    background: var(--danger-bg);
  }
  h3 {
    margin-bottom: var(--sp-2);
  }
  .kv {
    display: grid;
    grid-template-columns: 7.5rem 1fr;
    gap: var(--sp-1) var(--sp-3);
    margin: 0;
  }
  dt {
    color: var(--text-muted);
  }
  dd {
    margin: 0;
  }
  .switch {
    margin-top: var(--sp-3);
    padding-top: var(--sp-3);
    border-top: 1px solid var(--border);
  }
  .warnings {
    border: 1px solid var(--warn);
    background: var(--warn-bg);
    border-radius: var(--radius);
    padding: var(--sp-2) var(--sp-3) var(--sp-2) 1.6rem;
    margin: var(--sp-3) 0;
  }
</style>
