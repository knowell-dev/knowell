import type { GraphEdge, GraphNode } from './api';

export interface Placed {
  node: GraphNode;
  x: number;
  y: number;
  layer: number;
}

export interface LayoutResult {
  placed: Map<string, Placed>;
  width: number;
  height: number;
}

export const NODE_W = 150;
export const NODE_H = 34;

/**
 * Deterministic layered layout for small graphs (the lightweight stand-in until the WebGL
 * renderer lands). Layer = longest path from a source, relaxed a bounded number of times so
 * cycles cannot loop. Nodes inside a layer keep a stable order (kind, then label).
 */
export function layoutGraph(nodes: GraphNode[], edges: GraphEdge[], minWidth = 720): LayoutResult {
  const ids = new Set(nodes.map((n) => n.id));
  const layer = new Map<string, number>(nodes.map((n) => [n.id, 0]));
  const usable = edges.filter((e) => ids.has(e.from) && ids.has(e.to) && e.from !== e.to);
  for (let i = 0; i < nodes.length; i++) {
    let changed = false;
    for (const e of usable) {
      const next = (layer.get(e.from) ?? 0) + 1;
      if (next < nodes.length && next > (layer.get(e.to) ?? 0)) {
        layer.set(e.to, next);
        changed = true;
      }
    }
    if (!changed) break;
  }

  const byLayer = new Map<number, GraphNode[]>();
  for (const n of nodes) {
    const l = layer.get(n.id) ?? 0;
    byLayer.set(l, [...(byLayer.get(l) ?? []), n]);
  }
  const layers = [...byLayer.keys()].sort((a, b) => a - b);
  const colGap = 80;
  const rowGap = 18;
  const pad = 20;
  const maxRows = Math.max(1, ...[...byLayer.values()].map((v) => v.length));
  const height = pad * 2 + maxRows * NODE_H + (maxRows - 1) * rowGap;
  const placed = new Map<string, Placed>();
  layers.forEach((l, col) => {
    const list = [...(byLayer.get(l) ?? [])].sort(
      (a, b) => a.kind.localeCompare(b.kind) || a.label.localeCompare(b.label)
    );
    const colHeight = list.length * NODE_H + (list.length - 1) * rowGap;
    const top = pad + (height - pad * 2 - colHeight) / 2;
    list.forEach((node, row) => {
      placed.set(node.id, {
        node,
        layer: l,
        x: pad + col * (NODE_W + colGap),
        y: top + row * (NODE_H + rowGap)
      });
    });
  });
  const width = Math.max(
    minWidth,
    pad * 2 + layers.length * NODE_W + Math.max(0, layers.length - 1) * colGap
  );
  return { placed, width, height };
}
