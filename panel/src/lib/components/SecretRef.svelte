<script lang="ts">
  /** Shows a secret reference only. There is deliberately no way to pass or reveal a value. */
  let { reference }: { reference: string | undefined } = $props();
  // Defence in depth: anything that is not a reference is not rendered.
  const safe = $derived(
    reference && /^(env|file|keyring):[A-Za-z0-9_./\\:-]+$/.test(reference) ? reference : undefined
  );
</script>

{#if safe}
  <code title="Secret reference. The value is never shown.">{safe}</code>
{:else if reference}
  <span class="muted">(hidden)</span>
{:else}
  <span class="faint">none</span>
{/if}
