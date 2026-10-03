<script lang="ts">
  import type { Snippet } from 'svelte';
  import InlineText from './InlineText.svelte';

  let { title, lead, actions }: { title: string; lead?: string; actions?: Snippet } = $props();
</script>

<header class="ph">
  <div>
    <h1>{title}</h1>
    {#if lead}<p class="muted"><InlineText text={lead} /></p>{/if}
  </div>
  {#if actions}<div class="row">{@render actions()}</div>{/if}
</header>

<style>
  .ph {
    display: flex;
    justify-content: space-between;
    align-items: flex-end;
    gap: var(--sp-4);
    flex-wrap: wrap;
    margin-bottom: var(--sp-4);
  }
  p {
    margin-top: var(--sp-1);
    max-width: 52rem;
  }
</style>
