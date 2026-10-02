'use strict';

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const { assetNames, resolveTarget } = require('./platform');
const { parseSums, verifyFile } = require('./checksum');
const { download } = require('./fetch');

const DEFAULT_BASE = 'https://github.com/knowell-dev/knowell/releases/download';
const VERSION_RE = /^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/;

/** Per-user cache root; KNOWELL_CACHE_DIR overrides it. */
function cacheRoot(env = process.env, platform = process.platform, home = os.homedir()) {
  if (env.KNOWELL_CACHE_DIR) return env.KNOWELL_CACHE_DIR;
  if (platform === 'win32') return path.join(env.LOCALAPPDATA || path.join(home, 'AppData', 'Local'), 'knowell', 'npm');
  if (platform === 'darwin') return path.join(home, 'Library', 'Caches', 'knowell');
  return path.join(env.XDG_CACHE_HOME || path.join(home, '.cache'), 'knowell');
}

function extract(workDir, archiveName, destName, platform = process.platform) {
  // Windows 10+ ships bsdtar, which also reads .zip; Git's GNU tar on PATH does not.
  const tar = platform === 'win32' ? path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'tar.exe') : 'tar';
  // Relative names plus a cwd: GNU tar reads "C:" in an absolute Windows path as a host.
  const result = spawnSync(tar, ['-xf', archiveName, '-C', destName], { cwd: workDir, stdio: ['ignore', 'ignore', 'pipe'], encoding: 'utf8' });
  if (result.error) throw new Error(`could not run ${tar} to unpack the download: ${result.error.message}`);
  if (result.status !== 0) throw new Error(`unpacking the download failed: ${(result.stderr || '').trim()}`);
}

/**
 * Make sure the `know` binary for `version` is in the cache and return its path.
 * Downloads the archive and SHA256SUMS, verifies the checksum, unpacks, then moves the
 * result into place atomically. Messages go to `log` (stderr): stdout belongs to MCP.
 */
async function ensureBinary(options) {
  const {
    version,
    platform = process.platform,
    arch = process.arch,
    env = process.env,
    log = (message) => process.stderr.write(`${message}\n`),
  } = options;
  if (!VERSION_RE.test(version)) throw new Error(`invalid version: ${version}`);

  const info = resolveTarget(platform, arch, options.glibc);
  const names = assetNames(version, info);
  const finalDir = path.join(cacheRoot(env, platform), version, info.target);
  const finalExe = path.join(finalDir, info.exe);
  if (fs.existsSync(finalExe)) return finalExe;

  const base = `${(env.KNOWELL_DOWNLOAD_BASE || DEFAULT_BASE).replace(/\/+$/, '')}/v${version}`;
  fs.mkdirSync(path.dirname(finalDir), { recursive: true });
  const work = fs.mkdtempSync(path.join(path.dirname(finalDir), `.${info.target}-`));
  try {
    log(`knowell: downloading ${names.archive} (first run only) ...`);
    const archive = path.join(work, names.archive);
    const sumsFile = path.join(work, 'SHA256SUMS');
    await download(`${base}/${names.archive}`, archive);
    await download(`${base}/SHA256SUMS`, sumsFile);
    await verifyFile(archive, names.archive, parseSums(fs.readFileSync(sumsFile, 'utf8')));

    const unpacked = path.join(work, 'x');
    fs.mkdirSync(unpacked);
    extract(work, names.archive, 'x', platform);
    const exe = path.join(unpacked, names.dir, info.exe);
    if (!fs.existsSync(exe)) throw new Error(`the archive does not contain ${names.dir}/${info.exe}`);
    if (platform !== 'win32') fs.chmodSync(exe, 0o755);

    const staged = path.join(work, 'final');
    fs.mkdirSync(staged);
    fs.renameSync(exe, path.join(staged, info.exe));
    try {
      fs.renameSync(staged, finalDir);
    } catch (error) {
      // A concurrent run won the race: its copy is as good as ours.
      if (!fs.existsSync(finalExe)) throw error;
    }
  } finally {
    fs.rmSync(work, { recursive: true, force: true });
  }
  return finalExe;
}

module.exports = { ensureBinary, cacheRoot, VERSION_RE };
