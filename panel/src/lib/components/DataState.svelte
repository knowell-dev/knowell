<script lang="ts" generics="T">
  import type { Snippet } from 'svelte';
  import type { Resource } from '$lib/resource.svelte';
  import EmptyState from './EmptyState.svelte';
  import InlineText from './InlineText.svelte';
  import { isUnavailable } from '$lib/api';

  let {
    resource,
    isEmpty,
    emptyTitle = 'Nothing here yet',
    emptyWhy = '',
    children,
    emptyAction
  }: {
    resource: Resource<T>;
    isEmpty?: (data: T) => boolean;
    emptyTitle?: string;
    emptyWhy?: string;
    children: Snippet<[T]>;
    emptyAction?: Snippet;
  } = $props();
</script>

{#if resource.error && resource.data === undefined && isUnavailable(resource.error)}
  <!-- 503 engine_unavailable / store_unavailable: the server says why; this is not a crash. -->
  <EmptyState
    title={resource.error.code === 'engine_unavailable'
      ? 'The engine is not available'
      : resource.error.code === 'store_unavailable'
        ? 'The database is not available'
        : 'Not initialized yet'}
    why={resource.error.message}
  >
    <button class="btn" type="button" onclick={() => resource.reload()}>Check again</button>
  </EmptyState>
{:else if resource.error && resource.data === undefined}
  <div class="error" role="alert">
    <strong>Could not load this data.</strong>
    <p><InlineText text={resource.error.message} /></p>
    <p class="faint small mono">
      {resource.error.code}{resource.error.requestId
        ? ` - request ${resource.error.requestId}`
        : ''}
    </p>
    <button class="btn" type="button" onclick={() => resource.reload()}>Retry</button>
  </div>
{:else if resource.loading && resource.data === undefined}
  <!-- Placeholder lines appear only if loading takes longer than 200 ms, so fast local
       responses never flash a skeleton. Screen readers still hear the status at once. -->
  <div class="loading" role="status" aria-live="polite">
    <span class="sr-only">Loading...</span>
    {#each [38, 92, 76, 84, 58] as w, i (i)}
      <span class="bone" class:title={i === 0} style:width="{w}%" aria-hidden="true"></span>
    {/each}
  </div>
{:else if resource.data !== undefined}
  {#if resource.error}
    <div class="error inline" role="alert">
      Refresh failed: <InlineText text={resource.error.message} />
      <button class="btn sm" type="button" onclick={() => resource.reload()}>Retry</button>
    </div>
  {/if}
  {#if isEmpty?.(resource.data)}
    <EmptyState title={emptyTitle} why={emptyWhy}>
      {#if emptyAction}{@render emptyAction()}{/if}
    </EmptyState>
  {:else}
    {@render children(resource.data)}
  {/if}
{/if}

<style>
  .error {
    border: 1px solid var(--danger);
    background: var(--danger-bg);
    border-radius: var(--radius);
    padding: var(--sp-4);
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--sp-2);
  }
  .error.inline {
    flex-direction: row;
    align-items: center;
    margin-bottom: var(--sp-3);
  }
  .loading {
    padding: var(--sp-4);
    display: flex;
    flex-direction: column;
    gap: var(--sp-3);
    animation: kn-appear 160ms ease-out 200ms both;
  }
  .bone {
    display: block;
    height: 0.75rem;
    border-radius: 6px;
    background: linear-gradient(
      90deg,
      var(--surface-2) 30%,
      var(--shimmer) 50%,
      var(--surface-2) 70%
    );
    background-size: 300% 100%;
    animation: kn-shimmer 1.6s ease-in-out infinite;
  }
  .bone.title {
    height: 1rem;
    margin-bottom: var(--sp-1);
  }
</style>
