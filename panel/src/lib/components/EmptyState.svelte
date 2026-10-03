<script lang="ts">
  import type { Snippet } from 'svelte';
  import InlineText from './InlineText.svelte';

  let { title, why, children }: { title: string; why: string; children?: Snippet } = $props();
</script>

<div class="empty" role="status">
  <h3>{title}</h3>
  <p class="muted"><InlineText text={why} /></p>
  {#if children}<div class="action">{@render children()}</div>{/if}
</div>

<style>
  .empty {
    padding: var(--sp-5);
    text-align: center;
    border: 1px dashed var(--border-strong);
    border-radius: var(--radius);
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--sp-2);
  }
  p {
    max-width: 44rem;
  }
</style>
