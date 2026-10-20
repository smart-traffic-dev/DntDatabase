import react from '@vitejs/plugin-react';
import { defineConfig } from 'vitest/config';

// Tauri loads the dev server from `devUrl` (src-tauri/tauri.conf.json): fixed port, no clear.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    host: 'localhost',
    port: 5173,
    strictPort: true,
  },
  build: {
    target: 'es2022',
    sourcemap: false,
  },
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test/setup.ts'],
    // Process CSS so ?raw imports (cssVars.test.ts) return the real source instead of ''.
    css: { include: [/\.css(\?.*)?$/] },
    restoreMocks: true,
  },
});
