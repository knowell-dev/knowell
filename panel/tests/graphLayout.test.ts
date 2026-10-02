import { describe, expect, it } from 'vitest';
import { layoutGraph } from '$lib/graphLayout';
import type { GraphEdge, GraphNode } from '$lib/api';

const node = (id: string): GraphNode => ({ id, kind: 'service', label: id });
const edge = (id: string, from: string, to: string): GraphEdge => ({
  id,
  from,
  to,
  kind: 'calls',
  evidence: 'syntactic',
  status: 'resolved'
});

describe('layoutGraph', () => {
  it('layers nodes along edge direction', () => {
    const r = layoutGraph(
      [node('a'), node('b'), node('c')],
      [edge('1', 'a', 'b'), edge('2', 'b', 'c')]
    );
    expect(r.placed.get('a')?.layer).toBe(0);
    expect(r.placed.get('c')?.layer).toBe(2);
    expect((r.placed.get('c')?.x ?? 0) > (r.placed.get('a')?.x ?? 0)).toBe(true);
  });

  it('terminates on cycles and ignores dangling edges', () => {
    const r = layoutGraph(
      [node('a'), node('b')],
      [edge('1', 'a', 'b'), edge('2', 'b', 'a'), edge('3', 'a', 'zzz')]
    );
    expect(r.placed.size).toBe(2);
  });

  it('is deterministic and handles an empty graph', () => {
    const nodes = [node('b'), node('a')];
    expect(layoutGraph(nodes, [])).toEqual(layoutGraph([...nodes].reverse(), []));
    expect(layoutGraph([], []).placed.size).toBe(0);
  });
});
