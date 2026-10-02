<script lang="ts">
  import type { Origin } from '$lib/api';
  import Badge from './Badge.svelte';

  let { origin, note }: { origin: Origin; note?: string } = $props();
  const tone = $derived(
    origin === 'project' ? 'accent' : origin === 'workspace' ? 'info' : 'neutral'
  );
</script>

<Badge {tone} title={note ? `Comes from ${note}` : `Comes from ${origin} settings`}>{origin}</Badge>
