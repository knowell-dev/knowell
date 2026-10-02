<script lang="ts">
  import type { Snippet } from 'svelte';

  let {
    title,
    subtitle,
    actions,
    children,
    flush = false
  }: {
    title?: string;
    subtitle?: string;
    actions?: Snippet;
    children: Snippet;
    flush?: boolean;
  } = $props();
  const uid = $props.id();
</script>

<section class="card" aria-labelledby={title ? `${uid}-t` : undefined}>
  {#if title || actions}
    <header>
      <div>
        {#if title}<h2 id="{uid}-t">{title}</h2>{/if}
        {#if subtitle}<p class="muted small">{subtitle}</p>{/if}
      </div>
      {#if actions}<div class="row">{@render actions()}</div>{/if}
    </header>
  {/if}
  <div class="body" class:flush>{@render children()}</div>
</section>

<style>
  .card {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    box-shadow: var(--shadow);
    min-width: 0;
  }
  header {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: var(--sp-3);
    padding: var(--sp-3) var(--sp-4);
    border-bottom: 1px solid var(--border);
  }
  h2 {
    font-size: var(--fs-md);
  }
  .body {
    padding: var(--sp-4);
  }
  .body.flush {
    padding: 0;
  }
</style>
