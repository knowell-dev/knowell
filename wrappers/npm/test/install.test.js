'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const { ensureBinary, cacheRoot } = require('../lib/install');

const VERSION = '9.8.7';
const TARGET_DIR = `knowell-${VERSION}-x86_64-unknown-linux-gnu`;

/** Build a fake release (archive + SHA256SUMS) and serve it from a loopback server. */
async function fakeRelease({ tamper = false } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'knowell-rel-'));
  const served = path.join(root, 'srv', `v${VERSION}`);
  fs.mkdirSync(served, { recursive: true });
  const bytes = '#!/bin/sh\necho fake know\n';
  const digest = tamper ? '0'.repeat(64) : crypto.createHash('sha256').update(bytes).digest('hex');
  const asset = `${digest}.${TARGET_DIR}-engine`;
  fs.writeFileSync(path.join(served, asset), bytes);
  fs.writeFileSync(path.join(served, 'SHA256SUMS'), `${digest}  ${asset}\n`);

  const server = http.createServer((req, res) => {
    const file = path.join(root, 'srv', decodeURIComponent(req.url));
    if (!file.startsWith(path.join(root, 'srv')) || !fs.existsSync(file)) {
      res.writeHead(404).end();
      return;
    }
    res.writeHead(200).end(fs.readFileSync(file));
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const base = `http://127.0.0.1:${server.address().port}`;
  return {
    base,
    cache: path.join(root, 'cache'),
    close: () => {
      server.close();
      fs.rmSync(root, { recursive: true, force: true });
    },
  };
}

const linux = { platform: 'linux', arch: 'x64', glibc: () => true };

test('downloads, verifies and caches the pinned raw binary', async () => {
  const release = await fakeRelease();
  const logs = [];
  try {
    const env = { KNOWELL_DOWNLOAD_BASE: release.base, KNOWELL_CACHE_DIR: release.cache };
    const exe = await ensureBinary({ version: VERSION, env, log: (m) => logs.push(m), ...linux });
    assert.equal(exe, path.join(release.cache, VERSION, 'x86_64-unknown-linux-gnu', 'know'));
    assert.match(fs.readFileSync(exe, 'utf8'), /fake know/);
    assert.equal(logs.length, 1);

    // Second call is served from the cache: no download, so a dead server is fine.
    const again = await ensureBinary({ version: VERSION, env: { ...env, KNOWELL_DOWNLOAD_BASE: 'http://127.0.0.1:1' }, log: (m) => logs.push(m), ...linux });
    assert.equal(again, exe);
    assert.equal(logs.length, 1);
    // No temporary directories are left behind.
    assert.deepEqual(fs.readdirSync(path.join(release.cache, VERSION)), ['x86_64-unknown-linux-gnu']);
  } finally {
    release.close();
  }
});

test('a checksum mismatch installs nothing', async () => {
  const release = await fakeRelease({ tamper: true });
  try {
    const env = { KNOWELL_DOWNLOAD_BASE: release.base, KNOWELL_CACHE_DIR: release.cache };
    await assert.rejects(ensureBinary({ version: VERSION, env, log: () => {}, ...linux }), /checksum mismatch/);
    const dir = path.join(release.cache, VERSION);
    assert.deepEqual(fs.existsSync(dir) ? fs.readdirSync(dir) : [], []);
  } finally {
    release.close();
  }
});

test('a missing release is reported, not retried silently', async () => {
  const release = await fakeRelease();
  try {
    const env = { KNOWELL_DOWNLOAD_BASE: release.base, KNOWELL_CACHE_DIR: release.cache };
    await assert.rejects(ensureBinary({ version: '1.0.0', env, log: () => {}, ...linux }), /HTTP 404/);
  } finally {
    release.close();
  }
});

test('refuses plain http to a non-loopback host and bad versions', async () => {
  const env = { KNOWELL_DOWNLOAD_BASE: 'http://example.invalid', KNOWELL_CACHE_DIR: fs.mkdtempSync(path.join(os.tmpdir(), 'knowell-c-')) };
  try {
    await assert.rejects(ensureBinary({ version: VERSION, env, log: () => {}, ...linux }), /refusing an insecure download URL/);
    for (const version of ['1.0; rm -rf', '1.1.0-rc.01', '18446744073709551616.1.0', '01.1.0', '1.1.0+build']) {
      await assert.rejects(ensureBinary({ version, env, log: () => {}, ...linux }), /invalid version/);
    }
  } finally {
    fs.rmSync(env.KNOWELL_CACHE_DIR, { recursive: true, force: true });
  }
});

test('cache root honours overrides and per-OS conventions', () => {
  assert.equal(cacheRoot({ KNOWELL_CACHE_DIR: '/x' }, 'linux', '/h'), '/x');
  assert.equal(cacheRoot({ XDG_CACHE_HOME: '/xdg' }, 'linux', '/h'), path.join('/xdg', 'knowell'));
  assert.equal(cacheRoot({}, 'linux', '/h'), path.join('/h', '.cache', 'knowell'));
  assert.equal(cacheRoot({}, 'darwin', '/h'), path.join('/h', 'Library', 'Caches', 'knowell'));
  assert.equal(cacheRoot({ LOCALAPPDATA: 'C:\\L' }, 'win32', 'C:\\h'), path.join('C:\\L', 'knowell', 'npm'));
});

test('launcher passes arguments and the exit code through, and keeps stdout clean', () => {
  const launcher = path.join(__dirname, '..', 'bin', 'know.js');
  const result = spawnSync(process.execPath, [launcher, '-e', 'process.stdout.write("out"); process.exit(3)'], {
    env: { ...process.env, KNOWELL_BIN: process.execPath },
    encoding: 'utf8',
  });
  assert.equal(result.status, 3);
  assert.equal(result.stdout, 'out');
  assert.equal(result.stderr, '');
});

test('launcher without a binary or a released version explains itself on stderr', () => {
  const launcher = path.join(__dirname, '..', 'bin', 'know.js');
  const env = { ...process.env };
  delete env.KNOWELL_BIN;
  delete env.KNOWELL_BINARY_VERSION;
  const result = spawnSync(process.execPath, [launcher, '--version'], { env, encoding: 'utf8' });
  assert.equal(result.status, 1);
  assert.equal(result.stdout, '');
  assert.match(result.stderr, /unreleased development copy/);
});

test('tampered cached bytes and malformed receipts never run or redownload silently', async () => {
  const release = await fakeRelease();
  try {
    const env = { KNOWELL_DOWNLOAD_BASE: release.base, KNOWELL_CACHE_DIR: release.cache };
    const exe = await ensureBinary({ version: VERSION, env, log: () => {}, ...linux });
    const original = fs.readFileSync(exe);
    fs.writeFileSync(exe, 'KNOWELL_CANARY_TAMPERED');
    await assert.rejects(ensureBinary({ version: VERSION, env, log: () => {}, ...linux }), /checksum mismatch/);
    fs.writeFileSync(exe, original);
    const receipt = path.join(path.dirname(exe), 'receipt.json');
    fs.writeFileSync(receipt, '{"owner":');
    await assert.rejects(ensureBinary({ version: VERSION, env, log: () => {}, ...linux }), /receipt is malformed/);
  } finally { release.close(); }
});

test('concurrent downloads validate the winning cache receipt', async () => {
  const release = await fakeRelease();
  try {
    const env = { KNOWELL_DOWNLOAD_BASE: release.base, KNOWELL_CACHE_DIR: release.cache };
    const paths = await Promise.all(Array.from({ length: 3 }, () => ensureBinary({ version: VERSION, env, log: () => {}, ...linux })));
    assert.equal(new Set(paths).size, 1);
    assert.match(fs.readFileSync(paths[0], 'utf8'), /fake know/);
  } finally { release.close(); }
});

test('a poisoned concurrent winner is rejected instead of accepted by existence', async () => {
  const release = await fakeRelease();
  try {
    const env = { KNOWELL_DOWNLOAD_BASE: release.base, KNOWELL_CACHE_DIR: release.cache };
    const final = path.join(release.cache, VERSION, 'x86_64-unknown-linux-gnu');
    const log = () => {
      fs.mkdirSync(final, { recursive: true, mode: 0o700 });
      fs.writeFileSync(path.join(final, 'know'), 'KNOWELL_CANARY_POISONED');
      const receipt = { format_version: 1, owner: 'npm', version: VERSION, target: linux.arch === 'x64' ? 'x86_64-unknown-linux-gnu' : '', sha256: '0'.repeat(64), size: 22 };
      fs.writeFileSync(path.join(final, 'receipt.json'), `${JSON.stringify(receipt)}\n`);
    };
    await assert.rejects(ensureBinary({ version: VERSION, env, log, ...linux }), /checksum mismatch/);
  } finally { release.close(); }
});

test('incomplete cache and wrong owner receipt fail closed', async () => {
  const release = await fakeRelease();
  try {
    const env = { KNOWELL_DOWNLOAD_BASE: release.base, KNOWELL_CACHE_DIR: release.cache };
    const exe = await ensureBinary({ version: VERSION, env, log: () => {}, ...linux });
    const receipt = path.join(path.dirname(exe), 'receipt.json');
    const value = JSON.parse(fs.readFileSync(receipt, 'utf8'));
    value.owner = 'direct';
    fs.writeFileSync(receipt, `${JSON.stringify(value)}\n`);
    await assert.rejects(ensureBinary({ version: VERSION, env, log: () => {}, ...linux }), /does not match/);
    fs.unlinkSync(receipt);
    await assert.rejects(ensureBinary({ version: VERSION, env, log: () => {}, ...linux }), /incomplete/);
  } finally { release.close(); }
});
