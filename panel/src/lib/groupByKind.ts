/** Items that share a `kind`, in their original order. */
export interface KindGroup<T> {
  kind: string;
  items: T[];
}

/**
 * Groups items by `kind`: largest group first, ties by kind name, so the order is stable for
 * the same input. Items keep their original order inside a group.
 */
export function groupByKind<T extends { kind: string }>(list: readonly T[]): KindGroup<T>[] {
  const groups = new Map<string, T[]>();
  for (const item of list) {
    const items = groups.get(item.kind);
    if (items) items.push(item);
    else groups.set(item.kind, [item]);
  }
  return [...groups]
    .map(([kind, items]) => ({ kind, items }))
    .sort((a, b) => b.items.length - a.items.length || a.kind.localeCompare(b.kind));
}
