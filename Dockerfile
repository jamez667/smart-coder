# The hosted intake surface (sc-server, spec 18) as a standalone image.
#
# Installed in Portainer independently of everything else in this workspace: it
# is one binary, one port and one volume, with no dependency on the desktop
# client, the daemon, or a model. The daemon dials *out* to it, so the developer's
# machine needs no inbound port and this image needs no way back.
#
#   docker build -t sc-server .
#   docker run -p 8420:8420 -v sc-server-data:/data \
#     -e SC_SERVER_DAEMON_KEY=$(openssl rand -hex 32) sc-server
#
# Or in a Portainer stack, see deploy/sc-server.stack.yml.

# ---------------------------------------------------------------------------
# Build — static against musl, so the runtime stage needs no libc at all.
# ---------------------------------------------------------------------------
FROM rust:1.97.1-alpine AS build

RUN apk add --no-cache musl-dev

WORKDIR /src

# The whole workspace is the build context, but only one crate is *compiled*:
# `-p sc-server` builds its dependency tree and nothing else, so the desktop
# client, the workflow engine and the model stack are never touched. The image is
# small because of `sc-server`'s dependencies — it takes only `sc-proto` — not
# because of where the code lives.
COPY . .

RUN cargo build --release -p sc-server --bin sc-server \
    && strip target/release/sc-server

# ---------------------------------------------------------------------------
# Runtime — nothing but the binary.
# ---------------------------------------------------------------------------
#
# `scratch` was tempting and is wrong: the server makes no outbound calls today,
# but a distroless base with CA certificates and a timezone database costs a few
# megabytes and removes a whole category of "why does it not work in the
# container" from the developer's evening.
FROM alpine:3.21

RUN apk add --no-cache ca-certificates tzdata \
    # A fixed uid, so a volume written by one image tag is readable by the next.
    # An image that changes the uid on upgrade greets the developer with
    # permission errors on data that was fine yesterday.
    && addgroup -g 10001 -S sc \
    && adduser -u 10001 -S sc -G sc

COPY --from=build /src/target/release/sc-server /usr/local/bin/sc-server

# The one volume: requests, drafted specs, and credentials. One thing to mount,
# one thing to back up — state split across paths is a footgun, because the
# backup that misses one looks like it worked.
RUN mkdir -p /data && chown sc:sc /data
VOLUME ["/data"]
ENV SC_SERVER_DATA=/data

EXPOSE 8420

# **The binary checks itself.** No curl or wget in this layer: the runtime is
# Alpine plus one static binary, and adding an HTTP client to the one component
# facing the internet — for a check the binary answers in ten lines — is a
# package and its dependencies bought for nothing.
#
# Without a healthcheck Docker reports the container *running* the moment the
# process is spawned, which is before the port is listening; a proxy in front
# then forwards to a socket nobody is on yet and the deploy shows a 502 for a
# second or two. It is also how Swarm decides a new task is ready, so a
# start-first rollout has nothing to wait for without it.
#
# `--start-period` covers the first start, where a cold image opening its data
# directory takes longer than a steady-state check; failures during it do not
# count against the retries.
HEALTHCHECK --interval=10s --timeout=3s --start-period=15s --retries=3 \
    CMD ["/usr/local/bin/sc-server", "--health"]

# Non-root. This is the one process in this system that faces the public
# internet; it holds text and nothing else, and it runs as a user that can write
# exactly one directory.
USER sc:sc

# No TLS in-process: a reverse proxy terminates it, which is what the developer
# already runs in front of everything else. Certificates, renewal and a private
# key inside the container are three failure modes solving a solved problem.
ENTRYPOINT ["/usr/local/bin/sc-server"]
