#!/usr/bin/env bash
# Builds the naru binary — standalone, with no external framework, container,
# or checkout step.
#
# A plain `cargo build`: it needs a Rust toolchain (https://rustup.rs) and the
# host build deps (run scripts/install-deps.sh once). scripts/install.sh calls
# this automatically when the binary is missing, so a plain `./scripts/install.sh`
# builds and installs on its own.
#
# Profile:
#   RELEASE unset (default) → dev profile  → target/debug/naru
#   RELEASE=1               → release      → target/release/naru
#
# The dev profile stays the default because that's what you want when iterating
# locally. RELEASE=1 is set by anything building naru to *install* it — an
# unoptimized naru is ~700MB and slow, versus ~140MB optimized, so shipping a
# debug binary to /usr/bin is not a tradeoff anyone wants.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if ! command -v cargo >/dev/null 2>&1; then
    echo "ERROR: cargo not found. Install the Rust toolchain from https://rustup.rs," >&2
    echo "       and the build deps with scripts/install-deps.sh, then re-run." >&2
    exit 1
fi

if [[ -n "${RELEASE:-}" && "${RELEASE}" != "0" ]]; then
    echo "Building naru (cargo, release profile)…"
    cargo build --release --bin naru
    echo "Built target/release/naru"
else
    echo "Building naru (cargo, dev profile)…"
    cargo build --bin naru
    echo "Built target/debug/naru"
fi
