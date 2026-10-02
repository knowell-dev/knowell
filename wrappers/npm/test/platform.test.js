'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');

const { resolveTarget, assetNames } = require('../lib/platform');

test('maps every supported platform to its release target', () => {
  const cases = [
    ['linux', 'x64', true, 'x86_64-unknown-linux-gnu', 'tar.gz', 'know'],
    ['linux', 'arm64', true, 'aarch64-unknown-linux-gnu', 'tar.gz', 'know'],
    ['linux', 'x64', false, 'x86_64-unknown-linux-musl', 'tar.gz', 'know'],
    ['linux', 'arm64', false, 'aarch64-unknown-linux-musl', 'tar.gz', 'know'],
    ['darwin', 'x64', true, 'x86_64-apple-darwin', 'tar.gz', 'know'],
    ['darwin', 'arm64', true, 'aarch64-apple-darwin', 'tar.gz', 'know'],
    ['win32', 'x64', true, 'x86_64-pc-windows-msvc', 'zip', 'know.exe'],
    ['win32', 'arm64', true, 'aarch64-pc-windows-msvc', 'zip', 'know.exe'],
  ];
  for (const [platform, arch, glibc, target, ext, exe] of cases) {
    assert.deepEqual(resolveTarget(platform, arch, () => glibc), { target, ext, exe }, `${platform}/${arch}/${glibc}`);
  }
});

test('rejects unsupported systems with a clear message', () => {
  assert.throws(() => resolveTarget('freebsd', 'x64'), /unsupported operating system: freebsd/);
  assert.throws(() => resolveTarget('linux', 'ia32'), /unsupported CPU architecture: ia32/);
  assert.throws(() => resolveTarget('linux', 'riscv64'), /unsupported CPU architecture/);
});

test('asset names match the release naming used by the install scripts', () => {
  const info = resolveTarget('win32', 'x64');
  assert.deepEqual(assetNames('1.2.3', info), {
    dir: 'knowell-1.2.3-x86_64-pc-windows-msvc',
    archive: 'knowell-1.2.3-x86_64-pc-windows-msvc.zip',
  });
  assert.equal(
    assetNames('1.0.0-rc.1', resolveTarget('darwin', 'arm64')).archive,
    'knowell-1.0.0-rc.1-aarch64-apple-darwin.tar.gz',
  );
});
