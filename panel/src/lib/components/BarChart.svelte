<script lang="ts">
  let {
    values,
    labels,
    title,
    height = 80
  }: {
    values: number[];
    labels: string[];
    title: string;
    height?: number;
  } = $props();
  const max = $derived(Math.max(1, ...values));
  const w = 600;
  const bw = $derived(values.length ? w / values.length : w);
</script>

<figure>
  <svg viewBox="0 0 {w} {height}" role="img" aria-label={title} preserveAspectRatio="none">
    <title>{title}</title>
    {#each values as v, i (i)}
      <rect
        x={i * bw + 1}
        y={height - (v / max) * (height - 4)}
        width={Math.max(1, bw - 2)}
        height={(v / max) * (height - 4)}
        fill="var(--accent)"
        opacity="0.85"
      >
        <title>{labels[i]}: {v}</title>
      </rect>
    {/each}
  </svg>
</figure>

<style>
  figure {
    margin: 0;
  }
  svg {
    width: 100%;
    height: 5rem;
    display: block;
  }
</style>
