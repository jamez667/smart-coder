#!/usr/bin/env bash
# Local CI check: run the same gates a contributor's change must pass before it
# lands. Mirror this in any self-hosted CI runner (see CONTRIBUTING.md).
#
# Usage:  ./scripts/check.sh
# Exits non-zero on the first failing gate.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "==> rustfmt (check)"
cargo fmt --all -- --check

echo "==> clippy (deny warnings)"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> build"
cargo check --workspace

echo "==> tests"
cargo test --workspace

# Spec drift (spec 17): anchors that no longer resolve, assertions that are false.
# Deterministic and model-free, so it costs nothing to run every time. `unknown`
# never gates and an ungoverned crate only warns — this fails on BROKEN or STALE.
echo "==> spec traceability"
cargo run --quiet -p sc-cli -- trace --check

echo "All checks passed."
