#!/bin/sh
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

config_for_this_checkout() {
  sed "s|@WORKSPACE@|$ROOT|" "$CONFIG"
}

instance_status() {
  limactl list --format '{{.Status}}' "$NAME" 2>/dev/null || true
}

guest() {
  limactl shell --workdir / "$NAME" "$@"
}

require_instance_matches_config() {
  config_for_this_checkout | cmp -s - "$LIMA_HOME/$NAME/lima.yaml" ||
    die "$NAME was created from a different vm/lima.yaml or checkout; run 'make vm-delete vm-start'"
}

host_port_owner() {
  lsof -nP -iTCP:"$HOST_PORT" -sTCP:LISTEN -Fc 2>/dev/null | sed -n 's/^c//p' | sort -u
}

require_host_port_free_for_lima() {
  owner=$(host_port_owner)
  [ -z "$owner" ] || [ "$owner" = limactl ] || die "host port $HOST_PORT is already used by: $owner"
}

vm_agentd_is_healthy() {
  [ "$(host_port_owner)" = limactl ] &&
    [ "$(curl -fsS --max-time 2 "$HEALTH_URL" 2>/dev/null)" = ok ]
}

wait_until_vm_agentd_is_healthy() {
  deadline=$(($(date +%s) + 900))
  until vm_agentd_is_healthy; do
    if guest systemctl is-failed --quiet agentd; then
      guest sudo journalctl -u agentd -n 50 --no-pager >&2
      die "agentd failed"
    fi
    [ "$(date +%s)" -lt "$deadline" ] || die "timed out waiting for $HEALTH_URL"
    sleep 2
  done
}

require_provisioning_finished() {
  guest test -f /etc/systemd/system/agentd.service ||
    die "provisioning failed; see 'limactl shell $NAME sudo cat /var/log/cloud-init-output.log'"
}

rebuild_agentd_from_workspace() {
  guest sudo systemctl restart agentd
}

start() {
  case $(instance_status) in
    "")
      require_host_port_free_for_lima
      config_for_this_checkout | limactl create --tty=false --name="$NAME" -
      limactl start --tty=false "$NAME"
      ;;
    Stopped)
      require_instance_matches_config
      require_host_port_free_for_lima
      limactl start --tty=false "$NAME"
      ;;
    Running) require_instance_matches_config ;;
    *) die "$NAME is $(instance_status)" ;;
  esac
  require_provisioning_finished
  rebuild_agentd_from_workspace
  echo "vm: waiting for agentd to build and start"
  wait_until_vm_agentd_is_healthy
  echo "vm: agentd is healthy at $HEALTH_URL"
}

require_workspace_is_this_checkout_and_writable() {
  probe=$(guest mktemp /workspace/.vm-smoke.XXXXXX) || die "/workspace is not writable"
  host_probe="$ROOT/${probe#/workspace/}"
  trap 'rm -f "$host_probe"' EXIT
  [ -f "$host_probe" ] || die "/workspace is not this checkout"
}

require_guest_matches_configured_limits() {
  configured_cpus=$(sed -n 's/^cpus: *\([0-9]*\)$/\1/p' "$CONFIG")
  configured_kib=$(($(sed -n 's/^memory: *\([0-9]*\)GiB$/\1/p' "$CONFIG") * 1024 * 1024))
  min_kib_after_kernel_reservation=$((configured_kib * 9 / 10))

  cpus=$(guest nproc)
  [ "$cpus" = "$configured_cpus" ] || die "guest has $cpus CPUs, want $configured_cpus"

  mem_kib=$(guest awk '/^MemTotal:/ { print $2 }' /proc/meminfo)
  [ "$mem_kib" -le "$configured_kib" ] && [ "$mem_kib" -ge "$min_kib_after_kernel_reservation" ] ||
    die "guest has $mem_kib KiB of memory, want about $configured_kib KiB"
}

smoke() {
  [ "$(instance_status)" = Running ] || die "$NAME is not running; run 'make vm-start'"
  require_instance_matches_config
  vm_agentd_is_healthy || die "GET $HEALTH_URL did not return ok from the VM"
  guest systemctl is-active --quiet agentd || die "agentd.service is not active"
  require_workspace_is_this_checkout_and_writable
  require_guest_matches_configured_limits
  echo "vm: smoke ok ($HEALTH_URL, /workspace, $cpus CPUs, $mem_kib KiB memory)"
}

case ${1:-} in
  start) start ;;
  stop) [ "$(instance_status)" != Running ] || limactl stop "$NAME" ;;
  smoke) smoke ;;
  shell) exec limactl shell --workdir /workspace "$NAME" ;;
  delete) [ -z "$(instance_status)" ] || limactl delete --force "$NAME" ;;
  *) die "usage: $0 start|stop|smoke|shell|delete" ;;
esac
