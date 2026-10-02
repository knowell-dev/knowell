import { describe, expect, it } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { apiContext, type ApiClient, type ProgressEvent } from '$lib/api';
import { MockApiClient } from '$lib/api/mock';
import App from '../src/App.svelte';
import Overview from '../src/routes/Overview.svelte';
import Workspaces from '../src/routes/Workspaces.svelte';
import Projects from '../src/routes/Projects.svelte';
import Indexes from '../src/routes/Indexes.svelte';
import Jobs from '../src/routes/Jobs.svelte';
import Search from '../src/routes/Search.svelte';
import Memory from '../src/routes/Memory.svelte';

const mk = (o = {}) => new MockApiClient({ latencyMs: 0, shape: 'server', ...o });
const ctx = (c: ApiClient) => ({ context: apiContext(c) });

/** No screen may ever print a raw null/undefined to the user. */
function expectNoLiterals(container: HTMLElement) {
  const text = container.textContent ?? '';
  expect(text).not.toMatch(/\bundefined\b/);
  expect(text).not.toMatch(/\bnull\b/);
  expect(text).not.toMatch(/\bNaN\b/);
}

describe('screens with the nulls the real server sends', () => {
  it('Overview explains missing queue-adjacent data instead of printing it', async () => {
    const { container } = render(Overview, ctx(mk()));
    expect(await screen.findByText('Freshness tiers')).toBeInTheDocument();
    expect(screen.getAllByText(/not reported yet/).length).toBeGreaterThanOrEqual(3);
    expect(screen.getByText('6 queued')).toBeInTheDocument();
    expectNoLiterals(container);
  });

  it('Overview handles a missing database (queue is null)', async () => {
    const c = mk();
    c.getHealth = async () => ({
      status: 'down',
      version: '0.1.0',
      role: 'standalone',
      uptimeMs: 1000,
      bindAddress: '127.0.0.1:7420',
      components: [{ name: 'database', status: 'down', detail: 'no database is configured' }],
      queue: null,
      freshness: null,
      recentErrors: null,
      resources: null
    });
    const { container } = render(Overview, ctx(c));
    expect(await screen.findByText('no database is configured')).toBeInTheDocument();
    expect(screen.getAllByText(/not reported yet/).length).toBeGreaterThanOrEqual(5);
    expectNoLiterals(container);
  });

  it('Workspaces shows unknown settings and the settings error', async () => {
    const { container } = render(Workspaces, ctx(mk()));
    expect(await screen.findByText('shopfront')).toBeInTheDocument();
    expect(screen.getAllByText('not reported yet').length).toBeGreaterThan(0);
    expectNoLiterals(container);
    await fireEvent.click(screen.getByRole('button', { name: 'playground' }));
    expect((await screen.findAllByText(/could not be loaded/)).length).toBeGreaterThan(0);
    expect(screen.getByTitle('Workspace membership is not tracked yet')).toBeInTheDocument();
    expectNoLiterals(container);
  });

  it('Projects list and detail survive null kind, languages, files and settings', async () => {
    location.hash = '#/projects?id=p-orders';
    const { container } = render(Projects, ctx(mk()));
    expect(await screen.findByText('Effective settings')).toBeInTheDocument();
    expect(screen.getAllByText('not reported yet').length).toBeGreaterThan(8);
    // Workspace name column, per-pattern excludes with their own origin, embedding block.
    expect(screen.getAllByText('shopfront').length).toBeGreaterThan(0);
    expect(screen.getByText('node_modules/**')).toBeInTheDocument();
    expect(screen.getByText('Embedding preset')).toBeInTheDocument();
    expect(screen.getByText(/worktree discovery is not reported yet/)).toBeInTheDocument();
    expect(screen.getByText(/not tracked yet/)).toBeInTheDocument();
    expectNoLiterals(container);
  });

  it('Projects detail shows every origin for a fully populated project too', async () => {
    location.hash = '#/projects?id=p-migrations';
    render(Projects, ctx(new MockApiClient({ latencyMs: 0 })));
    expect(await screen.findByText('sql/seeds/**')).toBeInTheDocument();
    expect(screen.getByText('Vector dimensions')).toBeInTheDocument();
  });

  it('Indexes shows null tiers, analysis, profile and chunk counts as not reported', async () => {
    const { container } = render(Indexes, ctx(mk()));
    expect((await screen.findAllByText('Last seen commit')).length).toBeGreaterThan(0);
    expect(screen.getAllByText(/tier coverage is not tracked yet/).length).toBeGreaterThan(0);
    expect(screen.getAllByText(/analysis coverage is not tracked yet/).length).toBeGreaterThan(0);
    await fireEvent.click(
      screen.getAllByRole('button', { name: /Show generations/ })[0] as HTMLElement
    );
    expect(screen.getAllByText('not reported yet').length).toBeGreaterThan(1);
    expectNoLiterals(container);
    await fireEvent.click(screen.getByRole('tab', { name: /Profile migrations/ }));
    expect(screen.getByText('Profile migrations are reported by the engine')).toBeInTheDocument();
    await fireEvent.click(screen.getByRole('tab', { name: /Dead-letter queue/ }));
    expect(screen.getByText('not named')).toBeInTheDocument();
    expect(screen.getByText('no error message recorded')).toBeInTheDocument();
    expectNoLiterals(container);
  });

  it('Indexes without org-wide read explains the hidden job queue', async () => {
    render(Indexes, ctx(mk({ orgWide: false })));
    await fireEvent.click(await screen.findByRole('tab', { name: /^Jobs/ }));
    expect(screen.getByText('Job queue not visible')).toBeInTheDocument();
    await fireEvent.click(screen.getByRole('tab', { name: /Dead-letter queue/ }));
    expect(screen.getByText('Dead-letter queue not visible')).toBeInTheDocument();
  });

  it('Indexes reports an idempotent reindex that already existed', async () => {
    const c = mk();
    c.reindex = async () => ({ jobId: 'job-9', created: false });
    render(Indexes, ctx(c));
    await fireEvent.click(
      (await screen.findAllByRole('button', { name: 'Reindex changed' }))[0] as HTMLElement
    );
    expect(await screen.findByText(/already queued as job-9/)).toBeInTheDocument();
  });

  it('Indexes explains a retry that hit 409 not_dead and refreshes', async () => {
    const c = mk();
    render(Indexes, ctx(c));
    await fireEvent.click(await screen.findByRole('tab', { name: /Dead-letter queue/ }));
    await c.retryDeadLetter('job-4777');
    await fireEvent.click(screen.getAllByRole('button', { name: 'Retry' })[0] as HTMLElement);
    expect(await screen.findByText(/no longer dead-lettered/)).toBeInTheDocument();
  });

  it('Indexes refetches on a resync event', async () => {
    const c = mk();
    let push: ((e: ProgressEvent) => void) | undefined;
    c.subscribeProgress = (on) => {
      push = on;
      return () => {};
    };
    let calls = 0;
    const original = c.getIndexes;
    c.getIndexes = () => {
      calls++;
      return original();
    };
    render(Indexes, ctx(c));
    await screen.findAllByText('Last seen commit');
    expect(calls).toBe(1);
    push?.({ type: 'resync', missed: 12 });
    await waitFor(() => expect(calls).toBe(2));
  });

  it('Jobs lists jobs with kinds, filters by state and refetches on resync', async () => {
    const c = mk();
    let push: ((e: ProgressEvent) => void) | undefined;
    c.subscribeProgress = (on) => {
      push = on;
      return () => {};
    };
    const seen: (string | undefined)[] = [];
    const original = c.listJobs;
    c.listJobs = (q) => {
      seen.push(q?.state);
      return original(q);
    };
    const { container } = render(Jobs, ctx(c));
    expect((await screen.findAllByText('view.reindex')).length).toBeGreaterThan(0);
    expect(screen.getAllByText('source.refresh').length).toBeGreaterThan(0);
    await fireEvent.change(screen.getByLabelText('State'), { target: { value: 'dead' } });
    await waitFor(() => expect(seen).toContain('dead'));
    push?.({ type: 'resync', missed: 1 });
    await waitFor(() => expect(seen.length).toBe(3));
    expectNoLiterals(container);
  });

  it('Jobs without org-wide read shows the server refusal', async () => {
    render(Jobs, ctx(mk({ orgWide: false })));
    expect(await screen.findByRole('alert')).toHaveTextContent('organization-wide read access');
  });
});

