'use strict';

// Maps Node's platform/arch to the Rust target triple used in release asset names.
// Keep in step with PLATFORMS in dist/render.py and the build matrix in release.yml.

/** True when the running Linux uses glibc (false on musl, e.g. Alpine). */
function isGlibc() {
  try {
    const header = process.report.getReport().header;
    return Boolean(header && header.glibcVersionRuntime);
  } catch {
    throw new Error('could not determine the Linux libc; no binary target was substituted');
  }
}

/**
 * @param {string} platform process.platform
 * @param {string} arch process.arch
 * @param {() => boolean} [glibc] reports whether the Linux libc is glibc
 * @returns {{target: string, ext: 'tar.gz' | 'zip', exe: string}}
 */
function resolveTarget(platform, arch, glibc = isGlibc) {
  const cpu = { x64: 'x86_64', arm64: 'aarch64' }[arch];
  if (!cpu) throw new Error(`unsupported CPU architecture: ${arch}`);
  switch (platform) {
    case 'linux':
      return { target: `${cpu}-unknown-linux-${glibc() ? 'gnu' : 'musl'}`, ext: 'tar.gz', exe: 'know' };
    case 'darwin':
      return { target: `${cpu}-apple-darwin`, ext: 'tar.gz', exe: 'know' };
    case 'win32':
      return { target: `${cpu}-pc-windows-msvc`, ext: 'zip', exe: 'know.exe' };
    default:
      throw new Error(`unsupported operating system: ${platform}`);
  }
}

/** Release asset names, identical to the ones scripts/install.sh and dist/render.py use. */
function assetNames(version, info) {
  const base = `knowell-${version}-${info.target}`;
  return { dir: base, archive: `${base}.${info.ext}`, engine: `${base}-engine${info.exe.endsWith('.exe') ? '.exe' : ''}` };
}

module.exports = { resolveTarget, assetNames, isGlibc };
