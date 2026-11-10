#!/bin/sh
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
IMAGE_DIR=$HOME/machine-slop/image
STATE_DIR=$HOME/machine-slop/state

cd "$ROOT"
cargo build --locked --release -p agentd -p machined
target=$(cargo metadata --locked --format-version 1 --no-deps |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
vm/image/build.sh "$IMAGE_DIR" "$target/release/agentd"
exec "$target/release/machined" 127.0.0.1:4000 "$IMAGE_DIR" "$STATE_DIR"
