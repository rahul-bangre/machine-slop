VM_TARGETS := vm-start vm-stop vm-smoke vm-shell vm-delete

.PHONY: setup dev build check format $(VM_TARGETS)

setup:
	deno install --frozen-lockfile
	cargo fetch --locked

dev:
	cargo run --locked -p agentd

build:
	cargo build --locked --workspace --release

check:
	cargo fmt --all -- --check
	cargo clippy --locked --workspace --all-targets -- -D warnings
	deno task format:check
	deno task lint
	deno task check

format:
	cargo fmt --all
	deno task format

# Development VM; see vm/vm.sh.
$(VM_TARGETS):
	vm/vm.sh $(@:vm-%=%)
