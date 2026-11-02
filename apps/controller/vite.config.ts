import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  server: { host: '127.0.0.1', port: 5173, proxy: { '/ws': { target: 'ws://127.0.0.1:8787', ws: true } } },
  test: { environment: 'jsdom', setupFiles: './src/test/setup.ts', exclude: ['e2e/**', 'node_modules/**'] },
});
