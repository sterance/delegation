#!/usr/bin/env bash
# Local connectivity test: dockerized server <-> client, no Cloudflare.
#   dev/dev.sh up       build + start server and vite (:5173), enroll and start containerized client
#   dev/dev.sh logs     follow logs
#   dev/dev.sh status   list enrolled clients as the server sees them (sample counts, not samples)
#   dev/dev.sh native   run the client from source (cargo) against the dev server
#   dev/dev.sh restart-server   bounce the server to test client reconnect
#   dev/dev.sh down     stop containers (keeps data/identity)
#   dev/dev.sh reset    stop and wipe dev DB + client identity
#
# The API needs a session, so this signs in as a dev admin (by default
# admin / dev-admin-password; override with DEV_ADMIN_USER and
# DEV_ADMIN_PASSWORD). On a fresh database it creates that account through
# /api/auth/setup with the token from the server log. Sign in to the
# dashboard with the same credentials.
set -euo pipefail
SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
DEV_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DEV_DIR"
DC="docker compose -f $DEV_DIR/docker-compose.yml"
URL="http://127.0.0.1:7070"
JAR="$DEV_DIR/.cookies"
DEV_ADMIN_USER="${DEV_ADMIN_USER:-admin}"
DEV_ADMIN_PASSWORD="${DEV_ADMIN_PASSWORD:-dev-admin-password}"

wait_server() {
  echo "waiting for the server at $URL ..." >&2
  for _ in $(seq 1 60); do
    curl -fsS --max-time 2 "$URL/api/health" >/dev/null 2>&1 && return 0
    # It exits at once on bad config or an old database: say why, don't wait.
    if [[ $($DC ps -a --format '{{.State}}' server 2>/dev/null) == exited ]]; then
      echo "the server exited; the end of its log:" >&2
      $DC logs --no-log-prefix --tail 3 server >&2
      if $DC logs --tail 3 server 2>/dev/null | grep -q 'REFUSING TO START'; then
        echo >&2
        echo "The dev database is from an older build (there are no migrations). Run:" >&2
        echo "  dev/dev.sh reset   # wipes the dev DB and the dev client's identity" >&2
        echo "  dev/dev.sh up" >&2
      fi
      exit 1
    fi
    sleep 1
  done
  echo "server did not become ready" >&2; exit 1
}

# curl with the dev admin's session, and the Origin header the server
# requires on anything but GET.
api() { curl -fsS -b "$JAR" -c "$JAR" -H "Origin: $URL" "$@"; }

dev_login() {
  if api "$URL/api/me" >/dev/null 2>&1; then return 0; fi
  local body token
  body=$(jq -n --arg u "$DEV_ADMIN_USER" --arg p "$DEV_ADMIN_PASSWORD" '{username: $u, password: $p, remember: true}')
  if [[ $(curl -fsS "$URL/api/auth/status" | jq -r .setup_required) == true ]]; then
    token=$($DC logs server 2>/dev/null | grep -o 'setup token: [A-Za-z0-9_-]*' | tail -n 1 | awk '{print $3}')
    [[ -n $token ]] || { echo "no setup token in the server log; try dev/dev.sh restart-server" >&2; exit 1; }
    api -X POST -H 'Content-Type: application/json' \
      -d "$(jq --arg t "$token" '. + {token: $t}' <<<"$body")" "$URL/api/auth/setup" >/dev/null
    echo "created the dev admin account: $DEV_ADMIN_USER / $DEV_ADMIN_PASSWORD" >&2
  elif ! api -X POST -H 'Content-Type: application/json' -d "$body" "$URL/api/auth/login" >/dev/null; then
    echo "couldn't sign in as $DEV_ADMIN_USER: set DEV_ADMIN_USER/DEV_ADMIN_PASSWORD, or wipe with dev/dev.sh reset" >&2
    exit 1
  fi
}

mint_code() { dev_login; api -X POST "$URL/api/pairing-codes" | jq -r .code; }

case "${1:-up}" in
  up)
    $DC up -d --build server
    # Recreated every time: it only runs `npm install` on start, and its
    # node_modules lives in a volume, so new frontend deps would otherwise
    # never get installed into an already-running container.
    $DC up -d --force-recreate frontend
    wait_server
    dev_login
    if $DC run --rm --no-deps -T --entrypoint test client -f /root/.config/delegation-client/identity.json 2>/dev/null; then
      $DC --profile client up -d --build client
    else
      PAIRING_CODE="$(mint_code)" $DC --profile client up -d --build client
    fi
    sleep 3
    "$SELF" status
    echo "dashboard (built):        $URL/"
    echo "dashboard (hot reload):   http://localhost:5173/"
    echo "sign in as:               $DEV_ADMIN_USER / $DEV_ADMIN_PASSWORD"
    ;;
  logs)    $DC --profile client logs -f ;;
  status)
    dev_login
    api "$URL/api/clients" | jq '{epoch, latest_seq, clients: [.clients[] |
      {friendly_name, hostname, owner: .owner.username, connected, last_seen_at, samples: (.samples | length),
       latest: (.samples[-1] // null | if . then {cpu_percent, gpus: [.gpus[].name]} else null end)}]}'
    ;;
  native)
    wait_server
    CODE="$(mint_code)"
    cd ..
    OLDHOME="$HOME"
    # Separate HOME so the dev identity never clobbers ~/.config/delegation-client
    export HOME="$PWD/dev/.native-home"; mkdir -p "$HOME"
    export CARGO_HOME="${CARGO_HOME:-$OLDHOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$OLDHOME/.rustup}"
    exec cargo run -p delegation-client -- --server "$URL" --pairing-code "$CODE"
    ;;
  restart-server) $DC restart server ;;
  down)    $DC --profile client down ;;
  reset)   $DC --profile client down -v; rm -f "$JAR" ;;
  *) sed -n 2,9p "$SELF"; exit 1 ;;
esac
