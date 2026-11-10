# agentd

`agentd` runs processes, moves files and reaches ports on the machine it runs
on. Process execution is described in
[crates/process](../../crates/process/README.md).

```ts
import { Machine, tar } from "@machine-slop/sdk";

const machine = new Machine("http://127.0.0.1:3000");
await machine.writeArchive(
  "/workspace/app",
  tar({
    "package.json": '{"type":"module"}',
    "server.js": "...",
    "run.sh": { data: "#!/bin/sh\nnode server.js\n", mode: 0o755 },
  }),
);
await machine.startProcess({ program: "./run.sh", cwd: "/workspace/app" });
const forward = machine.forward(8000);
console.log(await (await fetch(forward.url)).text());
await forward.close();
console.log(
  new TextDecoder().decode(await machine.readFile("/workspace/app/out.log")),
);
```

## Files

Paths are absolute machine paths placed after `/files` or `/archives`, one
URL-encoded segment per path segment. A path with an empty, `.` or `..` segment
returns HTTP 400. Bodies are raw bytes with no size limit beyond the disk.

| Request                | Result                                                       |
| ---------------------- | ------------------------------------------------------------ |
| `GET /files/{path}`    | The regular file's bytes, with `Content-Length`              |
| `PUT /files/{path}`    | Replaces the file with the body; 204                         |
| `DELETE /files/{path}` | Removes the file, or the directory and everything in it; 204 |
| `GET /archives/{path}` | A tar stream of the directory's contents                     |
| `PUT /archives/{path}` | Extracts the tar body into the directory; 204                |

`PUT /files` creates missing parent directories and writes the body to a staging
file beside the target, which it renames over the target only after the whole
body arrives. A reader sees the previous file or the new one, never a partial
file, and an upload that fails or is cut off leaves the previous file and no
staging file. Repeating an upload is safe. `?mode=755` sets permission bits;
without it a replaced file keeps its bits and a new file gets `644`. The file is
visible to later requests and processes on the machine; it is not synced to
disk.

`DELETE` returns 204 once nothing is at the path, including when nothing was
there. It removes a symlink, not its target.

`GET /archives` writes entries relative to the directory, archives symlinks as
symlinks, and stores modes and modification times. An error while reading the
directory aborts the response, so the client sees a failed transfer rather than
a short archive. `PUT /archives` creates the directory and merges the archive
into it, overwriting files at the same paths. An entry that would land outside
the directory, through `..` or a symlink, stops extraction with HTTP 422.
Extraction is not atomic: after a failure, entries before the failure remain;
repeating the same archive is safe. `tar(files)` in the SDK builds an archive
from a map of relative paths to contents, so many files take one request.

Errors map from the file system: missing path 404, permission 403, wrong kind of
entry 409 (for example reading a directory as a file), full disk 507.

## Ports

`GET /ports/{port}` with `Connection: upgrade` and `Upgrade: tcp` connects to
the port on the machine's loopback, `127.0.0.1` then `::1`, and answers 101 only
once that connection is open. The HTTP connection then carries raw bytes both
ways, including a half-close, until either side closes. A port with no listener
returns 502, and a request without the upgrade headers 426. `machined` passes
the upgrade through, so this works for hosted machines too.

`machine.connect(port)` returns the tunnel as a readable and writable stream
pair. `machine.forward(port, localPort?)` listens on `127.0.0.1` and opens a
tunnel for each accepted connection, so `forward.url` serves the machine's web
server, websockets included, at its own root. A local connection whose tunnel
cannot open is closed. `forward.close()` stops listening and closes open
tunnels.

This API assumes a trusted machine; it provides no authentication, and file
access runs with `agentd`'s privileges.
