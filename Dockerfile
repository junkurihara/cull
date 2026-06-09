# syntax=docker/dockerfile:1

# ---- builder ----------------------------------------------------------------
# Full (non-slim) image so linking always works; the runtime image is separate
# so this layer's size does not matter. Pinned to the toolchain used in dev.
FROM rust:1.96-bookworm AS builder
WORKDIR /build

# The SPA is embedded via include_str! at compile time, so static/ must be
# present in the build context. Manifests are copied first to leverage layer
# caching for dependency compilation.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static

# Build only the server binary (gen_fixtures is a dev tool, not shipped).
RUN cargo build --release --bin triage-tool

# ---- runtime ----------------------------------------------------------------
# distroless/cc provides glibc + libgcc for the dynamically linked binary, with
# no shell or package manager (minimal attack surface). It runs as whatever uid
# is set; we use 1000 to match ComfyUI so rename(2) into keep/trash has write
# permission on the bind-mounted output volume (design.md §9).
FROM gcr.io/distroless/cc-debian12 AS runtime

COPY --from=builder /build/target/release/triage-tool /usr/local/bin/triage-tool

# Defaults; override at run time as needed (design.md §13).
ENV SOURCE_DIR=/srv/enc/warm/comfyui/output \
    BIND_ADDR=0.0.0.0:8080 \
    ORDER=asc \
    RUST_LOG=info

EXPOSE 8080
USER 1000:1000

ENTRYPOINT ["/usr/local/bin/triage-tool"]

# Run (single-FS requirement: keep/trash live under the mounted output volume):
#
#   docker run --rm \
#     -p 8080:8080 \
#     -v /srv/enc/warm/comfyui/output:/srv/enc/warm/comfyui/output:rw \
#     --user 1000:1000 \
#     --security-opt no-new-privileges \
#     --cap-drop ALL \
#     triage-tool
#
# Authentication is terminated upstream (rpxy / mTLS); the app listens plain
# HTTP inside the docker network and implements no auth of its own (§9).
