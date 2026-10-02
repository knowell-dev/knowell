<script lang="ts">
  /** Repository text is untrusted: it is only ever rendered as text nodes inside <pre>. */
  let { text, startLine, label }: { text: string; startLine?: number; label?: string } = $props();
  const lines = $derived(text.split('\n'));
</script>

<!-- A scrollable region must be keyboard-focusable. -->
<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<pre
  class="code"
  aria-label={label}
  tabindex="0">{#each lines as line, i (i)}{#if startLine !== undefined}<span
        class="ln"
        aria-hidden="true"
        >{String(startLine + i).padStart(4)}  </span>{/if}{line}
  {/each}</pre>

<style>
  .ln {
    color: var(--text-faint);
    user-select: none;
  }
</style>
