# Machine Slop

A computer for agents.

Rust owns the system runtime. TypeScript owns browser integration and client tooling.
Performance is measured by task completion time and total cost at comparable correctness.

## Development

Install Rust through rustup, Node.js (see `.node-version`), pnpm (see `packageManager`
in `package.json`), and Make. The Rust toolchain is pinned in `rust-toolchain.toml`.

```sh
make setup
make check
make build
```

`make format` formats the repository. `make check` checks formatting, runs Clippy,
and type-checks TypeScript. CI runs checks and builds on Linux.

## Run

```sh
make dev
```

The daemon listens on `127.0.0.1:3000`. Pass a socket address to override it:

```sh
cargo run -p agentd -- 127.0.0.1:4000
curl --fail http://127.0.0.1:4000/health
```

The health endpoint returns plain-text `ok`. After `make build`, the TypeScript
client can call it:

```sh
node --input-type=module -e 'import { Machine } from "./packages/sdk/dist/mod.js"; console.log(await new Machine("http://127.0.0.1:3000").health());'
```

This is a development scaffold. It has no authentication or workload isolation;
keep it bound to loopback.

## Structure

```text
apps/agentd/      Rust daemon and application composition
packages/sdk/    TypeScript client; consumers use package exports
```

Domain crates under `crates/`, the TypeScript browser worker under `apps/`, Linux
images, and benchmarks will be added with their first implementations. Packages
own their types and behavior, expose explicit public interfaces, and receive
dependencies through constructors. Applications compose them. A package boundary
does not require a process boundary.
