"use strict";

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const { assetNames, resolveTarget } = require('./platform');
const { parseSums, sha256File, verifyFile } = require('./checksum');
const { download, MAX_BINARY_BYTES } = require('./fetch');

const DEFAULT_BASE = 'https://github.com/knowell-dev/knowell/releases/download';
const VERSION_RE = /^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$/;
const DIGEST = /^[0-9a-f]{64}$/;
const RECEIPT_KEYS = ['format_version', 'owner', 'sha256', 'size', 'target', 'version'];

/** Per-user cache root; KNOWELL_CACHE_DIR overrides it. */
function cacheRoot(env = process.env, platform = process.platform, home = os.homedir()) {
  if (env.KNOWELL_CACHE_DIR) return env.KNOWELL_CACHE_DIR;
  if (platform === 'win32') return path.join(env.LOCALAPPDATA || path.join(home, 'AppData', 'Local'), 'knowell', 'npm');
  if (platform === 'darwin') return path.join(home, 'Library', 'Caches', 'knowell');
  return path.join(env.XDG_CACHE_HOME || path.join(home, '.cache'), 'knowell');
}

function regularFile(file, maxBytes) {
  const info = fs.lstatSync(file);
  if (!info.isFile() || info.isSymbolicLink() || info.nlink !== 1 || info.size <= 0 || info.size > maxBytes) {
    throw new Error('cache file is linked, empty or outside its byte limit');
  }
  return info;
}

function directory(dir, create = false) {
  if (create) fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  const info = fs.lstatSync(dir);
  if (!info.isDirectory() || info.isSymbolicLink()) throw new Error('cache directory must not be linked');
  if (process.platform !== 'win32' && (info.mode & 0o022) !== 0) throw new Error('cache directory must not be writable by other users');
}

function protectWindowsRoot(root) {
  if (process.platform !== 'win32') return;
  const system = path.join(process.env.SystemRoot || 'C:\\Windows', 'System32');
  // Replace the DACL instead of only disabling inheritance: an older cache could
  // have explicit grants to other users. The path is data, never PowerShell code.
  const script = `$ErrorActionPreference='Stop';
    $identity=[Security.Principal.WindowsIdentity]::GetCurrent();
    $acl=New-Object Security.AccessControl.DirectorySecurity;
    $acl.SetAccessRuleProtection($true,$false);$acl.SetOwner($identity.User);
    foreach($sid in @($identity.User,(New-Object Security.Principal.SecurityIdentifier('S-1-5-18')),(New-Object Security.Principal.SecurityIdentifier('S-1-5-32-544')))) {
      $rule=New-Object Security.AccessControl.FileSystemAccessRule($sid,'FullControl','ContainerInherit, ObjectInherit','None','Allow');$acl.AddAccessRule($rule)
    };[IO.Directory]::SetAccessControl($env:KNOWELL_NPM_ACL_ROOT,$acl)`;
  const result = spawnSync(path.join(system, 'WindowsPowerShell', 'v1.0', 'powershell.exe'),
    ['-NoProfile', '-NonInteractive', '-Command', script], {
    windowsHide: true, stdio: 'ignore', timeout: 10000,
    env: { ...process.env, KNOWELL_NPM_ACL_ROOT: root },
  });
  if (result.status !== 0) throw new Error('could not protect the npm cache directory ACL');
}

/** Verify the owner, exact version and digest on every cache hit, including a concurrent winner. */
async function validateCache(finalDir, version, info) {
  directory(finalDir);
  const receiptPath = path.join(finalDir, 'receipt.json');
  regularFile(receiptPath, 4096);
  let receipt;
  try {
    const text = fs.readFileSync(receiptPath, 'utf8');
    receipt = JSON.parse(text);
    if (text !== `${JSON.stringify(receipt)}\n`) throw new Error('noncanonical receipt');
  } catch {
    throw new Error('cached install receipt is malformed; remove this version cache and retry');
  }
  if (!receipt || typeof receipt !== 'object' || Array.isArray(receipt) ||
      JSON.stringify(Object.keys(receipt).sort()) !== JSON.stringify(RECEIPT_KEYS) ||
      receipt.format_version !== 1 || receipt.owner !== 'npm' || receipt.version !== version ||
      receipt.target !== info.target || !DIGEST.test(receipt.sha256) ||
      !Number.isSafeInteger(receipt.size) || receipt.size <= 0 || receipt.size > MAX_BINARY_BYTES) {
    throw new Error('cached install receipt does not match the requested npm binary');
  }
  const exe = path.join(finalDir, info.exe);
  const before = regularFile(exe, MAX_BINARY_BYTES);
  if (before.size !== receipt.size || await sha256File(exe) !== receipt.sha256) {
    throw new Error('cached binary checksum mismatch; remove this version cache and retry');
  }
  const after = regularFile(exe, MAX_BINARY_BYTES);
  if (before.size !== after.size || before.mtimeMs !== after.mtimeMs || before.ino !== after.ino) {
    throw new Error('cached binary changed during verification');
  }
  return exe;
}

