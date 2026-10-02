import { defineConfig } from 'vitest/config';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { fileURLToPath } from 'node:url';

const lib = fileURLToPath(new URL('./src/lib', import.meta.url));
const API_TARGET = process.env.KNOWELL_API_TARGET ?? 'http://127.0.0.1:7420';
const DEV_ORIGINS = new Set(['http://127.0.0.1:5173', 'http://localhost:5173']);

export default defineConfig({
  plugins: [svelte()],
  // Relative base: the Rust server may mount the panel under any prefix.
  base: './',
  resolve: {
    alias: { $lib: lib },
    // Svelte must resolve to its browser build under jsdom for component tests.
    ...(process.env.VITEST ? { conditions: ['browser'] } : {})
  },
  server: {
    host: '127.0.0.1',
    port: 5173,
    // Against a real engine: `VITE_API=http npm run dev`, with `know serve` on 127.0.0.1:7420.
    // The server checks Host (DNS-rebinding defence) and Origin (CSRF defence) on every request.
    // `changeOrigin` rewrites Host to the server's own host, so no extra_allowed_hosts entry is
    // needed. Origin is rewritten only when it is this dev server's own origin; any other
    // Origin is forwarded untouched and the server rejects it, so the check stays meaningful.
    // The dev server listens on 127.0.0.1 only. See README.md.
    proxy: {
      '/api': {
        target: API_TARGET,
        changeOrigin: true,
        configure: (proxy) => {
          proxy.on('proxyReq', (proxyReq, req) => {
            const origin = req.headers.origin;
            if (typeof origin === 'string' && DEV_ORIGINS.has(origin.toLowerCase())) {
              proxyReq.setHeader('Origin', API_TARGET);
            }
          });
        }
      }
    }
  },
  build: { outDir: 'dist', emptyOutDir: true, sourcemap: false, target: 'es2022' },
  test: {
    environment: 'jsdom',
    include: ['tests/**/*.test.ts', 'src/**/*.test.ts'],
    setupFiles: ['tests/setup.ts'],
    globals: false
  }
});
