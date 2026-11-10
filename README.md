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
