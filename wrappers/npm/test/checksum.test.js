'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const { parseSums, sha256File, verifyFile } = require('../lib/checksum');

function tempFile(content) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'knowell-test-'));
  const file = path.join(dir, 'asset.bin');
  fs.writeFileSync(file, content);
  return { file, cleanup: () => fs.rmSync(dir, { recursive: true, force: true }) };
}

const digestOf = (content) => crypto.createHash('sha256').update(content).digest('hex');

test('parses sha256sum output, including the binary marker and CRLF', () => {
  const a = 'a'.repeat(64);
  const b = 'B'.repeat(64);
  const sums = parseSums(`${a}  one.zip\r\n${b} *two.tar.gz\n\n`);
  assert.equal(sums.get('one.zip'), a);
  assert.equal(sums.get('two.tar.gz'), b.toLowerCase());
});

test('rejects malformed lines, bad digests and conflicting duplicates', () => {
  assert.throws(() => parseSums('nodigest'), /malformed/);
  assert.throws(() => parseSums('xyz  file'), /invalid digest/);
  assert.throws(() => parseSums(`${'a'.repeat(64)}  f\n${'b'.repeat(64)}  f`), /twice/);
});

test('verifies a matching file', async () => {
  const { file, cleanup } = tempFile('hello');
  try {
    assert.equal(await sha256File(file), digestOf('hello'));
    await verifyFile(file, 'asset.bin', new Map([['asset.bin', digestOf('hello')]]));
  } finally {
    cleanup();
  }
});

test('rejects a tampered file', async () => {
  const { file, cleanup } = tempFile('tampered');
  try {
    await assert.rejects(
      verifyFile(file, 'asset.bin', new Map([['asset.bin', digestOf('hello')]])),
      /checksum mismatch/,
    );
  } finally {
    cleanup();
  }
});

test('rejects a file missing from SHA256SUMS', async () => {
  const { file, cleanup } = tempFile('hello');
  try {
    await assert.rejects(verifyFile(file, 'asset.bin', new Map()), /no entry for asset.bin/);
  } finally {
    cleanup();
  }
});
