dev/dev.sh up # build, start server, mint pairing code, enroll + start client
dev/dev.sh status # GET /clients: shows enrolled clients and whether each is connected
dev/dev.sh logs
dev/dev.sh restart-server # test client reconnect/backoff
dev/dev.sh native # run the client from source (cargo) against the dev server
dev/dev.sh down # stop, keep data
dev/dev.sh reset # stop and wipe dev DB + identity

cd frontend && npm install && npm run dev # dashboard with hot reload at http://localhost:5173, API calls proxied to the dev server on :7070
http://127.0.0.1:7070/ # the built dashboard exactly as production serves it, from the Rust binary (rebuilt by dev/dev.sh up)
