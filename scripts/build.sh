#!/usr/bin/env bash
# Builds the naru binary (dev profile) into target/debug/naru — standalone, with
# no external framework, container, or checkout step.
#
# A plain `cargo build`: it needs a Rust toolchain (https://rustup.rs) and the
# host build deps (run scripts/install-deps.sh once). scripts/install.sh calls
# this automatically when the binary is missing, so a plain `./scripts/install.sh`
# builds and installs on its own.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if ! command -v cargo >/dev/null 2>&1; then
    echo "ERROR: cargo not found. Install the Rust toolchain from https://rustup.rs," >&2
    echo "       and the build deps with scripts/install-deps.sh, then re-run." >&2
    exit 1
fi

echo "Building naru (cargo, dev profile)…"
cargo build --bin naru

echo "Built target/debug/naru"
