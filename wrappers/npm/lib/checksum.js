'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');

const DIGEST = /^[0-9a-f]{64}$/;

/** Parse `sha256sum` output into a Map of file name to lowercase hex digest. */
function parseSums(text) {
  const sums = new Map();
  text.split(/\r?\n/).forEach((raw, index) => {
    const line = raw.trim();
    if (!line) return;
    const match = /^(\S+)\s+\*?(.+)$/.exec(line);
    if (!match) throw new Error(`SHA256SUMS line ${index + 1} is malformed`);
    const digest = match[1].toLowerCase();
    if (!DIGEST.test(digest)) throw new Error(`SHA256SUMS line ${index + 1} has an invalid digest`);
    const name = match[2].trim();
    if (sums.has(name) && sums.get(name) !== digest) {
      throw new Error(`SHA256SUMS lists ${name} twice with different digests`);
    }
    sums.set(name, digest);
  });
  return sums;
}

function sha256File(file) {
  return new Promise((resolve, reject) => {
    const hash = crypto.createHash('sha256');
    fs.createReadStream(file)
      .on('error', reject)
      .on('data', (chunk) => hash.update(chunk))
      .on('end', () => resolve(hash.digest('hex')));
  });
}

/** Throw unless `file` matches the digest `sums` lists for `name`. */
async function verifyFile(file, name, sums) {
  const expected = sums.get(name);
  if (!expected) throw new Error(`SHA256SUMS has no entry for ${name}`);
  const actual = await sha256File(file);
  if (actual !== expected) {
    throw new Error(`checksum mismatch for ${name}: expected ${expected}, got ${actual}`);
  }
}

module.exports = { parseSums, sha256File, verifyFile };
