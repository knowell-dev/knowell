import { describe, expect, it } from 'vitest';
import {
  formatBytes,
  formatDuration,
  relativeTime,
  shortSha,
  shortUserId,
  splitInlineCode
} from '$lib/format';
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
  it('splits backtick commands out of prose', () => {
    expect(splitInlineCode('run `know init` now')).toEqual([
      { text: 'run ', code: false },
      { text: 'know init', code: true },
      { text: ' now', code: false }
    ]);
    expect(splitInlineCode('`a``b`')).toEqual([
      { text: 'a', code: true },
      { text: 'b', code: true }
    ]);
    expect(splitInlineCode('empty `` pair')).toEqual([
      { text: 'empty ', code: false },
      { text: ' pair', code: false }
    ]);
    expect(splitInlineCode('one ` stray')).toEqual([{ text: 'one ` stray', code: false }]);
    expect(splitInlineCode('')).toEqual([]);
  });
  it('shortens user ids for the top bar', () => {
    expect(shortUserId('user:018f2b7e-6c1a-7000-8000-000000000001')).toBe('user:018f2b7e…');
    expect(shortUserId('user:local')).toBe('user:local');
    expect(shortUserId('018f2b7e-6c1a-7000-8000-000000000001')).toBe('018f2b7e…');
    expect(shortUserId('')).toBe('');
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
