.PHONY: setup dev build check format

setup:
	pnpm install --frozen-lockfile
	cargo fetch --locked

dev:
	cargo run --locked -p agentd

build:
	cargo build --locked --workspace --release
	pnpm build

check:
	cargo fmt --all -- --check
	cargo clippy --locked --workspace --all-targets -- -D warnings
	pnpm format:check
	pnpm check

format:
	cargo fmt --all
	pnpm format
