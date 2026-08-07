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

echo "==> tests"
cargo test --workspace

# The CRAFT-ONLY build (spec 21). A cargo feature is only compiled when something
# asks for it, so without these two gates the flag rots silently: nothing in the
# default build would notice a `cfg(feature = "craft-only")` block that stopped
# compiling, or a test whose assumptions the pinned mode invalidates.
echo "==> clippy (craft-only)"
cargo clippy -p sc-win --all-targets --features craft-only -- -D warnings

echo "==> tests (craft-only)"
cargo test -p sc-win --features craft-only

# Spec drift (spec 17): anchors that no longer resolve, assertions that are false.
# Deterministic and model-free, so it costs nothing to run every time. `unknown`
# never gates and an ungoverned crate only warns — this fails on BROKEN or STALE.
echo "==> spec traceability"
cargo run --quiet -p sc-cli -- trace --check

echo "All checks passed."
