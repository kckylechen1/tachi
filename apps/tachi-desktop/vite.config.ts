import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5111,
    proxy: {
      '/tachi': {
        target: 'http://127.0.0.1:8099',
        changeOrigin: true,
        rewrite: (path) => path.replace(/^\/tachi/, ''),
      },
    },
  },
})
