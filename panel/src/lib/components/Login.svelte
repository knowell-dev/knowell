<script lang="ts">
  import type { ApiClient, Session } from '$lib/api';

  let { client, onsession }: { client: ApiClient; onsession: (s: Session) => void } = $props();

  // The token lives only in this input and in one request; it is never stored by the panel.
  let token = $state('');
  let busy = $state(false);
  let error = $state<string | undefined>();

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    if (busy || token.trim() === '') return;
    busy = true;
    error = undefined;
    try {
      const s = await client.login(token.trim());
      token = '';
      onsession(s);
    } catch (err) {
      error = err instanceof Error ? err.message : 'sign-in failed';
    } finally {
      busy = false;
    }
  }
</script>

<main class="login" id="main-content">
  <form class="card" onsubmit={submit}>
    <h1>Sign in to Knowell</h1>
    <p class="muted">
      This server needs an API token of a user (it starts with <code>kn_</code>). The panel session
      is limited to what that token may do. The token is sent once and not stored in the browser.
    </p>
    <label for="token">API token</label>
    <input
      id="token"
      name="token"
      type="password"
      autocomplete="off"
      spellcheck="false"
      bind:value={token}
      aria-describedby={error ? 'login-error' : undefined}
      required
    />
    {#if error}
      <p id="login-error" class="err" role="alert">{error}</p>
    {/if}
    <button class="btn primary" type="submit" disabled={busy || token.trim() === ''}>
      {busy ? 'Signing in...' : 'Sign in'}
    </button>
  </form>
</main>

<style>
  .login {
    min-height: 100vh;
    display: grid;
    place-items: center;
    padding: var(--sp-4);
  }
  .card {
    width: min(26rem, 100%);
    display: flex;
    flex-direction: column;
    gap: var(--sp-3);
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: var(--sp-5);
  }
  h1 {
    font-size: var(--fs-lg);
    margin: 0;
  }
  input {
    font: inherit;
    padding: var(--sp-2);
    background: var(--bg);
    color: var(--text);
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
  }
  .err {
    color: var(--danger);
    margin: 0;
  }
</style>
