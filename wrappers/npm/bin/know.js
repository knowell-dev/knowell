#!/usr/bin/env node
'use strict';

// Launcher for the Knowell `know` binary. It never writes to stdout itself: when used
// as an MCP stdio server (`npx -y knowell mcp`) stdout carries the protocol.
//
// Environment:
//   KNOWELL_BIN               run this binary instead of downloading one
//   KNOWELL_BINARY_VERSION    release to run (default: this package's version)
//   KNOWELL_CACHE_DIR         where downloaded binaries are kept
//   KNOWELL_DOWNLOAD_BASE     alternative release download base URL (testing)

const { spawn } = require('node:child_process');
const path = require('node:path');

const { ensureBinary } = require('../lib/install');

function fail(message) {
  process.stderr.write(`knowell: ${message}\n`);
  process.exit(1);
}

async function main() {
  let binary = process.env.KNOWELL_BIN;
  if (!binary) {
    const version = process.env.KNOWELL_BINARY_VERSION || require(path.join(__dirname, '..', 'package.json')).version;
    if (version === '0.0.0') {
      fail('this is an unreleased development copy of the launcher; set KNOWELL_BIN or KNOWELL_BINARY_VERSION');
    }
    try {
      binary = await ensureBinary({ version });
    } catch (error) {
      fail(error.message);
    }
  }

  const child = spawn(binary, process.argv.slice(2), {
    stdio: 'inherit', windowsHide: true,
    env: { ...process.env, KNOWELL_INSTALL_OWNER: 'npm' },
  });
  for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
    process.on(signal, () => child.kill(signal));
  }
  child.on('error', () => fail('could not start the configured binary'));
  child.on('exit', (code, signal) => {
    if (signal) {
      process.removeAllListeners(signal);
      process.kill(process.pid, signal);
    } else {
      process.exit(code === null ? 1 : code);
    }
  });
}

main().catch((error) => fail(error.message));
