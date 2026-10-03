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
    padding: 0.1rem 0.55rem;
    border-radius: 999px;
    border: 1px solid transparent;
    font-weight: 500;
    font-size: var(--fs-xs);
    line-height: 1.5;
    white-space: nowrap;
    color: var(--text-muted);
    background: var(--surface-2);
  }
  .ok {
    color: var(--ok);
    background: var(--ok-bg);
  }
  .warn {
    color: var(--warn);
    background: var(--warn-bg);
  }
  .danger {
    color: var(--danger);
    background: var(--danger-bg);
  }
  .info {
    color: var(--info);
    background: var(--info-bg);
  }
  .accent {
    color: var(--accent);
    background: var(--accent-bg);
  }
</style>
