import { describe, expect, it } from 'vitest';
import { groupByKind } from '$lib/groupByKind';

const it_ = (id: string, kind: string) => ({ id, kind });

describe('groupByKind', () => {
  it('puts the largest group first and keeps item order inside groups', () => {
    const groups = groupByKind([it_('1', 'b'), it_('2', 'a'), it_('3', 'b'), it_('4', 'c')]);
    expect(groups.map((g) => g.kind)).toEqual(['b', 'a', 'c']);
    expect(groups[0]?.items.map((i) => i.id)).toEqual(['1', '3']);
  });

  it('breaks ties by kind name and handles an empty list', () => {
    expect(groupByKind([it_('1', 'z'), it_('2', 'm')]).map((g) => g.kind)).toEqual(['m', 'z']);
    expect(groupByKind([])).toEqual([]);
  });
});
