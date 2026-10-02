<script lang="ts">
  let {
    tabs,
    active = $bindable(),
    label
  }: {
    tabs: { id: string; label: string; count?: number }[];
    active: string;
    label: string;
  } = $props();
  const uid = $props.id();

  function onkey(e: KeyboardEvent, i: number) {
    let n = -1;
    if (e.key === 'ArrowRight') n = (i + 1) % tabs.length;
    else if (e.key === 'ArrowLeft') n = (i - 1 + tabs.length) % tabs.length;
    else if (e.key === 'Home') n = 0;
    else if (e.key === 'End') n = tabs.length - 1;
    if (n < 0) return;
    e.preventDefault();
    const t = tabs[n];
    if (!t) return;
    active = t.id;
    document.getElementById(`${uid}-${t.id}`)?.focus();
  }
</script>

<div class="tabs" role="tablist" aria-label={label}>
  {#each tabs as t, i (t.id)}
    <button
      id="{uid}-{t.id}"
      type="button"
      role="tab"
      aria-selected={active === t.id}
      tabindex={active === t.id ? 0 : -1}
      class:active={active === t.id}
      onclick={() => (active = t.id)}
      onkeydown={(e) => onkey(e, i)}
    >
      {t.label}{#if t.count !== undefined}<span class="count">{t.count}</span>{/if}
    </button>
  {/each}
</div>

<style>
  .tabs {
    display: flex;
    gap: var(--sp-1);
    border-bottom: 1px solid var(--border);
    overflow-x: auto;
  }
  button {
    background: none;
    border: 0;
    border-bottom: 2px solid transparent;
    padding: var(--sp-2) var(--sp-3);
    color: var(--text-muted);
    cursor: pointer;
    white-space: nowrap;
    margin-bottom: -1px;
  }
  button:hover {
    color: var(--text);
  }
  button.active {
    color: var(--text);
    border-bottom-color: var(--accent);
    font-weight: 600;
  }
  .count {
    margin-left: var(--sp-2);
    font-size: var(--fs-xs);
    background: var(--surface-2);
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 0 0.4rem;
  }
</style>
