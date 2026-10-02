<script lang="ts">
  /** Shown where the server reports `null`: the value is unknown, never zero or empty. */
  let { why }: { why?: string } = $props();
</script>

<span class="faint" title={why ?? 'The server does not report this value yet'}
  >not reported yet</span
>
