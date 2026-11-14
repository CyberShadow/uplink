import { defineConfig } from 'vitest/config'
import preact from '@preact/preset-vite'

// The dev server's page is not behind uplink's basic auth, so the proxy
// adds the credentials: UPLINK_DEV_AUTH=user:password. It proxies to
// `uplink serve` as `uplink init` configures it, without TLS, or at
// UPLINK_DEV_SERVER (host:port).
const devAuth = process.env.UPLINK_DEV_AUTH
const devServer = process.env.UPLINK_DEV_SERVER ?? '127.0.0.1:8443'
const proxyHeaders = devAuth
  ? { Authorization: `Basic ${Buffer.from(devAuth).toString('base64')}` }
  : undefined

export default defineConfig({
  plugins: [preact()],
  server: {
    port: 5173,
    proxy: {
      '/api': {
        target: `http://${devServer}`,
        changeOrigin: true,
        headers: proxyHeaders,
      },
      // Its host kept: uplink takes a WebSocket only from a page at the
      // host it is asked at.
      '/ws': {
        target: `ws://${devServer}`,
        ws: true,
        headers: proxyHeaders,
      },
    },
  },
  build: {
    outDir: '../crates/uplink-server/static',
    emptyOutDir: true,
  },
  test: {
    environment: 'jsdom',
    setupFiles: ['src/test/setup.ts'],
    // Well past a query's wait (setup.ts), so that one that waits in vain
    // fails as itself, naming what it found not and showing the page.
    testTimeout: 15000,
    // Local times differ from UTC ones, as they do for most users: the
    // calendar shows all-day events by their UTC dates, timed ones at
    // local times.
    env: { TZ: 'America/New_York' },
  },
})
