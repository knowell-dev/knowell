import { describe, expect, it } from 'vitest';
import { edgePath, layoutGraph, NODE_H, NODE_W } from '$lib/graphLayout';
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

  it('curves forward edges side to side and keeps other edges straight', () => {
    const r = layoutGraph([node('a'), node('b')], [edge('1', 'a', 'b')]);
    const a = r.placed.get('a');
    const b = r.placed.get('b');
    expect(a && b).toBeTruthy();
    if (!a || !b) return;
    const forward = edgePath(a.x, a.y, b.x, b.y);
    expect(forward).toMatch(new RegExp(`^M${a.x + NODE_W} ${a.y + NODE_H / 2}C`));
    expect(forward).toMatch(new RegExp(` ${b.x} ${b.y + NODE_H / 2}$`));
    expect(edgePath(b.x, b.y, a.x, a.y)).toMatch(/^M[\d.]+ [\d.]+L[\d.]+ [\d.]+$/);
    expect(edgePath(0, 0, 0, 100)).toBe(`M${NODE_W / 2} ${NODE_H}L${NODE_W / 2} 100`);
  });

  it('is deterministic and handles an empty graph', () => {
    const nodes = [node('b'), node('a')];
    expect(layoutGraph(nodes, [])).toEqual(layoutGraph([...nodes].reverse(), []));
    expect(layoutGraph([], []).placed.size).toBe(0);
  });
});
