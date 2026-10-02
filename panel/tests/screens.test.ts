import { describe, expect, it } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { ApiError, apiContext } from '$lib/api';
import { MockApiClient } from '$lib/api/mock';
import Overview from '../src/routes/Overview.svelte';
import Memory from '../src/routes/Memory.svelte';
import Admin from '../src/routes/Admin.svelte';
import Models from '../src/routes/Models.svelte';
import Indexes from '../src/routes/Indexes.svelte';
import Search from '../src/routes/Search.svelte';
import Graph from '../src/routes/Graph.svelte';

const mk = (o = {}) => new MockApiClient({ latencyMs: 0, ...o });
const ctx = (c: MockApiClient) => ({ context: apiContext(c) });

describe('screens against the mock client', () => {
  it('Overview shows health, queue and freshness tiers', async () => {
    render(Overview, ctx(mk()));
    expect(await screen.findByText(/T2 embeddings/)).toBeInTheDocument();
    expect(screen.getByText('Recent errors')).toBeInTheDocument();
    expect(screen.getByText('6 queued')).toBeInTheDocument();
  });

  it('Overview shows an error state with retry when the engine fails', async () => {
    render(
      Overview,
      ctx(mk({ failures: { getHealth: new ApiError('network', 'cannot reach the engine') } }))
    );
    expect(await screen.findByRole('alert')).toHaveTextContent('cannot reach the engine');
    expect(screen.getByRole('button', { name: 'Retry' })).toBeInTheDocument();
  });

  it('Memory accepts a proposal and moves it out of the queue', async () => {
    render(Memory, ctx(mk()));
    expect(await screen.findByText('Refund flow touches three services')).toBeInTheDocument();
    const before = screen.getAllByRole('button', { name: 'Accept' }).length;
    await fireEvent.click(screen.getAllByRole('button', { name: 'Accept' })[0] as HTMLElement);
    await waitFor(() =>
      expect(screen.getAllByRole('button', { name: 'Accept' }).length).toBe(before - 1)
    );
    expect(await screen.findByText(/Accepted/)).toBeInTheDocument();
  });

  it('Memory empty state explains why', async () => {
    render(Memory, ctx(mk({ empty: true })));
    expect(await screen.findByText('Memory is empty')).toBeInTheDocument();
  });

  it('Admin is hub-only outside the hub role and populated in the hub role', async () => {
    const a = render(Admin, ctx(mk()));
    expect(await screen.findByText('Available in the hub role only')).toBeInTheDocument();
    a.unmount();
    render(Admin, ctx(mk({ role: 'hub' })));
    expect(await screen.findByText('API tokens')).toBeInTheDocument();
    expect(screen.getByText('kw_ci_a1b2...')).toBeInTheDocument();
  });

  it('Models separates dimensions from measured quality and shows only secret references', async () => {
    const { container } = render(Models, ctx(mk()));
    expect((await screen.findAllByText('not measured')).length).toBeGreaterThan(0);
    expect(screen.getAllByText('env:GEMINI_API_KEY').length).toBeGreaterThan(0);
    expect(container.textContent).not.toMatch(/AIza/);
  });

  it('Models previews cost before switching', async () => {
    render(Models, ctx(mk()));
    const buttons = await screen.findAllByRole('button', { name: 'Preview switch' });
    await fireEvent.click(buttons[0] as HTMLElement);
    expect(await screen.findByText(/Estimated cost/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Start blue-green switch' })).toBeInTheDocument();
  });

  it('Indexes shows the three commits separately and queues a reindex', async () => {
    render(Indexes, ctx(mk()));
    expect((await screen.findAllByText('Last seen commit')).length).toBeGreaterThan(0);
    expect(screen.getAllByText('Active index commit').length).toBeGreaterThan(0);
    expect(screen.getAllByText('Track target').length).toBeGreaterThan(0);
    await fireEvent.click(
      screen.getAllByRole('button', { name: 'Reindex changed' })[0] as HTMLElement
    );
    expect(await screen.findByText(/Queued incremental reindex/)).toBeInTheDocument();
  });

  it('Search runs a query and shows the score breakdown with "did not run" signals', async () => {
    render(Search, ctx(mk()));
    const input = await screen.findByRole('searchbox');
    await fireEvent.input(input, { target: { value: 'PlaceOrder' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    expect((await screen.findAllByText('Why this result')).length).toBeGreaterThan(0);
    expect(screen.getAllByText('did not run').length).toBeGreaterThan(0);
  });

  it('Search shows an unindexed project as skipped', async () => {
    render(Search, ctx(mk()));
    const input = await screen.findByRole('searchbox');
    await fireEvent.input(input, { target: { value: 'terraform' } });
    await fireEvent.click(await screen.findByLabelText(/infra/));
    await fireEvent.click(screen.getByRole('button', { name: 'Search' }));
    expect(await screen.findByText(/project not indexed yet/)).toBeInTheDocument();
  });

  it('Graph renders the service map with an accessible edge list', async () => {
    render(Graph, ctx(mk()));
    expect(
      await screen.findByRole('button', { name: 'service orders-service' })
    ).toBeInTheDocument();
    expect(screen.getByText('Edges')).toBeInTheDocument();
    expect(screen.getByText(/sigma\.js/)).toBeInTheDocument();
  });
});