function rawAsset(sums, basename) {
  const matches = [...sums].filter(([name, digest]) => name === `${digest}.${basename}`);
  if (matches.length !== 1) throw new Error('SHA256SUMS must list exactly one digest-prefixed runtime asset');
  return { name: matches[0][0], digest: matches[0][1] };
}

/**
 * Download only the package-pinned raw runtime, then publish a private immutable cache.
 * Same-origin checksums are bootstrap integrity, not TUF authentication. The runtime
 * remains owned by npm and is never independently upgraded by `know update`.
 */
async function ensureBinary(options) {
  const { version, platform = process.platform, arch = process.arch, env = process.env,
    log = (message) => process.stderr.write(`${message}\n`) } = options;
  if (typeof version !== 'string' || version.length > 128 || !VERSION_RE.test(version) ||
      version.split('-').slice(1).join('-').split('.').some((part) => /^0[0-9]+$/.test(part)) ||
      version.split('-')[0].split('.').some((part) => BigInt(part) > 18446744073709551615n)) {
    throw new Error('invalid version');
  }
  const info = resolveTarget(platform, arch, options.glibc);
  const names = assetNames(version, info);
  const root = cacheRoot(env, platform);
  directory(root, true);
  protectWindowsRoot(root);
  const versionDir = path.join(root, version);
  directory(versionDir, true);
  const finalDir = path.join(versionDir, info.target);
  try {
    fs.lstatSync(finalDir);
    return await validateCache(finalDir, version, info);
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
    if (fs.existsSync(finalDir)) throw new Error('cached installation is incomplete; remove this version cache and retry');
  }
  const base = `${(env.KNOWELL_DOWNLOAD_BASE || DEFAULT_BASE).replace(/\/+$/, '')}/v${version}`;
  const work = fs.mkdtempSync(path.join(versionDir, `.${info.target}-`));
  if (process.platform !== 'win32') fs.chmodSync(work, 0o700);
  try {
    log(`knowell: downloading the pinned runtime ${version} for ${info.target} ...`);
    const sumsFile = path.join(work, 'SHA256SUMS');
    await download(`${base}/SHA256SUMS`, sumsFile, { maxBytes: 1024 * 1024 });
    const sums = parseSums(fs.readFileSync(sumsFile, 'utf8'));
    const raw = rawAsset(sums, names.engine);
    const exe = path.join(work, info.exe);
    await download(`${base}/${raw.name}`, exe);
    await verifyFile(exe, raw.name, sums);
    const size = regularFile(exe, MAX_BINARY_BYTES).size;
    if (process.platform !== 'win32') fs.chmodSync(exe, 0o700);
    const staged = path.join(work, 'final');
    fs.mkdirSync(staged, { mode: 0o700 });
    fs.renameSync(exe, path.join(staged, info.exe));
    const receipt = { format_version: 1, owner: 'npm', version, target: info.target, sha256: raw.digest, size };
    fs.writeFileSync(path.join(staged, 'receipt.json'), `${JSON.stringify(receipt)}\n`, { flag: 'wx', mode: 0o600 });
    try { fs.renameSync(staged, finalDir); } catch (error) {
      if (!fs.existsSync(finalDir)) throw error;
    }
    return await validateCache(finalDir, version, info);
  } finally {
    fs.rmSync(work, { recursive: true, force: true });
  }
}

module.exports = { ensureBinary, cacheRoot, VERSION_RE, validateCache, rawAsset };
