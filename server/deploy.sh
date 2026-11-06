#!/usr/bin/env bash
# Server-side deploy entry point. The Deploy workflow calls it over SSH
# through the Cloudflare Tunnel as the forced command of a restricted key
# (see docs/DEV-USAGE-AND-DEPLOYMENT.md §2.8), so that key can run nothing but
# the commands below. It also works by hand from this directory.
#
#   deploy <sha>   pull the GHCR image tagged <sha>, switch to it, health-check
#                  it, and roll back automatically if it doesn't come up
#   rollback       switch back to the previously deployed image
#   status         show the deployed image and container state
#
# For `deploy`, stdin may carry two lines: a registry username and a
# short-lived token (CI sends the job's GITHUB_TOKEN). They are used for that
# one pull and never written to disk. Without them, the host's own
# `docker login ghcr.io` (if any) is used.
#
# The deployed image is recorded in .env as DELEGATION_IMAGE/DELEGATION_TAG,
# which docker-compose.yml reads, so a plain `docker compose up -d` later
# keeps running the same image.
set -euo pipefail

readonly DEFAULT_IMAGE=ghcr.io/sterance/delegation-server
readonly SERVICE=delegation-server
readonly CURL_IMAGE=curlimages/curl:8.10.1
readonly HEALTH_URL=http://127.0.0.1:7070/clients
readonly HEALTH_TIMEOUT=60 # seconds

cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")"

log() { printf '[deploy] %s\n' "$*" >&2; }
die() {
  log "error: $*"
  exit 1
}

# Temporary docker config holding the registry token during a pull.
PULL_CFG=
trap 'if [[ -n $PULL_CFG ]]; then rm -rf "$PULL_CFG"; fi' EXIT

env_get() {
  sed -n "s/^$1=//p" .env | tail -n 1
}

# Sets KEY=value in .env, or removes the key when value is empty (so
# compose falls back to its defaults). Rewrites the file in place to keep
# its owner and mode.
env_set() {
  local key=$1 value=$2 tmp
  tmp=$(mktemp .env.XXXXXX)
  grep -v "^$key=" .env >"$tmp" || true
  if [[ -n $value ]]; then
    printf '%s=%s\n' "$key" "$value" >>"$tmp"
  fi
  cat "$tmp" >.env
  rm -f "$tmp"
}

pull() {
  local ref=$1 user= token=
  if [[ ! -t 0 ]]; then
    IFS= read -r -t 5 user || true
    IFS= read -r -t 5 token || true
  fi
  if [[ -n $token ]]; then
    PULL_CFG=$(mktemp -d)
    docker --config "$PULL_CFG" login ghcr.io -u "${user:-deploy}" --password-stdin <<<"$token" >/dev/null
    docker --config "$PULL_CFG" pull -q "$ref" >/dev/null
    rm -rf "$PULL_CFG"
    PULL_CFG=
  else
    docker pull -q "$ref" >/dev/null
  fi
  if ! docker image inspect "$CURL_IMAGE" >/dev/null 2>&1; then
    docker pull -q "$CURL_IMAGE" >/dev/null
  fi
}

# Neither image ships curl, so probe from a throwaway container that shares
# the server container's network namespace.
healthy() {
  local cid deadline=$((SECONDS + HEALTH_TIMEOUT))
  while ((SECONDS < deadline)); do
    cid=$(docker compose ps -q "$SERVICE" 2>/dev/null || true)
    if [[ -n $cid ]] &&
      [[ $(docker inspect -f '{{.State.Status}}' "$cid" 2>/dev/null) == running ]] &&
      docker run --rm --network "container:$cid" "$CURL_IMAGE" \
        -fsS -o /dev/null --max-time 3 "$HEALTH_URL" 2>/dev/null; then
      return 0
    fi
    sleep 2
  done
  return 1
}

