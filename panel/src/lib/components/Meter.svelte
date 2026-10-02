<script lang="ts">
  let {
    value,
    label,
    tone = 'accent',
    showText = true
  }: {
    /** 0..1 */
    value: number;
    label: string;
    tone?: 'accent' | 'ok' | 'warn' | 'danger';
    showText?: boolean;
  } = $props();
  const pct = $derived(Math.max(0, Math.min(1, value)) * 100);
</script>

<div class="meter">
  <div
    class="track"
    role="meter"
    aria-label={label}
    aria-valuemin="0"
    aria-valuemax="100"
    aria-valuenow={Math.round(pct)}
  >
    <div class="fill {tone}" style:width="{pct}%"></div>
  </div>
  {#if showText}<span class="text num">{pct.toFixed(0)}%</span>{/if}
</div>

<style>
  .meter {
    display: flex;
    align-items: center;
    gap: var(--sp-2);
    min-width: 6rem;
  }
  .track {
    flex: 1;
    height: 0.5rem;
    background: var(--bg-sunken);
    border: 1px solid var(--border);
    border-radius: 999px;
    overflow: hidden;
  }
  .fill {
    height: 100%;
    background: var(--accent);
  }
  .fill.ok {
    background: var(--ok);
  }
  .fill.warn {
    background: var(--warn);
  }
  .fill.danger {
    background: var(--danger);
  }
  .text {
    font-size: var(--fs-xs);
    min-width: 2.6rem;
    color: var(--text-muted);
  }
</style>
