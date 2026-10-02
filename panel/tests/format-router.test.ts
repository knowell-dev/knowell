import { describe, expect, it } from 'vitest';
import { formatBytes, formatDuration, relativeTime, shortSha } from '$lib/format';
import { parseHash } from '$lib/router.svelte';

describe('format', () => {
  it('formats sizes and durations', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(1536)).toBe('1.5 KiB');
    expect(formatDuration(450)).toBe('450 ms');
    expect(formatDuration(90_000)).toBe('2 min');
  });
  it('handles missing and invalid times', () => {
    expect(relativeTime(undefined)).toBe('never');
    expect(relativeTime('not a date')).toBe('-');
    expect(shortSha(undefined)).toBe('-');
  });
});

describe('router', () => {
  it('parses paths and query strings', () => {
    const r = parseHash('#/projects?id=p-web');
    expect(r.path).toBe('/projects');
    expect(r.query.get('id')).toBe('p-web');
    expect(parseHash('').path).toBe('/');
  });
  it('survives malformed percent-encoding', () => {
    expect(parseHash('#/%E0%A4%A').path).toContain('%E0');
  });
});
