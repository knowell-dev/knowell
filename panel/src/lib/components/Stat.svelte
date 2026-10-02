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
    border-left: 3px solid var(--border-strong);
    border-radius: var(--radius);
    padding: var(--sp-3) var(--sp-4);
  }
  .ok {
    border-left-color: var(--ok);
  }
  .warn {
    border-left-color: var(--warn);
  }
  .danger {
    border-left-color: var(--danger);
  }
  .label {
    color: var(--text-muted);
    font-size: var(--fs-xs);
    text-transform: uppercase;
    letter-spacing: 0.04em;
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
