import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    // Dev only: in production the Rust server serves this app and the API
    // from the same origin, so the frontend only ever uses relative paths.
    // Point these at the local axum instance from dev/dev.sh.
    proxy: {
      '/clients': 'http://localhost:7070',
      '/admin': 'http://localhost:7070',
    },
  },
})