# Points .env at image:tag (empty values mean compose's local-build
# default), recreates the stack without building, and waits for health.
switch_to() {
  env_set DELEGATION_IMAGE "$1"
  env_set DELEGATION_TAG "$2"
  docker compose up -d --no-build
  healthy
}

describe() { printf '%s:%s' "${1:-delegation-server}" "${2:-latest}"; }

# Removes local tags of the deployed image other than the ones given.
prune() {
  local image=$1 tag
  shift
  while read -r tag; do
    [[ $tag == '<none>' || " $* " == *" $tag "* ]] && continue
    if docker image rm "$image:$tag" >/dev/null 2>&1; then
      log "removed old image $image:$tag"
    fi
  done < <(docker image ls "$image" --format '{{.Tag}}')
}

cmd_deploy() {
  local tag=$1 image prev_image prev_tag
  [[ $tag =~ ^[0-9a-f]{40}$ ]] || die "tag must be a full 40-character commit SHA"
  prev_image=$(env_get DELEGATION_IMAGE)
  prev_tag=$(env_get DELEGATION_TAG)
  image=${prev_image:-$DEFAULT_IMAGE}

  log "pulling $image:$tag"
  pull "$image:$tag"

  # Redeploying the current image must not overwrite the rollback target.
  if [[ "$prev_image:$prev_tag" != "$image:$tag" ]]; then
    printf '%s %s\n' "$prev_image" "$prev_tag" >.deploy-previous
  fi

  log "switching to $image:$tag"
  if switch_to "$image" "$tag"; then
    log "healthy: $image:$tag"
    prune "$image" "$tag" "$prev_tag"
    return 0
  fi

  log "health check failed after ${HEALTH_TIMEOUT}s; recent logs:"
  docker compose logs --tail 50 "$SERVICE" >&2 || true
  log "rolling back to $(describe "$prev_image" "$prev_tag")"
  if switch_to "$prev_image" "$prev_tag"; then
    log "rollback healthy"
  else
    log "rollback is unhealthy too; needs manual attention"
  fi
  exit 1
}

cmd_rollback() {
  local cur_image cur_tag prev_image= prev_tag=
  [[ -f .deploy-previous ]] || die "no previous deploy recorded"
  read -r prev_image prev_tag <.deploy-previous || true
  cur_image=$(env_get DELEGATION_IMAGE)
  cur_tag=$(env_get DELEGATION_TAG)

  log "rolling back to $(describe "$prev_image" "$prev_tag")"
  if switch_to "$prev_image" "$prev_tag"; then
    # Swap, so a second rollback returns to where we started.
    printf '%s %s\n' "$cur_image" "$cur_tag" >.deploy-previous
    log "healthy: $(describe "$prev_image" "$prev_tag")"
    return 0
  fi
  log "rollback target is unhealthy; restoring $(describe "$cur_image" "$cur_tag")"
  switch_to "$cur_image" "$cur_tag" || log "restore is unhealthy too; needs manual attention"
  exit 1
}

cmd_status() {
  log "deployed: $(describe "$(env_get DELEGATION_IMAGE)" "$(env_get DELEGATION_TAG)")"
  docker compose ps
}

usage() { die "usage: deploy <sha> | rollback | status"; }

# With a forced command, the requested command arrives in
# SSH_ORIGINAL_COMMAND rather than as arguments.
if [[ -n ${SSH_ORIGINAL_COMMAND+x} ]]; then
  read -r -a args <<<"$SSH_ORIGINAL_COMMAND"
else
  args=("$@")
fi

[[ -f .env ]] || die "$PWD/.env not found (it holds TUNNEL_TOKEN; see §2.2)"

exec 9>.deploy.lock
flock -w 300 9 || die "another deploy is still running"

case "${args[0]:-}" in
  deploy) ((${#args[@]} == 2)) || usage; cmd_deploy "${args[1]}" ;;
  rollback) ((${#args[@]} == 1)) || usage; cmd_rollback ;;
  status) ((${#args[@]} == 1)) || usage; cmd_status ;;
  *) usage ;;
esac
