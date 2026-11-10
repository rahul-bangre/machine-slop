# Machine Slop

A computer for agents.

Requires rustup, Deno (version in `.tool-versions`), and Make.

```sh
make setup
make check
make build
make dev
```

`agentd` listens on `127.0.0.1:3000`; `GET /health` returns `ok`. `make format`
formats the repository.

## Linux VM

`vm/lima.yaml` defines a Debian 13 development VM for macOS hosts. It needs
[Lima](https://lima-vm.io) 1.1 or later (`brew install lima`).

```sh
make vm-start   # create or boot the VM, rebuild agentd from /workspace, wait for /health
make vm-smoke   # check /health, the /workspace mount, and the CPU and memory limits
make vm-shell   # open a shell in /workspace
make vm-stop    # shut the VM down
make vm-delete  # remove the VM
```

The VM has 4 CPUs, 4 GiB of memory, and a 20 GiB disk. This checkout is mounted
read-write at `/workspace`. `agentd` runs as the `agentd` systemd service on
guest port 3000, forwarded to `http://127.0.0.1:3001/health` on the host. The
base image and apt packages are pinned to one Debian snapshot, and the Rust
toolchain comes from `rust-toolchain.toml`. Guest builds go to `~/target` in the
VM, not `/workspace/target`.

A VM keeps the config it was created with. After `vm/lima.yaml` changes,
`make vm-start` refuses to run the old VM; run `make vm-delete vm-start`.
