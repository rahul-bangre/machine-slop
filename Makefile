VM_TARGETS := vm-start vm-stop vm-smoke vm-test vm-shell vm-delete

.PHONY: setup dev build check format generate test $(VM_TARGETS)

setup:
	deno install --frozen-lockfile
	cargo fetch --locked

dev:
	cargo run --locked -p agentd

build:
	cargo build --locked --workspace --release

check:
	cargo run --locked -p slop-process --bin export-types -- --check
	cargo run --locked -p slop-machines --bin export-types -- --check
	cargo fmt --all -- --check
	cargo clippy --locked --workspace --all-targets -- -D warnings
	deno task format:check
	deno task lint
	deno task check

format:
	cargo fmt --all
	deno task format

$(VM_TARGETS):
	vm/vm.sh $(@:vm-%=%)

generate:
	cargo run --locked -p slop-process --bin export-types
	cargo run --locked -p slop-machines --bin export-types

test:
	cargo test --locked --workspace
	cargo build --locked -p agentd
	deno task test
