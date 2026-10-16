#!/usr/bin/env bash
# Local connectivity test: dockerized server <-> client, no Cloudflare.
#   dev/dev.sh up       build + start server and vite (:5173), enroll and start containerized client
#   dev/dev.sh logs     follow logs
#   dev/dev.sh status   list enrolled clients as the server sees them
#   dev/dev.sh native   run the client from source (cargo) against the dev server
#   dev/dev.sh restart-server   bounce the server to test client reconnect
#   dev/dev.sh down     stop containers (keeps data/identity)
#   dev/dev.sh reset    stop and wipe dev DB + client identity
set -euo pipefail
SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
cd "$(dirname "$0")"
DC="docker compose"
URL="http://127.0.0.1:7070"

wait_server() {
  for _ in $(seq 1 60); do
    curl -fsS "$URL/clients" >/dev/null 2>&1 && return 0
    sleep 1
  done
  echo "server did not become ready" >&2; exit 1
}
mint_code() { curl -fsS -X POST "$URL/admin/pairing-code" | jq -r .code; }

case "${1:-up}" in
  up)
    $DC up -d --build server frontend
    wait_server
    if $DC run --rm --no-deps -T --entrypoint test client -f /root/.config/delegation-client/identity.json 2>/dev/null; then
      $DC --profile client up -d --build client
    else
      PAIRING_CODE="$(mint_code)" $DC --profile client up -d --build client
    fi
    sleep 3
    "$SELF" status
    echo "dashboard (built):        $URL/"
    echo "dashboard (hot reload):   http://localhost:5173/"
    ;;
  logs)    $DC --profile client logs -f ;;
  status)  curl -fsS "$URL/clients" | jq . ;;
  native)
    wait_server
    cd ..
    OLDHOME="$HOME"
    # Separate HOME so the dev identity never clobbers ~/.config/delegation-client
    export HOME="$PWD/dev/.native-home"; mkdir -p "$HOME"
    export CARGO_HOME="${CARGO_HOME:-$OLDHOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$OLDHOME/.rustup}"
    exec cargo run -p delegation-client -- --server "$URL" --pairing-code "$(mint_code)"
    ;;
  restart-server) $DC restart server ;;
  down)    $DC --profile client down ;;
  reset)   $DC --profile client down -v ;;
  *) sed -n 2,9p "$SELF"; exit 1 ;;
esac
