#!/bin/sh
set -eu

NAME=machine-slop
ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
CONFIG="$ROOT/vm/lima.yaml"
LIMA_HOME=${LIMA_HOME:-$HOME/.lima}

host_port_for_guest_port() {
  awk -v guest="guestPort: $1" '$0 ~ guest"$" { found = 1; next } found && /hostPort:/ { print $2; exit }' "$CONFIG"
}

AGENTD_PORT=$(host_port_for_guest_port 3000)
MACHINED_PORT=$(host_port_for_guest_port 4000)
AGENTD_URL="http://127.0.0.1:$AGENTD_PORT"
MACHINED_URL="http://127.0.0.1:$MACHINED_PORT"

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

host_port_listener_pid() {
  lsof -nP -tiTCP:"$1" -sTCP:LISTEN 2>/dev/null | sort -u
}

this_vm_hostagent_pid() {
  cat "$LIMA_HOME/$NAME/ha.pid" 2>/dev/null || true
}

host_port_is_served_by_this_vm() {
  listener=$(host_port_listener_pid "$1")
  [ -n "$listener" ] && [ "$listener" = "$(this_vm_hostagent_pid)" ]
}

require_host_ports_free_for_this_vm() {
  for port in "$AGENTD_PORT" "$MACHINED_PORT"; do
    listener=$(host_port_listener_pid "$port")
    [ -z "$listener" ] || host_port_is_served_by_this_vm "$port" ||
      die "host port $port is already used by: $(ps -o comm= -p "$listener" | tr '\n' ' ')"
  done
}

vm_service_is_healthy() {
  host_port_is_served_by_this_vm "$1" &&
    [ "$(curl -fsS --max-time 2 "http://127.0.0.1:$1/health" 2>/dev/null)" = ok ]
}

wait_until_vm_service_is_healthy() {
  service=$1
  port=$2
  deadline=$(($(date +%s) + 900))
  until vm_service_is_healthy "$port"; do
    require_host_ports_free_for_this_vm
    if guest systemctl is-failed --quiet "$service"; then
      guest sudo journalctl -u "$service" -n 50 --no-pager >&2
      die "$service failed"
    fi
    [ "$(date +%s)" -lt "$deadline" ] || die "timed out waiting for $service on port $port"
    sleep 2
  done
}

require_provisioning_finished() {
  guest test -f /etc/systemd/system/machined.service ||
    die "provisioning failed; see 'limactl shell $NAME sudo cat /var/log/cloud-init-output.log'"
}

rebuild_services_from_workspace() {
  guest sudo systemctl restart agentd machined
}

start() {
  case $(instance_status) in
    "")
      require_host_ports_free_for_this_vm
      config_for_this_checkout | limactl create --tty=false --name="$NAME" -
      limactl start --tty=false "$NAME"
      ;;
    Stopped)
      require_instance_matches_config
      require_host_ports_free_for_this_vm
      limactl start --tty=false "$NAME"
      ;;
    Running) require_instance_matches_config ;;
    *) die "$NAME is $(instance_status)" ;;
  esac
  require_provisioning_finished
  rebuild_services_from_workspace
  echo "vm: waiting for agentd and machined to build and start"
  wait_until_vm_service_is_healthy agentd "$AGENTD_PORT"
  echo "vm: agentd is healthy at $AGENTD_URL"
  wait_until_vm_service_is_healthy machined "$MACHINED_PORT"
  echo "vm: machined is healthy at $MACHINED_URL"
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

json_string_field() {
  sed -n "s/.*\"$1\":\"\([^\"]*\)\".*/\1/p"
}

require_machine_runs_a_process() {
  machine=$(curl -fsS --max-time 60 -X POST "$MACHINED_URL/machines") ||
    die "POST $MACHINED_URL/machines failed"
  machine_id=$(echo "$machine" | json_string_field id)
  [ -n "$machine_id" ] || die "unexpected machine: $machine"
  boot_ms=$(echo "$machine" | sed -n 's/.*"boot_ms":\([0-9]*\).*/\1/p')
  machine_url="$MACHINED_URL/machines/$machine_id"
  process_id=$(curl -fsS -X POST "$machine_url/processes" \
    -H 'content-type: application/json' -d '{"program":"/bin/true"}' | json_string_field id)
  result=$(curl -fsS "$machine_url/processes/$process_id/wait" || true)
  curl -fsS -X DELETE "$machine_url" || die "DELETE $machine_url failed"
  case $result in
    *'"code":0'*) ;;
    *) die "/bin/true in machine $machine_id did not exit 0: $result" ;;
  esac
}

smoke() {
  [ "$(instance_status)" = Running ] || die "$NAME is not running; run 'make vm-start'"
  require_instance_matches_config
  vm_service_is_healthy "$AGENTD_PORT" || die "GET $AGENTD_URL/health did not return ok from the VM"
  vm_service_is_healthy "$MACHINED_PORT" || die "GET $MACHINED_URL/health did not return ok from the VM"
  guest systemctl is-active --quiet agentd || die "agentd.service is not active"
  guest systemctl is-active --quiet machined || die "machined.service is not active"
  require_workspace_is_this_checkout_and_writable
  require_guest_matches_configured_limits
  require_machine_runs_a_process
  echo "vm: smoke ok ($AGENTD_URL, $MACHINED_URL, /workspace, $cpus CPUs, $mem_kib KiB memory, machine booted in ${boot_ms}ms)"
}

case ${1:-} in
  start) start ;;
  stop) [ "$(instance_status)" != Running ] || limactl stop "$NAME" ;;
  smoke) smoke ;;
  test)
    [ "$(instance_status)" = Running ] || die "$NAME is not running; run 'make vm-start'"
    limactl shell --workdir /workspace "$NAME" sg kvm -c \
      'SLOP_IMAGE_DIR="$HOME/machine-slop/image" cargo test --locked -p machined -- --ignored'
    ;;
  shell) exec limactl shell --workdir /workspace "$NAME" ;;
  delete) [ -z "$(instance_status)" ] || limactl delete --force "$NAME" ;;
  *) die "usage: $0 start|stop|smoke|test|shell|delete" ;;
esac
