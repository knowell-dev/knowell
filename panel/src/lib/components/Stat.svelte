<script lang="ts">
  import type { Snippet } from 'svelte';

  let {
    label,
    value,
    hint,
    tone = 'neutral',
    children
  }: {
    label: string;
    value: string;
    hint?: string;
    tone?: 'neutral' | 'ok' | 'warn' | 'danger';
    children?: Snippet;
  } = $props();
</script>

<div class="stat {tone}">
  <div class="label">{label}</div>
  <div class="value">{value}</div>
  {#if hint}<div class="hint">{hint}</div>{/if}
  {#if children}{@render children()}{/if}
</div>

<style>
  .stat {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    padding: var(--sp-4) var(--sp-5);
    box-shadow: var(--shadow);
  }
  .label {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    color: var(--text-muted);
    font-size: var(--fs-sm);
  }
  .ok .label::before,
  .warn .label::before,
  .danger .label::before {
    content: '';
    width: 0.45rem;
    height: 0.45rem;
    border-radius: 999px;
  }
  .ok .label::before {
    background: var(--ok);
  }
  .warn .label::before {
    background: var(--warn);
  }
  .danger .label::before {
    background: var(--danger);
  }
  .value {
    font-size: 1.5rem;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }
  .hint {
    color: var(--text-faint);
    font-size: var(--fs-xs);
  }
</style>
