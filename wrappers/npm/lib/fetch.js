"use strict";

const fs = require('node:fs');
const http = require('node:http');
const https = require('node:https');

const MAX_REDIRECTS = 5;
const MAX_BINARY_BYTES = 512 * 1024 * 1024;

function allowed(url) {
  if (url.username || url.password || url.hash) return false;
  if (url.protocol === 'https:') return true;
  return url.protocol === 'http:' && ['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname);
}

function redirectAllowed(previous, next) {
  if (!allowed(next) || (previous.protocol === 'https:' && next.protocol !== 'https:')) return false;
  if (next.origin === previous.origin) return true;
  return previous.hostname === 'github.com' && next.protocol === 'https:' &&
    (next.hostname === 'release-assets.githubusercontent.com' || next.hostname === 'objects.githubusercontent.com');
}

/** Download bounded bytes with exclusive creation; diagnostics never echo URL credentials. */
function download(href, dest, options = {}, redirects = 0) {
  const maxBytes = options.maxBytes === undefined ? MAX_BINARY_BYTES : options.maxBytes;
  if (!Number.isSafeInteger(maxBytes) || maxBytes <= 0 || maxBytes > MAX_BINARY_BYTES) {
    return Promise.reject(new Error('invalid download byte limit'));
  }
  return new Promise((resolve, reject) => {
    let url;
    try { url = new URL(href); } catch {
      reject(new Error('invalid download URL'));
      return;
    }
    if (!allowed(url)) {
      reject(new Error('refusing an insecure download URL'));
      return;
    }
    let settled = false;
    let out;
    let timer;
    const finish = (error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      if (error) {
        if (out) out.destroy();
        reject(error);
      } else resolve();
    };
    const client = url.protocol === 'https:' ? https : http;
    const request = client.get(url, { headers: { 'User-Agent': 'knowell-npm-launcher', 'Accept-Encoding': 'identity' } }, (response) => {
      const status = response.statusCode || 0;
      if (status >= 300 && status < 400 && response.headers.location) {
        response.resume();
        let next;
        try { next = new URL(response.headers.location, url); } catch {
          finish(new Error('invalid download redirect'));
          return;
        }
        if (redirects >= MAX_REDIRECTS || !redirectAllowed(url, next)) {
          finish(new Error('refusing download redirect'));
          return;
        }
        clearTimeout(timer);
        download(next.toString(), dest, options, redirects + 1).then(() => finish(), finish);
        return;
      }
      if (status !== 200) {
        response.resume();
        finish(new Error(`download failed: HTTP ${status}`));
        return;
      }
      const length = response.headers['content-length'];
      if ((length !== undefined && (!/^[0-9]+$/.test(length) || Number(length) > maxBytes)) ||
          (response.headers['content-encoding'] && response.headers['content-encoding'] !== 'identity')) {
        finish(new Error('download exceeds the byte limit or uses unsupported encoding'));
        response.destroy();
        return;
      }
      let received = 0;
      out = fs.createWriteStream(dest, { flags: 'wx', mode: 0o600 });
      response.on('data', (chunk) => {
        received += chunk.length;
        if (received > maxBytes) {
          finish(new Error('download exceeds the byte limit'));
          response.destroy();
          request.destroy();
        }
      });
      response.on('aborted', () => finish(new Error('download was truncated')));
      response.on('error', () => finish(new Error('download response failed')));
      out.on('error', () => finish(new Error('could not write the download')));
      out.on('finish', () => out.close((error) => finish(error ? new Error('could not close the download') : undefined)));
      response.pipe(out);
    });
    timer = setTimeout(() => {
      request.destroy();
      finish(new Error('download total timeout exceeded'));
    }, 300000);
    request.setTimeout(60000, () => {
      request.destroy();
      finish(new Error('download timed out'));
    });
    request.on('error', () => finish(new Error('download request failed')));
  });
}

module.exports = { download, MAX_BINARY_BYTES };
