<script lang="ts">
  import { tick } from 'svelte';
  import { provideApi, type ApiClient, type Session } from '$lib/api';
  import { router, href } from '$lib/router.svelte';
  import { matchRoute, notFound, routes } from '$lib/routes';
  import { applyTheme, loadTheme, type Theme } from '$lib/theme';
  import { shortUserId } from '$lib/format';
  import { useResource } from '$lib/resource.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import Login from '$lib/components/Login.svelte';

  let { client, mock = false }: { client: ApiClient; mock?: boolean } = $props();
  // The client is chosen once at start-up and never swapped.
  // svelte-ignore state_referenced_locally
  provideApi(client);
  const session = useResource<Session>(() => client.getSession());
  // True after a 401 (no session yet, expired, revoked or signed out): show the sign-in form.
  let signedOut = $state(false);
  // Bumped after a sign-in so every screen refetches with the new session.
  let epoch = $state(0);
  const needsLogin = $derived(signedOut || session.error?.status === 401);
  $effect(() => client.onUnauthenticated(() => (signedOut = true)));

  function signedIn(s: Session) {
    session.data = s;
    session.error = undefined;
    signedOut = false;
    epoch++;
  }
  async function signOut() {
    try {
      await client.logout();
    } catch {
      // The cookie may already be gone; the form is shown either way.
    }
    session.data = undefined;
    signedOut = true;
  }

  let theme = $state<Theme>(loadTheme());
  let menuOpen = $state(false);
  let main: HTMLElement | undefined = $state();

  const current = $derived(matchRoute(router.current.path));
  const Page = $derived(current?.component ?? notFound);
  const groups = $derived([...new Set(routes.map((r) => r.group))]);

  function toggleTheme() {
    theme = theme === 'dark' ? 'light' : 'dark';
    applyTheme(theme);
  }

  // Move focus to the main region and update the title on navigation (screen-reader friendly).
  let lastPath = router.current.path;
  $effect(() => {
    const p = router.current.path;
    document.title = `${current?.title ?? 'Not found'} - Knowell panel`;
    menuOpen = false;
    if (p !== lastPath) {
      lastPath = p;
      void tick().then(() => main?.focus({ preventScroll: true }));
    }
  });
</script>

