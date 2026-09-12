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

echo "==> rustfmt (check)"
cargo fmt --all -- --check

echo "==> clippy (deny warnings)"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> build"
cargo check --workspace

# The tests, minus four crates that are slow enough to change behaviour: a gate
# people skip is not a gate. sc-eval ranks retrieval against the whole repository,
# sc-trace walks every spec and every crate, sc-comply-author builds framework
# packs, sc-cli drives the binary end to end -- together about 77 seconds against
# the ~40 everything else takes.
#
# None of them is flaky and none needs a model; they are excluded for TIME alone.
# CI runs the full workspace on every push (.woodpecker/ci.yml), so nothing is
# unwatched -- it is watched after the fact instead of before the commit.
#
# Run them yourself when you touch what they cover:
#     cargo test -p sc-eval -p sc-trace -p sc-comply-author -p sc-cli
echo "==> tests (fast crates; see the note above for the four excluded)"
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

echo "All checks passed."
