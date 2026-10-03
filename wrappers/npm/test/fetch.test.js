'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { download } = require('../lib/fetch');

async function served(handler, run) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'knowell-download-'));
  const server = http.createServer(handler);
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  try { await run(`http://127.0.0.1:${server.address().port}`, path.join(root, 'download')); }
  finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
    fs.rmSync(root, { recursive: true, force: true });
  }
}

test('rejects oversized content length and chunked response bodies', async () => {
  await served((req, res) => res.writeHead(200, { 'Content-Length': '1000' }).end('too large'),
    async (base, dest) => assert.rejects(download(base, dest, { maxBytes: 4 }), /byte limit/));
  await served((req, res) => { res.writeHead(200); res.write('123'); res.end('456'); },
    async (base, dest) => assert.rejects(download(base, dest, { maxBytes: 4 }), /byte limit/));
});

test('rejects truncated bodies and redirects outside the approved origin', async () => {
  await served((req, res) => {
    res.writeHead(200, { 'Content-Length': '20' });
    res.write('123');
    setImmediate(() => res.destroy());
  }, async (base, dest) => assert.rejects(download(base, dest, { maxBytes: 30 }), /truncated|failed/));
  await served((req, res) => res.writeHead(302, { Location: 'http://example.invalid/private' }).end(),
    async (base, dest) => assert.rejects(download(base, dest), /redirect/));
});

test('does not overwrite an existing file and does not echo URL credentials', async () => {
  await served((req, res) => res.end('hello'), async (base, dest) => {
    fs.writeFileSync(dest, 'KNOWELL_CANARY_EXISTING');
    await assert.rejects(download(base, dest), /write/);
    assert.equal(fs.readFileSync(dest, 'utf8'), 'KNOWELL_CANARY_EXISTING');
    await assert.rejects(download('https://KNOWELL_CANARY_USER:KNOWELL_CANARY_PASSWORD@example.invalid', dest),
      (error) => !error.message.includes('KNOWELL_CANARY') && /insecure/.test(error.message));
  });
});
