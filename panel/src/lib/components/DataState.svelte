<script lang="ts" generics="T">
  import type { Snippet } from 'svelte';
  import type { Resource } from '$lib/resource.svelte';
  import EmptyState from './EmptyState.svelte';
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
    <p>{resource.error.message}</p>
    <p class="faint small mono">
      {resource.error.code}{resource.error.requestId
        ? ` - request ${resource.error.requestId}`
        : ''}
    </p>
    <button class="btn" type="button" onclick={() => resource.reload()}>Retry</button>
  </div>
{:else if resource.loading && resource.data === undefined}
  <div class="loading" role="status" aria-live="polite">
    <span class="spinner" aria-hidden="true"></span> Loading...
  </div>
{:else if resource.data !== undefined}
  {#if resource.error}
    <div class="error inline" role="alert">
      Refresh failed: {resource.error.message}
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
    color: var(--text-muted);
    padding: var(--sp-4);
    display: flex;
    gap: var(--sp-2);
    align-items: center;
  }
  .spinner {
    width: 0.9rem;
    height: 0.9rem;
    border: 2px solid var(--border-strong);
    border-top-color: var(--accent);
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
</style>
