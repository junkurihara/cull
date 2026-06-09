# triage-tool

Fullscreen image triage for ComfyUI output. Displays generated images one at a
time and moves the chosen ones into `keep` / `trash` via `rename(2)`. Rust +
axum, single binary with an embedded single-page UI.

The design goal is **O(1) work per image**: the server keeps no in-memory image
list. Every `next` is a fresh, sort-free single pass over the source tree, so
cost scales with the unprocessed backlog (which shrinks as images are moved
out), not with total generations. See `.tmp/design.md` for the full spec.

## Build & run

```sh
cargo build --release --bin triage-tool
SOURCE_DIR=/path/to/output ./target/release/triage-tool
# open http://localhost:8080
```

### Docker

```sh
docker build -t triage-tool .
docker run --rm \
  -p 8080:8080 \
  -v /srv/enc/warm/comfyui/output:/srv/enc/warm/comfyui/output:rw \
  --user 1000:1000 \
  --security-opt no-new-privileges \
  --cap-drop ALL \
  triage-tool
```

Or with Docker Compose (hardened: `user 1000:1000`, `cap_drop ALL`,
`no-new-privileges`, read-only rootfs):

```sh
COMFY_OUTPUT=/srv/enc/warm/comfyui/output docker compose up --build
```

`keep`/`trash` must sit on the **same filesystem** as the source (move is
`rename(2)`; a cross-device move is reported as a misconfiguration, never copied).
By default they are `$SOURCE_DIR/keep` and `$SOURCE_DIR/trash`, so mounting the
output volume read-write is sufficient.

Authentication is terminated upstream (e.g. rpxy / mTLS); the app listens plain
HTTP and implements no auth of its own.

## Controls

| Action | Keyboard | Touch |
| --- | --- | --- |
| keep  | →         | swipe right |
| trash | ←         | swipe left  |
| skip  | ↓         | swipe down  |
| undo  | Backspace | swipe up    |
| meta  | `i`       | tap         |

`skip` is client-only: it steps past the current image without changing server
state, so it reappears after a refresh. `undo` is a bounded, in-memory "take
back the last move" stack (not a journal); it fails with HTTP 409 once the stack
is empty or the moved file has been reclaimed externally.

## Configuration (environment)

| Variable | Default | Purpose |
| --- | --- | --- |
| `SOURCE_DIR`  | `/srv/enc/warm/comfyui/output` | Read root (walked recursively; keep/trash pruned) |
| `KEEP_DIR`    | `$SOURCE_DIR/keep`  | keep destination |
| `TRASH_DIR`   | `$SOURCE_DIR/trash` | trash destination |
| `ORDER`       | `asc`               | `asc` = oldest first, `desc` = newest first |
| `EXTENSIONS`  | `png,jpg,jpeg,webp` | image extensions to enumerate |
| `UNDO_DEPTH`  | `50`                | max undo stack depth |
| `BIND_ADDR`   | `0.0.0.0:8080`      | listen address |

## API

- `GET  /api/next?after=<relpath>` — next relpath, or 204 when drained
- `GET  /api/image/<relpath>` — image bytes
- `GET  /api/meta/<relpath>` — formatted prompt JSON + best-effort positive/negative
- `POST /api/keep` `{ "relpath": ... }` — move to keep
- `POST /api/trash` `{ "relpath": ... }` — move to trash
- `POST /api/undo` — undo the last move (409 if not possible)
- `GET  /api/count` — approximate backlog size

## Development

```sh
cargo test                       # unit + http + e2e
cargo run --bin gen_fixtures     # write synthetic fixtures into .tmp/fixtures/
SOURCE_DIR=$PWD/.tmp/fixtures/output cargo run --bin triage-tool
```

Metadata extraction targets PNG `tEXt` `prompt` chunks; non-PNG inputs simply
return empty metadata. The graph walk (positive/negative prompts) is
best-effort and degrades silently to raw JSON on any structural mismatch.