describe('503 engine_unavailable is an explanation, not a crash', () => {
  const reason = 'the indexing engine is not connected to this server yet';

  it('Search shows the reason after a query', async () => {
    render(Search, ctx(mk({ engineUnavailable: reason })));
    const input = await screen.findByRole('searchbox');
    await fireEvent.input(input, { target: { value: 'PlaceOrder' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    expect(await screen.findByText(new RegExp(reason))).toBeInTheDocument();
  });

  it('Memory shows an explanatory empty state with the reason', async () => {
    render(Memory, ctx(mk({ engineUnavailable: reason })));
    expect(await screen.findByText('The engine is not available')).toBeInTheDocument();
    expect(screen.getByText(reason)).toBeInTheDocument();
    expect(screen.queryByText('Could not load this data.')).not.toBeInTheDocument();
  });
});

describe('hub sign-in', () => {
  it('shows the sign-in form, opens a session and can sign out again', async () => {
    const c = mk({ role: 'hub', requireLogin: true });
    render(App, { props: { client: c } });
    const input = await screen.findByLabelText('API token');
    await fireEvent.input(input, { target: { value: 'wrong' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Sign in' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('api token is not valid');
    await fireEvent.input(input, { target: { value: 'kn_fake_token_value' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Sign in' }));
    expect(await screen.findByRole('button', { name: 'Sign out' })).toBeInTheDocument();
    expect(screen.getByText(/^user:/)).toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: 'Sign out' }));
    expect(await screen.findByLabelText('API token')).toBeInTheDocument();
  });
});
