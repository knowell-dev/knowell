<script lang="ts">
  import { splitInlineCode } from '$lib/format';

  // Text with `backtick` commands; rendered as text nodes and <code>, never as HTML.
  let { text }: { text: string } = $props();
  const parts = $derived(splitInlineCode(text));
</script>

{#each parts as p, i (i)}{#if p.code}<code>{p.text}</code>{:else}{p.text}{/if}{/each}
