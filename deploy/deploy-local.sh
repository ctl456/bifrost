#!/usr/bin/env bash
#
# Run this repository's image on the machine you are sitting at, with the account key
# held here rather than handed to every caller.
#
# The arrangement is the one `docker-compose.access.yml` describes. What differs is the
# account the process runs as. The overlay's process is the image's uid 10001, so the
# copy of the key it reads has to be `chown`ed to that uid, and that is a command only
# root can run. Here the container is told to run as the invoking user instead, and the
# key is mounted from the home directory it is already in: the same arrangement, one
# line shorter, and no sudo. Nothing else is loosened -- the read-only filesystem, the
# dropped capabilities and the port bound to the host's loopback are the base compose
# file's, which is the unit file's.
#
#   deploy/deploy-local.sh                  start it and wait until it answers
#   deploy/deploy-local.sh --check          what it makes of the file it serves
#   deploy/deploy-local.sh --token laptop   issue a caller a token
#   deploy/deploy-local.sh --token-list     who it has issued to
#   deploy/deploy-local.sh --token-revoke X stop honouring X's token
#   deploy/deploy-local.sh --logs           follow what it says
#   deploy/deploy-local.sh --down           stop and remove it
#
# Two files it needs, and it names the one that is missing rather than starting without
# it. `bifrost.container.toml` is the configuration, and the two lines of it that matter
# here are `host`, which may not be `127.0.0.1`, and `[access]`, which is what makes
# this the key-holding arrangement. `~/.commandcode/auth.json` is the key `cmdc login`
# wrote; it is mounted rather than copied, because the copy the overlay wants is one
# only root can make.
#
# Every path inside the container is one the configuration names, so the file served is
# the file validated: /etc/bifrost/bifrost.toml, the key at /etc/bifrost/auth.json, and
# /var/lib/bifrost -- the working directory the image defaults, which is where the
# relative `tokens_file` lands, and so the one thing mounted writable.
#
# Each of these is the default of an environment variable: BIFROST_IMAGE, BIFROST_NAME,
# BIFROST_PORT, BIFROST_BIND, BIFROST_CONFIG, BIFROST_KEY, BIFROST_STATE.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

image="${BIFROST_IMAGE:-ghcr.io/ctl456/bifrost:latest}"
name="${BIFROST_NAME:-bifrost}"
port="${BIFROST_PORT:-3050}"
# Loopback by default: the reach a host deployment has, and not a step further. A
# deployment meant to answer a colleague's harness sets BIFROST_BIND=0.0.0.0.
bind="${BIFROST_BIND:-0.0.0.0}"
config="${BIFROST_CONFIG:-$root/bifrost.container.toml}"
key="${BIFROST_KEY:-$HOME/.commandcode/auth.json}"
state="${BIFROST_STATE:-${XDG_DATA_HOME:-$HOME/.local/share}/bifrost}"

in_config=/etc/bifrost/bifrost.toml
in_key=/etc/bifrost/auth.json
in_state=/var/lib/bifrost

say() { printf '%s\n' "$*"; }
die() { printf '%s\n' "$*" >&2; exit 1; }

# The mounts and the account, as a NUL-separated list for `mapfile`.
#
# Shared by every command rather than written once per command because `--token-new`
# writes the file the server reads: a token issued against a different set of mounts is
# a token the server never sees, and the symptom is a caller refused a credential it was
# just handed.
mounts() {
  printf '%s\0' \
    --user "$(id -u):$(id -g)" \
    -v "$config:$in_config:ro" \
    -v "$key:$in_key:ro" \
    -v "$state:$in_state"
}

# What has to be in place before anything is run.
require() {
  command -v docker >/dev/null || die "docker is not on this PATH"
  docker info >/dev/null 2>&1 ||
    die "cannot reach the docker daemon; is it running, and can this user talk to it?"
  [ -f "$config" ] || die "no configuration at $config"
  [ -f "$key" ] || die "no key at $key -- 'cmdc login' writes one, or set BIFROST_KEY"
}

require_running() {
  require
  docker inspect "$name" >/dev/null 2>&1 ||
    die "no container named $name; start it with $0"
}

# What the binary makes of the file, in a container that exits.
#
# Binds nothing and writes nothing, and it runs before a server starts rather than after
# one fails: this is the command that reads the key file, so a key it cannot read is
# reported here rather than at the first turn.
check() {
  local args=()
  mapfile -d '' -t args < <(mounts)
  docker run --rm "${args[@]}" "$image" --config "$in_config" --check
}

# The binary's own commands, against the file the running server serves.
exec_in() {
  docker exec "$name" /usr/local/bin/bifrost --config "$in_config" "$@"
}

wait_healthy() {
  local status
  for ((i = 0; i < 90; i++)); do
    status="$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}starting{{end}}' "$name" 2>/dev/null)" ||
      die "the container $name disappeared"
    case "$status" in
      healthy)
        say "healthy on http://$bind:$port"
        return 0
        ;;
      unhealthy)
        say "the probe never passed; the last thing it said was:" >&2
        docker logs --tail 20 "$name" >&2 || true
        return 1
        ;;
    esac
    if [ "$(docker inspect --format '{{.State.Running}}' "$name" 2>/dev/null)" != true ]; then
      say "it exited; the last thing it said was:" >&2
      docker logs --tail 20 "$name" >&2 || true
      return 1
    fi
    sleep 1
  done
  die "it did not become healthy within 90s; 'docker logs $name' says why"
}

up() {
  mkdir -p "$state"
  # Before the old container is removed rather than after: a configuration this build
  # refuses is a reason to leave the deployment that was working alone.
  check || die "this build will not serve $config"

  # Replacing rather than complaining: running this twice is how a changed image or a
  # changed configuration reaches a container that is already up.
  if docker inspect "$name" >/dev/null 2>&1; then
    say "replacing the container $name"
    docker rm -f "$name" >/dev/null
  fi

  local args=()
  mapfile -d '' -t args < <(mounts)
  docker run -d \
    --name "$name" \
    --restart unless-stopped \
    -p "$bind:$port:3050" \
    --read-only \
    --tmpfs /tmp \
    --cap-drop ALL \
    --security-opt no-new-privileges:true \
    --stop-timeout 90 \
    "${args[@]}" \
    "$image" --config "$in_config" >/dev/null

  say "started $image as $name"
  wait_healthy
  say ""
  say "issue a caller a token:  $0 --token laptop"
}

case "${1:-}" in
  --down)
    docker rm -f "$name" >/dev/null && say "removed $name"
    ;;
  --logs)
    docker logs --follow "$name"
    ;;
  --check)
    require
    check
    ;;
  --token)
    shift
    require_running
    exec_in --token-new "$@"
    ;;
  --token-list | --token-revoke)
    command="$1"
    shift
    require_running
    exec_in "$command" "$@"
    ;;
  "")
    require
    up
    ;;
  *)
    die "unknown argument $1; --check, --token, --token-list, --token-revoke, --logs, --down"
    ;;
esac
