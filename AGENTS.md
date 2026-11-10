# Repository instructions

## Code structure

- Keep HTTP routing and daemon startup in `apps/`; process and machine lifecycle
  logic belongs in `crates/`.
- Keep the TypeScript client in `packages/sdk/` and benchmark code in `bench/`.
  Follow the existing provider and observer interfaces when adding benchmark
  implementations.
- Prefer small, direct changes that follow neighboring code. Add shared
  abstractions when they remove concrete duplication.

## Rust and TypeScript

- Use the pinned Rust and Deno versions in `rust-toolchain.toml` and
  `.tool-versions`. Use Cargo workspace dependencies and settings where defined.
- Format Rust with rustfmt and TypeScript with `deno fmt` (`make format` runs
  both). Follow `.editorconfig`: four spaces for Rust and TypeScript, two for
  other files, and tabs in Makefiles.
- Keep Rust free of unsafe code; the workspace forbids it. Use typed errors in
  library code and map them to HTTP responses at the application boundary.
- Preserve the strict TypeScript checks in `deno.json`; narrow unions and handle
  missing values explicitly. Use `import type` for type-only imports.
- Rust contracts in `crates/process/src/contract.rs` and
  `crates/machines/src/contract.rs` define the SDK wire types. After changing
  them, run `make generate` and commit the generated files. Do not hand-edit
  `packages/sdk/src/*.gen.ts`.

## Behavior and tests

- Preserve the documented process and machine contracts, including cancellation,
  retries, output replay, and cleanup. Read the relevant crate README before
  changing these behaviors.
- Add regression coverage for behavior changes. Rust integration tests live in
  crate or app `tests/` directories; Deno tests live in component `test/`
  directories and use `Deno.test` with `node:assert/strict`.
- Give async tests bounded waits and clean up processes, machines, and temporary
  files even when assertions fail.
- Use `make setup` to install dependencies with the lockfiles. For code changes,
  run `make check`, `make test`, and `make build`, matching CI. Changes that
  need real Firecracker VMs also require `make vm-start vm-smoke` and
  `make vm-test` on a supported host. Report any checks you could not run.

## README maintenance

Keep the root README focused on what the project does, how to get started, and
where to find detailed documentation.

Do not update the README for every feature, fix, refactor, or internal change.
Change it only when the project's purpose, prerequisites, getting-started steps,
or documentation links materially change, or when the user explicitly requests
it. Put component-specific details in the relevant component docs and change
summaries in commit messages or PR descriptions.
