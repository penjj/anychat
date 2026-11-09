import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import { fileURLToPath } from 'node:url'
import { dirname, resolve } from 'node:path'

const here = dirname(fileURLToPath(import.meta.url))

export default defineConfig({
  plugins: [react()],
  root: here,
  build: {
    // Two documents: the rail UI, and the launch animation. The splash is
    // separate because it must paint before the app has any state to render.
    rollupOptions: {
      input: {
        main: resolve(here, 'index.html'),
        splash: resolve(here, 'splash.html'),
      },
    },
    // Tauri serves `frontend/` directly, so the build lands there.
    outDir: resolve(here, '../frontend'),
    emptyOutDir: true,
    // Desktop bundles load from disk, not a network: inlining small assets keeps
    // the request count down.
    assetsInlineLimit: 4096,
  },
  server: { port: 5183, strictPort: true },
})
