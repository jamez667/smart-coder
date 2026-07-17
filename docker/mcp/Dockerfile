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

# ---- runtime stage: binaries + the Rust toolchain (Rust only) ----
# Must match the builder's Debian release so glibc matches (rust:1-slim is
# trixie-based; a bookworm runtime is glibc-too-old and the binary won't load).
FROM debian:trixie-slim
# The MCP always runs the STAGED decomposition engine (`dumb-coder staged`), which gates
# each stage on a per-stage `cargo check` that runs IN THIS container. So the container
# needs a WORKING Rust toolchain — but ONLY Rust (these projects are all Rust; no Go/JDK/
# Node/Python). Two hard-won constraints:
#   • It must be a CURRENT rustc, not Debian's apt package (trixie ships 1.85, too old for
#     modern deps like time 0.3.47 which needs 1.88 — that was the original verify failure).
#     So we COPY the rust:1-slim build stage's rustup toolchain (rustc 1.97.x) wholesale
#     rather than `apt install rustc cargo`.
#   • cargo needs a linker → gcc, plus libc6-dev for the C startup objects (crt*.o) and
#     the standard libs (-lc/-lm/-lpthread/…) the linker resolves — gcc alone under
#     --no-install-recommends does NOT pull these, and linking fails without them.
#     ca-certificates stays for the ureq backend's HTTPS.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates gcc libc6-dev \
    && rm -rf /var/lib/apt/lists/*

# The current Rust toolchain, copied from the build stage (rustup layout: RUSTUP_HOME +
# CARGO_HOME, with cargo/rustc as bins under CARGO_HOME). No network, no apt, no drift.
COPY --from=build /usr/local/rustup /usr/local/rustup
COPY --from=build /usr/local/cargo  /usr/local/cargo

COPY --from=build /src/target/release/dumb-coder /usr/local/bin/dumb-coder
COPY --from=build /src/target/release/dumb-coder-mcp /usr/local/bin/dumb-coder-mcp

# The MCP server resolves these from the environment (see dc-mcp::Config). Defaults
# suit "llama.cpp on the host, this container reaching it via host.docker.internal".
# Override -e at `docker run` time for a different rig.
# RUSTUP_HOME/CARGO_HOME/PATH make the copied toolchain usable; CARGO_TARGET_DIR points
# build artifacts at a mountable cache volume (see .mcp.json) so stages don't cold-rebuild
# the world every run, and keeps them OFF the bind-mounted /workspace/target (host triple).
ENV DC_BASE_URL=http://host.docker.internal:11435/v1 \
    DC_MODEL=qwen3-coder-30b \
    DC_MCP_BINARY=dumb-coder \
    DC_MCP_WORKSPACE=/workspace \
    DC_MCP_YOLO=1 \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    CARGO_TARGET_DIR=/target-cache \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

WORKDIR /workspace
# stdio server: no ports, no daemon. The client (Claude Code) drives it over the
# pipe that `docker run -i` connects to.
ENTRYPOINT ["dumb-coder-mcp"]
