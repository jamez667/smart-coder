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

# ---- runtime stage: a slim image with the two binaries + a verify toolchain ----
# Must match the builder's Debian release so glibc matches (rust:1-slim is
# trixie-based; a bookworm runtime is glibc-too-old and the binary won't load).
FROM debian:trixie-slim
# ca-certificates: HTTPS for the ureq backend when the model URL is remote.
#
# NO language toolchains are installed here on purpose. Verification of a delegated
# task happens on the HOST (by the caller — Claude Code), not in this container:
#   • the host already has cargo / python / node and the LICENSED Unity Editor;
#   • Unity C# can only be compiled by that Editor, which can't run headless here;
#   • keeping the image slim avoids a per-language, drift-prone duplicate of the
#     host toolchain.
# The delegated agent's job is to EDIT; the caller runs the real verify afterwards
# and re-delegates with the failure if needed. See DC_MCP_NO_VERIFY below.
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
