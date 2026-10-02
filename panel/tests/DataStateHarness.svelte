<script lang="ts">
  import DataState from '$lib/components/DataState.svelte';
  import type { Resource } from '$lib/resource.svelte';

  let { resource }: { resource: Resource<string[]> } = $props();
</script>

<DataState
  {resource}
  isEmpty={(d) => d.length === 0}
  emptyTitle="Nothing indexed"
  emptyWhy="project not indexed yet"
>
  {#snippet children(items)}
    <ul>
      {#each items as i (i)}<li>{i}</li>{/each}
    </ul>
  {/snippet}
</DataState>
