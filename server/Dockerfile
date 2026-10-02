# Pure-JS dependencies only (@noble/ed25519, ws) — deliberately, so this
# builds cleanly on Alpine's musl libc without needing a native build
# toolchain (python3/make/g++) for node-gyp. `ws` has optional native
# accelerators (bufferutil, utf-8-validate) that node-gyp would otherwise
# try and fail to compile here; --omit=optional skips that entirely
# rather than relying on npm's fallback-on-failure behavior.
FROM node:20-alpine AS builder
WORKDIR /app
COPY package.json package-lock.json* ./
RUN npm install --omit=optional
COPY tsconfig.json ./
COPY src ./src
RUN npm run build

FROM node:20-alpine
WORKDIR /app
COPY --from=builder /app/package.json ./
COPY --from=builder /app/node_modules ./node_modules
COPY --from=builder /app/dist ./dist

# Registry (clients.json, pending-codes.json) lives here — mount a
# volume at this path or every container recreation forgets every
# enrolled client.
VOLUME /app/data

EXPOSE 7070
CMD ["node", "dist/server.js"]
