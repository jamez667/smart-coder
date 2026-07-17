# dumb-coder MCP server (spec 06 machine surface), containerized.
#
# Runs `dumb-coder-mcp` as an MCP server over stdio. Claude Code launches it with:
#   docker run -i --rm \
#     -v <host-repo>:/workspace \
#     -e DC_BASE_URL=http://host.docker.internal:11435/v1 \
#     dumb-coder-mcp
# stdin/stdout are the JSON-RPC channel; the host repo is bind-mounted at
# /workspace, which is the default workspace the `code` tool edits.
#
# The MCP server spawns the `dumb-coder` binary (also built here) as a subprocess
# for each job. Both binaries are Linux — inside the container there is no Windows
# path juggling. The model backend (llama.cpp) runs in its OWN container; this one
# reaches it over the network via DC_BASE_URL (host.docker.internal by default).

# ---- build stage: compile both binaries against the pinned workspace ----
FROM rust:1-slim AS build
WORKDIR /src
# Copy the whole workspace; a release build of just the two bins we ship.
COPY . .
RUN cargo build --release -p dc-cli -p dc-mcp

# ---- runtime stage: a slim image with just the two binaries ----
# Must match the builder's Debian release so glibc matches (rust:1-slim is
# trixie-based; a bookworm runtime is glibc-too-old and the binary won't load).
FROM debian:trixie-slim
# NO language toolchains. The container is an EDIT-ONLY surface: the delegated agent
# reads and edits the workspace, then finishes on "edited"; the CALLER (host / planning
# agent) verifies against its own correct, warm toolchain. We deliberately do NOT ship
# rustc/cargo/go/jdk/etc. here because:
#   • The distro toolchain lags the host's — a modern Rust workspace needs a rustc newer
#     than Debian ships, so an in-container `cargo test` fails on the TOOLCHAIN VERSION,
#     not the edit: a false red the caller then has to untangle.
#   • Cold in-container builds re-download + recompile the whole dep tree every run
#     (minutes on a big workspace), for a result the host would produce warm anyway.
#   • With no cargo present the model can't shell out to a build even under --yolo, so it
#     stops burning turns fighting a toolchain that will never match the host.
# Only ca-certificates remains — HTTPS for the ureq backend that reaches the model.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/dumb-coder /usr/local/bin/dumb-coder
COPY --from=build /src/target/release/dumb-coder-mcp /usr/local/bin/dumb-coder-mcp

# The MCP server resolves these from the environment (see dc-mcp::Config). Defaults
# suit "llama.cpp on the host, this container reaching it via host.docker.internal".
# Override -e at `docker run` time for a different rig.
ENV DC_BASE_URL=http://host.docker.internal:11435/v1 \
    DC_MODEL=qwen3-coder-30b \
    DC_MCP_BINARY=dumb-coder \
    DC_MCP_WORKSPACE=/workspace \
    DC_MCP_YOLO=1

WORKDIR /workspace
# stdio server: no ports, no daemon. The client (Claude Code) drives it over the
# pipe that `docker run -i` connects to.
ENTRYPOINT ["dumb-coder-mcp"]
