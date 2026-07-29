# Neovim remote file actor

`neovim-files` is the backend half of the local Component Neovim plugin. It
owns workspace filesystem authority while the editor and its UI remain in the
presentation client. Session init binds one exact, host-authenticated
`presentation:<actor>` identity over the same-realm local lane. Other
presentation actors are denied even when they belong to the same session.
Replies use `send-presentation-message` to the bound actor; a request cannot
choose its reply target.

Session init starts it as the `neovim-files` child with workspace-write and
presentation-messaging capabilities. Once the client runtime is ready, init
sends `bind-client` from its host-authenticated local identity. The file actor
returns `editor-client-authorized` directly to that presentation actor; the
client acknowledges `editor-files-ready` to init before init forwards queued
opens. This three-step handshake avoids depending on delivery order between
different actors. Client stop and replacement send `revoke-client`, which
removes its snapshots and transfers and cooperatively cancels its workers.

The protocol is JSON with `kind` as its tag. All request/response IDs are
unsigned 64-bit integers. Paths are normalized workspace-relative UTF-8 paths;
empty paths, absolute paths, control bytes, `.` components, and `..`
components are rejected.

Reads use a dedicated read-only host operation and accept only an existing
regular file. Final symlinks, directories, FIFOs, sockets, devices, and other
special targets are rejected. This remains true even though the actor's grant
also permits later atomic replacement, so mode-0444 files can still be opened
without granting the read operation write access.

## Wire protocol

An authorized client may query the negotiated bounds with:

```json
{"kind":"hello","request":1}
```

The `hello` reply carries `version`, `max_file_bytes`, `max_chunk_bytes`,
`max_open_files`, and `max_transfers`. Protocol version 1 uses an 8 MiB file
ceiling, four open snapshots, four active save transfers, and raw chunks no
larger than 7168 bytes. Chunk bytes are lower-case hex in JSON so the complete
cross-placement record remains below the 16 KiB shared lane.

Open a consistent backend snapshot with:

```json
{"kind":"open","request":2,"path":"src/lib.rs"}
```

The `open` reply contains `request`, `path`, the SHA-256 `revision`, total
`size`, `offset: 0`, `data_hex`, and `eof`. Continue through that immutable
snapshot with:

```json
{"kind":"read","request":2,"path":"src/lib.rs","offset":7168}
```

Its `read` reply has the same file/chunk fields. Offsets are byte offsets. The
client reuses the open request ID for every chunk; active request IDs are
unique per authenticated presentation actor. The backend releases the
snapshot immediately after returning its EOF chunk. Reads must name the same
path and exact next offset. A malformed, wrong-path, or out-of-order read is
terminal for that snapshot and releases its bounded slot. A client that stops
an otherwise valid non-EOF read sends:

```json
{"kind":"open-abort","request":2}
```

This also cancels an open worker if the snapshot has not completed yet.

Saving is a bounded, sequential transfer:

```json
{"kind":"save-begin","request":4,"transfer":9,"path":"src/lib.rs","base_revision":"<64 hex digits>","size":12345}
{"kind":"save-chunk","request":4,"transfer":9,"offset":0,"data_hex":"..."}
{"kind":"save-commit","request":4,"transfer":9}
```

`save-ready` acknowledges the declaration. Every contiguous chunk receives a
`save-chunk-accepted` reply containing `next_offset`. Commit rereads the
backend file and compares its SHA-256 revision with `base_revision`. Commits
for the same normalized path are serialized. Once a commit changes the
revision, a later save based on the prior revision cannot pass that check
inside this actor. A mismatch returns an `error` with `code: "conflict"` and
`current_revision`. A matching file is replaced through the worker-only
whole-file host primitive and returns `save-complete` with the new revision
and size.

A client-side validation failure terminates a live save with:

```json
{"kind":"save-abort","request":4,"transfer":9}
```

An abort removes an assembling transfer. If its commit worker is already
running, the actor cancels that worker and retains the per-path lock until the
worker reaches a terminal event.

Malformed chunks, changed request IDs, incomplete commits, and `path-busy`
commits terminate the transfer and release its bounded slot. The client starts
a new save after a terminal error; once the active commit finishes, that new
save either proceeds or observes its new revision as a conflict.

The compare and replacement are not a filesystem-wide compare-and-swap.
Another process with workspace write access can still modify the path between
the actor's final reread and rename. The replacement itself is atomic, so
readers never observe a partially written file, but coordinating external
writers requires a higher-level workspace lock.

Every error contains `kind: "error"`, `request`, `code`, and `message`.
Transfer errors also contain `transfer`; conflicts additionally contain
`current_revision`.
