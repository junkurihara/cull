# cull

Fullscreen triage for a directory of images. Displays generated images one at a
time and moves the chosen ones into `keep` / `trash` via `rename(2)`. Rust +
axum, single binary with an embedded single-page UI.

The design goal is **O(1) work per image**: the server keeps no in-memory image
list. Every `next` is a fresh, sort-free single pass over the source tree, so
cost scales with the unprocessed backlog (which shrinks as images are moved
out), not with total generations. See `.tmp/design.md` for the full spec.

## Build & run

```sh
cargo build --release --bin cull
SOURCE_DIR=/path/to/output ./target/release/cull
# open http://localhost:8080
```

### Docker

```sh
docker build -t cull .
docker run --rm \
  -p 8080:8080 \
  -v /data/images:/data/images:rw \
  --user 1000:1000 \
  --security-opt no-new-privileges \
  --cap-drop ALL \
  cull
```

Or with Docker Compose (hardened: `user 1000:1000`, `cap_drop ALL`,
`no-new-privileges`, read-only rootfs):

```sh
IMAGE_DIR=/data/images docker compose up --build
```

`keep`/`trash` must sit on the **same filesystem** as the source (move is
`rename(2)`; a cross-device move is reported as a misconfiguration, never copied).
By default they are `$SOURCE_DIR/keep` and `$SOURCE_DIR/trash`, so mounting the
output volume read-write is sufficient.

Authentication is terminated upstream by a TLS-terminating reverse proxy; the app listens plain
HTTP and implements no auth of its own.

## Controls

| Action | Keyboard | Touch |
| --- | --- | --- |
| keep  | →         | swipe right |
| trash | ←         | swipe left  |
| skip  | ↓         | swipe down  |
| undo  | Backspace | swipe up    |
| meta  | `i`       | tap         |
| zoom  | `+` / `-` / `0` | pinch; tap to reset |
| help  | `?`       | `?` button  |
| keep gallery | `g` | `▦` button |

The status bar shows the remaining backlog plus today's totals (`✓` kept /
`✗` trashed). Totals are counted server-side per calendar day, so phone and
desktop sessions add up; they are in-memory only and reset when the container
restarts. Set `TZ_OFFSET_HOURS` so "today" rolls over at your local midnight.

`skip` is client-only: it steps past the current image without changing server
state, so it reappears after a refresh. `undo` is a bounded, in-memory "take
back the last move" stack (not a journal); it fails with HTTP 409 once the stack
is empty or the moved file has been reclaimed externally.

### Keep gallery

The gallery (`g` or the `▦` button, hash-routed as `#keep` so the phone back
button closes it) shows everything under `KEEP_DIR` as a thumbnail grid, newest
first. Tap a thumbnail to view it full size, then **restore** it to the source
tree for re-triage or demote it to **trash**. Gallery moves adjust the daily
totals but are not undoable via the main undo stack — the reverse of a restore
is simply keeping the image again. Thumbnails (~320px JPEG) are generated on
demand and cached in memory (32 MiB LRU); nothing is written to disk.

## Configuration (environment)

| Variable | Default | Purpose |
| --- | --- | --- |
| `SOURCE_DIR`  | `/data/images` | Read root (walked recursively; keep/trash pruned) |
| `KEEP_DIR`    | `$SOURCE_DIR/keep`  | keep destination |
| `TRASH_DIR`   | `$SOURCE_DIR/trash` | trash destination |
| `ORDER`       | `asc`               | `asc` = oldest first, `desc` = newest first |
| `EXTENSIONS`  | `png,jpg,jpeg,webp` | image extensions to enumerate |
| `UNDO_DEPTH`  | `50`                | max undo stack depth |
| `BIND_ADDR`   | `0.0.0.0:8080`      | listen address |
| `TZ_OFFSET_HOURS` | `0`             | UTC offset (whole hours) for the daily-stats day boundary, e.g. `9` for JST |

## API

- `GET  /api/next?after=<relpath>` — next relpath, or 204 when drained
- `GET  /api/image/<relpath>` — image bytes
- `GET  /api/meta/<relpath>` — formatted prompt JSON + best-effort positive/negative
- `POST /api/keep` `{ "relpath": ... }` — move to keep
- `POST /api/trash` `{ "relpath": ... }` — move to trash
- `POST /api/undo` — undo the last move (409 if not possible)
- `GET  /api/count` — approximate backlog size
- `GET  /api/stats` — today's keep/trash totals (also echoed in move/undo responses)
- `GET  /api/keep/list?after=<relpath>&limit=<n>` — kept images, newest first (page ≤ 200)
- `GET  /api/keep/image/<relpath>` / `GET /api/keep/thumb/<relpath>` — kept image / 320px JPEG thumb
- `POST /api/keep/restore` `{ "relpath": ... }` — move back to source for re-triage
- `POST /api/keep/trash` `{ "relpath": ... }` — move a kept image to trash

## Development

```sh
cargo test                       # unit + http + e2e
cargo run --bin gen_fixtures     # write synthetic fixtures into .tmp/fixtures/
SOURCE_DIR=$PWD/.tmp/fixtures/output cargo run --bin cull
```

Metadata extraction targets PNG `tEXt` `prompt` chunks; non-PNG inputs simply
return empty metadata. The graph walk (positive/negative prompts) is
best-effort and degrades silently to raw JSON on any structural mismatch.
