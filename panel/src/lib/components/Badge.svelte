<script lang="ts">
  import type { Snippet } from 'svelte';

  type Tone = 'neutral' | 'ok' | 'warn' | 'danger' | 'info' | 'accent';
  let {
    tone = 'neutral',
    mono = false,
    title,
    children
  }: {
    tone?: Tone;
    mono?: boolean;
    title?: string;
    children: Snippet;
  } = $props();
</script>

<span class="badge {tone}" class:mono {title}>{@render children()}</span>

<style>
  .badge {
    display: inline-block;
    padding: 0.05rem 0.45rem;
    border-radius: 999px;
    border: 1px solid var(--border-strong);
    font-size: var(--fs-xs);
    line-height: 1.5;
    white-space: nowrap;
    color: var(--text-muted);
    background: var(--surface-2);
  }
  .ok {
    color: var(--ok);
    border-color: var(--ok);
    background: var(--ok-bg);
  }
  .warn {
    color: var(--warn);
    border-color: var(--warn);
    background: var(--warn-bg);
  }
  .danger {
    color: var(--danger);
    border-color: var(--danger);
    background: var(--danger-bg);
  }
  .info {
    color: var(--info);
    border-color: var(--info);
    background: var(--info-bg);
  }
  .accent {
    color: var(--accent);
    border-color: var(--accent);
    background: var(--accent-bg);
  }
</style>
