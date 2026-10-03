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

export const NODE_W = 176;
export const NODE_H = 34;

/** Point on the border of a node box toward another box, so arrows touch the box edge. */
function borderPoint(ax: number, ay: number, bx: number, by: number): [number, number] {
  const cx = ax + NODE_W / 2;
  const cy = ay + NODE_H / 2;
  const dx = bx + NODE_W / 2 - cx;
  const dy = by + NODE_H / 2 - cy;
  if (dx === 0 && dy === 0) return [cx, cy];
  const t = Math.min(NODE_W / 2 / Math.abs(dx || 1e-9), NODE_H / 2 / Math.abs(dy || 1e-9));
  return [cx + dx * t, cy + dy * t];
}

/**
 * SVG path for an edge between two placed boxes (top-left corners, in layout units). Edges that
 * run forward to a later column leave the right side and enter the left side on a smooth curve,
 * which keeps fan-in to one column readable; any other edge is a straight border-to-border line.
 */
export function edgePath(ax: number, ay: number, bx: number, by: number): string {
  if (bx >= ax + NODE_W) {
    const x1 = ax + NODE_W;
    const y1 = ay + NODE_H / 2;
    const y2 = by + NODE_H / 2;
    const mid = (x1 + bx) / 2;
    return `M${x1} ${y1}C${mid} ${y1} ${mid} ${y2} ${bx} ${y2}`;
  }
  const [x1, y1] = borderPoint(ax, ay, bx, by);
  const [x2, y2] = borderPoint(bx, by, ax, ay);
  return `M${x1} ${y1}L${x2} ${y2}`;
}

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
