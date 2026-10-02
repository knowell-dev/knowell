<script lang="ts">
  import EmptyState from '$lib/components/EmptyState.svelte';
  import { href } from '$lib/router.svelte';
</script>

<EmptyState title="Page not found" why="This address does not match any panel screen.">
  <a class="btn" href={href('/')}>Back to overview</a>
</EmptyState>