{#if needsLogin}
  <Login {client} onsession={signedIn} />
{:else}
  <a
    class="skip"
    href="#main-content"
    onclick={(e) => {
      e.preventDefault();
      main?.focus();
    }}>Skip to content</a
  >

  <div class="shell">
    <aside id="nav-side" class="side" class:open={menuOpen}>
      <a class="brand" href={href('/')} aria-label="Knowell overview">
        <svg viewBox="0 0 64 64" width="28" height="28" aria-hidden="true">
          <rect width="64" height="64" rx="12" fill="#111a2c" />
          <g stroke="#f6f6f0" stroke-width="6" stroke-linecap="round" fill="none">
            <path d="M22 14v36" /><path d="M24 34l18-14M26 36l16 14" />
          </g>
          <circle cx="44" cy="19" r="5.5" fill="#f6f6f0" /><circle
            cx="44"
            cy="49"
            r="5.5"
            fill="#f6f6f0"
          />
          <circle cx="24" cy="35" r="5.5" fill="#3dd6c6" />
        </svg>
        <span>Knowell</span>
      </a>
      <nav aria-label="Main">
        {#each groups as g (g)}
          <div class="group">{g}</div>
          <ul>
            {#each routes.filter((r) => r.group === g) as r (r.path)}
              <li>
                <a
                  href={href(r.path)}
                  aria-current={router.current.path === r.path ? 'page' : undefined}
                  class:active={router.current.path === r.path}>{r.title}</a
                >
              </li>
            {/each}
          </ul>
        {/each}
      </nav>
    </aside>

    <div class="content">
      <header class="top">
        <button
          class="btn ghost menu"
          type="button"
          aria-expanded={menuOpen}
          aria-controls="nav-side"
          onclick={() => (menuOpen = !menuOpen)}>Menu</button
        >
        <div class="spacer"></div>
        {#if mock}
          <Badge tone="warn" title="Showing fictional data from the mock client">mock data</Badge>
        {/if}
        {#if session.data}
          <Badge tone="info" mono title="Engine role">{session.data.role}</Badge>
          <span
            class="who muted small"
            title="{session.data.user}, session ends {session.data.expiresAt}"
            >{shortUserId(session.data.user)}</span
          >
          {#if session.data.role === 'hub'}
            <button class="btn sm" type="button" onclick={signOut}>Sign out</button>
          {/if}
        {/if}
        <button
          class="btn sm"
          type="button"
          onclick={toggleTheme}
          aria-label="Switch to {theme === 'dark' ? 'light' : 'dark'} theme"
        >
          {theme === 'dark' ? 'Light theme' : 'Dark theme'}
        </button>
      </header>
      <main id="main-content" tabindex="-1" bind:this={main}>
        {#key `${router.current.path}#${epoch}`}
          <!-- Re-created on every navigation, so its entry animation replays per page. -->
          <div class="enter"><Page /></div>
        {/key}
      </main>
    </div>
  </div>
{/if}

<style>
  .skip {
    position: absolute;
    left: -999px;
    top: 0;
    background: var(--accent);
    color: var(--accent-contrast);
    padding: var(--sp-2) var(--sp-3);
    z-index: 100;
  }
  .skip:focus {
    left: 0;
  }
  .shell {
    display: grid;
    grid-template-columns: var(--sidebar-w) minmax(0, 1fr);
    min-height: 100vh;
  }
  .side {
    background: var(--bg-sunken);
    border-right: 1px solid transparent;
    padding: var(--sp-3);
    position: sticky;
    top: 0;
    height: 100vh;
    overflow-y: auto;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: var(--sp-2);
    color: var(--text);
    font-weight: 700;
    font-size: var(--fs-lg);
    padding: var(--sp-1) var(--sp-2) var(--sp-3);
  }
  .brand:hover {
    text-decoration: none;
  }
  .group {
    font-size: var(--fs-xs);
    font-weight: 500;
    color: var(--text-faint);
    padding: var(--sp-3) var(--sp-2) var(--sp-1);
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  nav a {
    display: block;
    padding: 0.4rem var(--sp-3);
    border-radius: var(--radius);
    color: var(--text-muted);
    transition:
      background-color 120ms ease,
      color 120ms ease;
  }
  nav a:hover {
    background: var(--surface-2);
    color: var(--text);
    text-decoration: none;
  }
  nav a.active {
    background: var(--surface-2);
    color: var(--text);
    font-weight: 500;
  }
  .content {
    min-width: 0;
    display: flex;
    flex-direction: column;
  }
  .top {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--sp-2) var(--sp-3);
    padding: var(--sp-2) var(--sp-5);
    border-bottom: 1px solid var(--border);
    background: var(--bg);
    position: sticky;
    top: 0;
    z-index: 5;
  }
  main {
    padding: var(--sp-5);
    max-width: 90rem;
    width: 100%;
  }
  main:focus {
    outline: none;
  }
  .menu {
    display: none;
  }
  @media (max-width: 900px) {
    .shell {
      grid-template-columns: minmax(0, 1fr);
    }
    .side {
      display: none;
      position: fixed;
      z-index: 20;
      width: var(--sidebar-w);
      box-shadow: 4px 0 16px rgba(0, 0, 0, 0.4);
    }
    .side.open {
      display: block;
    }
    .menu {
      display: inline-flex;
    }
    main {
      padding: var(--sp-4);
    }
    .top {
      padding: var(--sp-2) var(--sp-4);
    }
  }
  @media (max-width: 600px) {
    /* Keep the theme and sign-out controls on screen; the full id stays in the tooltip. */
    .who {
      display: none;
    }
  }
</style>
