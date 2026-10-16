import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

const apiTarget = process.env.API_TARGET ?? 'http://localhost:7070'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    // Dev only: in production the Rust server serves this app and the API
    // from the same origin, so the frontend only ever uses relative paths.
    // Point these at the local axum instance from dev/dev.sh. API_TARGET is
    // set by the dev compose stack, where the server is reachable as `server`.
    proxy: {
      '/clients': apiTarget,
      '/admin': apiTarget,
    },
  },
})
