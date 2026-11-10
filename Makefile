.PHONY: setup dev build check format

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
