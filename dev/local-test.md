dev/dev.sh up # build, start server, sign in as the dev admin (admin / dev-admin-password, created via /setup on a fresh DB), mint pairing code, enroll + start client
dev/dev.sh status # GET /api/clients: shows enrolled clients and whether each is connected
dev/dev.sh logs
dev/dev.sh restart-server # test client reconnect/backoff
dev/dev.sh native # run the client from source (cargo) against the dev server
dev/dev.sh down # stop, keep data
dev/dev.sh reset # stop and wipe dev DB + identity

cd frontend && npm install && npm run dev # dashboard with hot reload at http://localhost:5173, API calls proxied to the dev server on :7070
http://127.0.0.1:7070/ # the built dashboard exactly as production serves it, from the Rust binary (rebuilt by dev/dev.sh up)

# Models (llama.cpp)
# Expand a client card, Models > Install. The first install also fetches llama.cpp (pinned
# in client/src/inference/runtime.rs, checked by SHA-256) into the client's data dir:
#   Linux   $XDG_DATA_HOME/delegation-client, else ~/.local/share/delegation-client
#           (dev.sh native: dev/.native-home/.local/share/delegation-client)
#   Windows %LOCALAPPDATA%\delegation-client
#   Docker  the client-models volume
# Native Linux clients need libssl3 and libgomp1 for llama-server (Debian/Ubuntu:
# apt install libssl3 libgomp1), plus glibc >= 2.34 and GCC 12's libstdc++.
DELEGATION_LOG=debug dev/dev.sh native # also logs everything llama-server prints
