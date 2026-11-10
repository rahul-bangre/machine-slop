# Machine Slop

Machine Slop runs shell commands for agents over HTTP, with a TypeScript SDK. It
can run processes directly on a host or inside Firecracker microVMs.

- `agentd` starts processes, streams output, accepts stdin, and reports exits.
- `machined` creates and destroys Linux microVMs, each running `agentd`.

The repo also includes benchmarks for comparing agent sandboxes and browser
observation methods.

## Run locally

Install [rustup](https://rustup.rs/), Deno (version in
[.tool-versions](.tool-versions)), and Make. Rust is pinned in
[rust-toolchain.toml](rust-toolchain.toml).

```sh
git clone https://github.com/rahul-bangre/machine-slop.git
cd machine-slop
make setup
make dev
```

This starts `agentd` at `http://127.0.0.1:3000`. In another terminal:

```sh
curl http://127.0.0.1:3000/health
curl http://127.0.0.1:3000/processes \
  -H 'content-type: application/json' \
  -d '{"program":"/bin/sh","args":["-c","printf hello"]}'
```

The second request returns a process record. See the
[process API and SDK examples](crates/process/README.md) for reading output and
waiting for completion. Commands run as the daemon's user on the host; the API
has no authentication or workload isolation.

## Run microVMs

`machined` needs Linux with `/dev/kvm`. See
[machine setup and usage](crates/machines/README.md) for the host requirements
and SDK example.

On macOS, the included [Lima](https://lima-vm.io) configuration requires Lima
1.1+ and an Apple M3 or later for nested virtualization:

```sh
make vm-start vm-smoke
make vm-test
make vm-shell
make vm-stop
```

The VM forwards `agentd` to `http://127.0.0.1:3001` and `machined` to
`http://127.0.0.1:4001`.

## Development

```sh
make check
make test
make build
```

## Benchmarks

- [Agent bench](bench/agent/README.md): run the same coding tasks across sandbox
  providers.
- [Browser bench](bench/browser/README.md): compare browser resource use and
  observation costs on scripted tasks.
