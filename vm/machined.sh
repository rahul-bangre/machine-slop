#!/bin/sh
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
IMAGE_DIR=$HOME/machine-slop/image
STATE_DIR=$HOME/machine-slop/state
API_KEY_FILE=$HOME/machine-slop/api-key
TENANTS_FILE=$HOME/machine-slop/tenants.json

cd "$ROOT"
cargo build --locked --release -p agentd -p machined
target=$(cargo metadata --locked --format-version 1 --no-deps |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
vm/image/build.sh "$IMAGE_DIR" "$target/release/agentd"
if [ ! -s "$API_KEY_FILE" ]; then
  (umask 077 && od -An -tx1 -N32 /dev/urandom | tr -d ' \n' > "$API_KEY_FILE")
fi
cat > "$TENANTS_FILE" <<JSON
{ "tenants": [{
  "id": "dev",
  "api_key_sha256": ["$(printf %s "$(cat "$API_KEY_FILE")" | sha256sum | cut -d' ' -f1)"],
  "quotas": { "running_machines": 16, "memory_mib": 1048576, "disk_gib": 1048576, "creates_per_second": 100 }
}] }
JSON
exec sudo "$target/release/machined" 127.0.0.1:4000 "$IMAGE_DIR" "$STATE_DIR" "$TENANTS_FILE"
