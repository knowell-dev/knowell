import { describe, expect, it } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/svelte';
import { tick } from 'svelte';
import { ApiError } from '$lib/api';
import { Resource } from '$lib/resource.svelte';
import DataStateHarness from './DataStateHarness.svelte';
import SecretRef from '$lib/components/SecretRef.svelte';
import CodeBlock from '$lib/components/CodeBlock.svelte';
import Tabs from '$lib/components/Tabs.svelte';
import Meter from '$lib/components/Meter.svelte';

const settle = async () => {
  await Promise.resolve();
  await tick();
  await Promise.resolve();
  await tick();
};

describe('DataState', () => {
  it('shows loading, then data', async () => {
    let done: (v: string[]) => void = () => {};
    const r = new Resource<string[]>(() => new Promise((res) => (done = res)));
    void r.load();
    render(DataStateHarness, { resource: r });
    expect(screen.getByRole('status')).toHaveTextContent(/loading/i);
    done(['alpha', 'beta']);
    await settle();
    expect(screen.getByText('alpha')).toBeInTheDocument();
  });

  it('explains why it is empty', async () => {
    const r = new Resource<string[]>(async () => []);
    await r.load();
    render(DataStateHarness, { resource: r });
    expect(screen.getByText('Nothing indexed')).toBeInTheDocument();
    expect(screen.getByText('project not indexed yet')).toBeInTheDocument();
  });

  it('shows an error with a working retry', async () => {
    let n = 0;
    const r = new Resource<string[]>(async () => {
      n += 1;
      if (n === 1) throw new ApiError('network', 'cannot reach the engine');
      return ['ok-item'];
    });
    await r.load();
    render(DataStateHarness, { resource: r });
    expect(screen.getByRole('alert')).toHaveTextContent('cannot reach the engine');
    await fireEvent.click(screen.getByRole('button', { name: 'Retry' }));
    await settle();
    expect(screen.getByText('ok-item')).toBeInTheDocument();
  });

  it('ignores a stale response that resolves after a newer one', async () => {
    const resolvers: ((v: string[]) => void)[] = [];
    const r = new Resource<string[]>(() => new Promise((res) => resolvers.push(res)));
    const first = r.load();
    const second = r.load();
    resolvers[1]?.(['new']);
    resolvers[0]?.(['old']);
    await Promise.all([first, second]);
    expect(r.data).toEqual(['new']);
  });
});

describe('SecretRef', () => {
  it('renders references and hides anything else', () => {
    const { unmount } = render(SecretRef, { reference: 'env:GEMINI_API_KEY' });
    expect(screen.getByText('env:GEMINI_API_KEY')).toBeInTheDocument();
    unmount();
    render(SecretRef, { reference: 'KNOWELL_CANARY_not_a_reference_value' });
    expect(screen.queryByText(/CANARY/)).not.toBeInTheDocument();
    expect(screen.getByText('(hidden)')).toBeInTheDocument();
  });
});

describe('CodeBlock', () => {
  it('renders repository text as text, never as HTML', () => {
    const { container } = render(CodeBlock, {
      text: '<img src=x onerror="alert(1)"><script>boom()</script>'
    });
    expect(container.querySelector('img')).toBeNull();
    expect(container.querySelector('script')).toBeNull();
    expect(container.querySelector('pre')?.textContent).toContain('<img src=x');
  });
});

describe('Tabs', () => {
  it('supports roving keyboard navigation', async () => {
    render(Tabs, {
      tabs: [
        { id: 'a', label: 'Alpha' },
        { id: 'b', label: 'Beta' }
      ],
      active: 'a',
      label: 'Demo'
    });
    const alpha = screen.getByRole('tab', { name: 'Alpha' });
    expect(alpha).toHaveAttribute('aria-selected', 'true');
    await fireEvent.keyDown(alpha, { key: 'ArrowRight' });
    expect(screen.getByRole('tab', { name: 'Beta' })).toHaveAttribute('aria-selected', 'true');
  });
});

describe('Meter', () => {
  it('exposes an accessible value and clamps', () => {
    render(Meter, { value: 1.7, label: 'Coverage' });
    expect(screen.getByRole('meter', { name: 'Coverage' })).toHaveAttribute('aria-valuenow', '100');
  });
});
