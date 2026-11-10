#!/bin/sh
# Manage the Machine Slop development VM: start | stop | smoke | shell | delete.
set -eu

NAME=machine-slop
ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
CONFIG="$ROOT/vm/lima.yaml"
LIMA_HOME=${LIMA_HOME:-$HOME/.lima}
HOST_PORT=$(sed -n 's/^ *hostPort: *\([0-9]*\)$/\1/p' "$CONFIG")
HEALTH_URL="http://127.0.0.1:$HOST_PORT/health"

die() {
  echo "vm: $*" >&2
  exit 1
}

render() {
  sed "s|@WORKSPACE@|$ROOT|" "$CONFIG"
}

# Empty if the instance does not exist, else Running or Stopped.
status() {
  limactl list --format '{{.Status}}' "$NAME" 2>/dev/null || true
}

guest() {
  limactl shell --workdir / "$NAME" "$@"
}

# An instance keeps the config it was created with. Refuse one that no longer
# matches vm/lima.yaml or that mounts another checkout.
check_config() {
  render | cmp -s - "$LIMA_HOME/$NAME/lima.yaml" ||
    die "$NAME was created from a different vm/lima.yaml or checkout; run 'make vm-delete vm-start'"
}

port_owner() {
  lsof -nP -iTCP:"$HOST_PORT" -sTCP:LISTEN -Fc 2>/dev/null | sed -n 's/^c//p' | sort -u
}

# Lima cannot forward a host port that something else already holds.
check_port_free() {
  owner=$(port_owner)
  [ -z "$owner" ] || [ "$owner" = limactl ] || die "host port $HOST_PORT is already used by: $owner"
}

# The VM's agentd is healthy when Lima owns the host port and /health is ok.
# Checking the owner keeps another server on the port from passing.
healthy() {
  [ "$(port_owner)" = limactl ] &&
    [ "$(curl -fsS --max-time 2 "$HEALTH_URL" 2>/dev/null)" = ok ]
}

wait_healthy() {
  deadline=$(($(date +%s) + 900))
  until healthy; do
    if guest systemctl is-failed --quiet agentd; then
      guest sudo journalctl -u agentd -n 50 --no-pager >&2
      die "agentd failed"
    fi
    [ "$(date +%s)" -lt "$deadline" ] || die "timed out waiting for $HEALTH_URL"
    sleep 2
  done
}

start() {
  case $(status) in
    "")
      check_port_free
      render | limactl create --tty=false --name="$NAME" -
      limactl start --tty=false "$NAME"
      ;;
    Stopped)
      check_config
      check_port_free
      limactl start --tty=false "$NAME"
      ;;
    Running) check_config ;;
    *) die "$NAME is $(status)" ;;
  esac
  # Lima reports READY even when provisioning fails; the unit is written last.
  guest test -f /etc/systemd/system/agentd.service ||
    die "provisioning failed; see 'limactl shell $NAME sudo cat /var/log/cloud-init-output.log'"
  # Restart so agentd is rebuilt from the current /workspace sources.
  guest sudo systemctl restart agentd
  echo "vm: waiting for agentd to build and start"
  wait_healthy
  echo "vm: agentd is healthy at $HEALTH_URL"
}

smoke() {
  [ "$(status)" = Running ] || die "$NAME is not running; run 'make vm-start'"
  check_config

  healthy || die "GET $HEALTH_URL did not return ok from the VM"
  guest systemctl is-active --quiet agentd || die "agentd.service is not active"

  # A file written in the guest must appear in this checkout.
  probe=$(guest mktemp /workspace/.vm-smoke.XXXXXX) || die "/workspace is not writable"
  trap 'rm -f "$ROOT/${probe#/workspace/}"' EXIT
  [ -f "$ROOT/${probe#/workspace/}" ] || die "/workspace is not this checkout"

  # The limits vm/lima.yaml declares, checked against what the guest sees.
  want_cpus=$(sed -n 's/^cpus: *\([0-9]*\)$/\1/p' "$CONFIG")
  want_kib=$(($(sed -n 's/^memory: *\([0-9]*\)GiB$/\1/p' "$CONFIG") * 1024 * 1024))
  cpus=$(guest nproc)
  [ "$cpus" = "$want_cpus" ] || die "guest has $cpus CPUs, want $want_cpus"
  # MemTotal excludes memory the kernel reserves, so accept 90-100%.
  mem_kib=$(guest awk '/^MemTotal:/ { print $2 }' /proc/meminfo)
  [ "$mem_kib" -le "$want_kib" ] && [ "$mem_kib" -ge $((want_kib * 9 / 10)) ] ||
    die "guest has $mem_kib KiB of memory, want about $want_kib KiB"

  echo "vm: smoke ok ($HEALTH_URL, /workspace, $cpus CPUs, $mem_kib KiB memory)"
}

case ${1:-} in
  start) start ;;
  stop) [ "$(status)" != Running ] || limactl stop "$NAME" ;;
  smoke) smoke ;;
  shell) exec limactl shell --workdir /workspace "$NAME" ;;
  delete) [ -z "$(status)" ] || limactl delete --force "$NAME" ;;
  *) die "usage: $0 start|stop|smoke|shell|delete" ;;
esac
