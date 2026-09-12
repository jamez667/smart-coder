#!/usr/bin/env bash
# Local CI check: run the same gates a contributor's change must pass before it
# lands. Mirror this in any self-hosted CI runner (see CONTRIBUTING.md).
#
# Usage:  ./scripts/check.sh
# Exits non-zero on the first failing gate.
set -euo pipefail

cd "$(dirname "$0")/.."

# The browser interface, first — `include_str!` reads its output at compile
# time, so nothing below this compiles without it.
#
# **Skipped rather than fatal when the bundle is already there and npm is not.**
# This gate was offline and deterministic before the interface existed, and a
# gate that suddenly needs a toolchain is a gate people stop running. CI always
# builds it; here, a developer touching only Rust is not made to install Node.
if [ ! -f crates/sc-server/assets/ui/app.js ] || [ -n "${SC_BUILD_WEB:-}" ]; then
  echo "==> the interface (vite)"
  if command -v npm >/dev/null 2>&1; then
    ( cd web && npm ci --silent && npm run lint && npm run build )
  else
    echo "npm is not installed, and crates/sc-server/assets/ui/app.js is missing."
    echo "The server cannot compile without it. Install Node 22, or fetch a build."
    exit 1
  fi
else
  echo "==> the interface (already built; SC_BUILD_WEB=1 to rebuild)"
fi

# ---------------------------------------------------------------------------
# The slow half, started FIRST and collected at the end.
#
# Five suites plus the two report steps hold ~77s of the ~119s this gate spends
# running tests: spec traceability walks every spec and every crate, the retrieval
# eval ranks against the real repository, the compliance authoring tests build
# framework packs, and so on. None of them is slow by accident -- each does real
# work over the real tree, which is exactly why they catch things. Spec anchors
# broke today and this is what noticed.
#
# So they are not dropped, they are OVERLAPPED. They start here, run while
# rustfmt/clippy/check and the fast tests do their thing, and are waited on at the
# bottom. The gate reports every failure it always did; it just stops making you
# wait for the slow ones in series.
#
# A failure is still fatal -- see the wait at the end.
SLOW_LOG="$(mktemp -t sc-check-slow.XXXXXX)"
trap 'rm -f "$SLOW_LOG"' EXIT
echo "==> slow suites (started in the background)"
(
  set +e
  {
    cargo test --quiet -p sc-eval -p sc-trace -p sc-comply-author -p sc-cli || exit 1
    # The gateway benchmark: does routing a plain-language need reach the right
    # capability? Model-free and deterministic -- routing is a pure function of the
    # need text and the capability table -- so a vocabulary change that starts
    # misrouting fails the build with the need named.
    echo
    echo "--- gateway benchmark ---"
    cargo test --quiet -p sc-gateway --test bench -- --nocapture || exit 1
    # Spec drift (spec 17): anchors that no longer resolve, assertions that are
    # false. Deterministic and model-free. `unknown` never gates and an ungoverned
    # crate only warns -- this fails on BROKEN or STALE.
    echo
    echo "--- spec traceability ---"
    cargo run --quiet -p sc-cli -- trace --check || exit 1
  } > "$SLOW_LOG" 2>&1
  echo "$?" > "$SLOW_LOG.status"
) &
SLOW_PID=$!

echo "==> rustfmt (check)"
cargo fmt --all -- --check

echo "==> clippy (deny warnings)"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> build"
cargo check --workspace

echo "==> tests (the fast half; the slow suites are running in parallel)"
cargo test --workspace   --exclude sc-eval --exclude sc-trace --exclude sc-comply-author --exclude sc-cli

# THE CRAFTER'S GUARANTEE (spec 21).
#
# `smart-coder-crafter` is an editor that cannot contact a language model, and the whole
# of that claim is its dependency tree: no crate that can reach a model is in it. This
# check is what keeps the claim true, and it replaced ~1,600 lines of tests that asserted
# each individual refusal at runtime -- a test proves a path was refused on the day it
# ran, a dependency tree proves the path does not exist.
#
# It fails the moment someone adds a model crate to `sc-crafter` or `sc-craft-ui`. That
# mistake is otherwise SILENT: the Crafter would still compile, still run, still look
# right, and no longer be what it says it is.
# Asserted against `sc-craft-ui` rather than the Crafter binary: the binary is step 6
# of spec 25's migration and does not exist yet, and the rule is really about this
# crate anyway -- it is the widest part of the editor's tree, and the one a model crate
# would most plausibly be added to.
echo "==> the crafter links no model code"
CRAFTER_TREE="$(cargo tree -p sc-craft-ui --prefix none --no-dedupe)"
# Match the crate NAME at the start of a line, so a path containing the string (or a
# crate that merely mentions one) cannot trip this.
FORBIDDEN="$(echo "$CRAFTER_TREE" | awk '{print $1}' | sort -u | grep -E     '^(sc-core|sc-model|sc-swarm|sc-workflow|sc-iterate|sc-verify|sc-web|sc-proto|sc-comply|sc-tools)$' || true)"
if [ -n "$FORBIDDEN" ]; then
    echo "the editor must not link model code, but its tree contains:" >&2
    echo "$FORBIDDEN" >&2
    echo "See spec 21 -- the editor half belongs in sc-craft-ui." >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Collect the slow half.
#
# Its output is printed in full rather than summarised: the retrieval eval and the
# spec-traceability report are meant to be READ, not merely passed. A score nobody
# sees is a score nobody watches.
echo "==> slow suites (waiting)"
wait "$SLOW_PID" || true
cat "$SLOW_LOG"
SLOW_STATUS="$(cat "$SLOW_LOG.status" 2>/dev/null || echo 1)"
rm -f "$SLOW_LOG.status"
if [ "$SLOW_STATUS" != "0" ]; then
  echo "the slow suites failed -- see the output above" >&2
  exit 1
fi

echo "All checks passed."
