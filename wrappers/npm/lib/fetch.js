'use strict';

const fs = require('node:fs');
const http = require('node:http');
const https = require('node:https');

const MAX_REDIRECTS = 5;

function allowed(url) {
  if (url.protocol === 'https:') return true;
  // Plain http only for a loopback test server, never for a real download.
  return url.protocol === 'http:' && ['127.0.0.1', 'localhost', '[::1]'].includes(url.hostname);
}

/** Download `href` to `dest`. Follows a few redirects (GitHub serves assets via a CDN). */
function download(href, dest, redirects = 0) {
  return new Promise((resolve, reject) => {
    let url;
    try {
      url = new URL(href);
    } catch {
      reject(new Error(`invalid URL: ${href}`));
      return;
    }
    if (!allowed(url)) {
      reject(new Error(`refusing to download over ${url.protocol}`));
      return;
    }
    const client = url.protocol === 'https:' ? https : http;
    const request = client.get(url, { headers: { 'User-Agent': 'knowell-npm-launcher' } }, (response) => {
      const status = response.statusCode || 0;
      if (status >= 300 && status < 400 && response.headers.location) {
        response.resume();
        if (redirects >= MAX_REDIRECTS) {
          reject(new Error('too many redirects'));
          return;
        }
        const next = new URL(response.headers.location, url).toString();
        download(next, dest, redirects + 1).then(resolve, reject);
        return;
      }
      if (status !== 200) {
        response.resume();
        reject(new Error(`download failed: HTTP ${status} for ${href}`));
        return;
      }
      const out = fs.createWriteStream(dest);
      response.pipe(out);
      out.on('finish', () => out.close(resolve));
      out.on('error', reject);
      response.on('error', reject);
    });
    request.setTimeout(60000, () => request.destroy(new Error(`timed out downloading ${href}`)));
    request.on('error', reject);
  });
}

module.exports = { download };
