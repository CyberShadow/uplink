import { defineConfig } from 'vitest/config'
import preact from '@preact/preset-vite'

// The dev server's page is not behind uplink's basic auth, so the proxy
// adds the credentials: UPLINK_DEV_AUTH=user:password.
const devAuth = process.env.UPLINK_DEV_AUTH
const proxyHeaders = devAuth
  ? { Authorization: `Basic ${Buffer.from(devAuth).toString('base64')}` }
  : undefined

export default defineConfig({
  plugins: [preact()],
  server: {
    port: 5173,
    proxy: {
      '/api': {
        target: 'https://127.0.0.1:8443',
        changeOrigin: true,
        secure: false,
        headers: proxyHeaders,
      },
      '/ws': {
        target: 'wss://127.0.0.1:8443',
        ws: true,
        changeOrigin: true,
        secure: false,
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
    // Local times differ from UTC ones, as they do for most users: the
    // calendar shows all-day events by their UTC dates, timed ones at
    // local times.
    env: { TZ: 'America/New_York' },
  },
})
