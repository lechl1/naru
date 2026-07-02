# naru — common dev tasks. Run `just` for a list.
#
# Recipes:
#   just setup    — install build dependencies (auto-detects apt / dnf / pacman / apk).
#                   Only needed for `just test` and host-side hacking; `just build`
#                   runs inside the Docker builder image so the host does not need
#                   rustup or any of the *-dev packages.
#   just build-base — (re)build the prebuilt apt + rust base image. Run this
#                   after changing the dep set or Rust pin in Dockerfile.base;
#                   `just build` builds it automatically when it is missing.
#   just build    — build the naru binary via docker buildx using Dockerfile.alloy
#                   (FROM the prebuilt base) and extract it into target/release/naru
#   just test     — run the workspace test suite (excludes naru-visual-tests)
#   just install  — build (docker buildx) and install system-wide via scripts/install.sh

set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# Default recipe: list available recipes.
default:
    @just --list

# Install build dependencies. Detects the package manager from /etc/os-release.
# Mirrors the dependency lists pinned in .github/workflows/ci.yml.
setup:
    #!/usr/bin/env bash
    set -euo pipefail

    DEPS_APT="curl gcc clang libudev-dev libgbm-dev libxkbcommon-dev libegl1-mesa-dev libwayland-dev libinput-dev libdbus-1-dev libsystemd-dev libseat-dev libpipewire-0.3-dev libpango1.0-dev libdisplay-info-dev"
    DEPS_DNF="cargo gcc clang libudev-devel libgbm-devel libxkbcommon-devel wayland-devel libinput-devel dbus-devel systemd-devel libseat-devel pipewire-devel pango-devel cairo-gobject-devel libdisplay-info-devel"
    DEPS_APK="cargo clang-libclang eudev-dev glib-dev libdisplay-info-dev libinput-dev libseat-dev libxkbcommon-dev mesa-dev pango-dev pipewire-dev tar"
    DEPS_PACMAN="rust clang pkgconf libudev0-shim libxkbcommon libinput dbus systemd seatd pipewire pango libdisplay-info wayland mesa"

    SUDO=""
    if [[ $EUID -ne 0 ]]; then SUDO="sudo"; fi

    if command -v apt-get >/dev/null 2>&1; then
        $SUDO apt-get update -y
        $SUDO apt-get install -y $DEPS_APT
    elif command -v dnf >/dev/null 2>&1; then
        $SUDO dnf install -y $DEPS_DNF
    elif command -v pacman >/dev/null 2>&1; then
        $SUDO pacman -S --needed --noconfirm $DEPS_PACMAN
    elif command -v apk >/dev/null 2>&1; then
        $SUDO apk add $DEPS_APK
    else
        echo "Unsupported package manager. Install equivalents of:" >&2
        echo "  $DEPS_APT" >&2
        exit 1
    fi

    # Rust toolchain — stable, via rustup if available.
    if ! command -v cargo >/dev/null 2>&1; then
        echo "cargo not found. Install rustup from https://rustup.rs and re-run \`just setup\`." >&2
        exit 1
    fi

# Release build via docker buildx (no host rustup / cargo / *-dev tree
# required). Uses the multi-stage Dockerfile.alloy: the `builder` stage
# installs the full Wayland/graphics toolchain plus a pinned Rust
# toolchain and runs `cargo build --release --bin naru`; the `export`
# stage stages just the binary on `scratch`.
#
# `--output type=local` is written by the docker daemon, not by the user
# invoking `just build`. With rootful Docker (or a rootless daemon with
# uid mapping) that writer can end up not matching the target directory's
# owner, leaving artifacts the next invocation cannot overwrite. To stay
# robust to that, we materialise the export stage into a per-build tmp
# directory and then `install` the binary into target/release/ — same
# path `scripts/install.sh` expects, but with predictable ownership.
# Prebuilt build base: the apt *-dev toolchain + pinned Rust, baked into a
# tagged image so the slow package install survives BuildKit's build-cache GC
# and doesn't re-run on every code build. Rebuild after changing the dep set or
# the Rust pin in Dockerfile.base; `build` below does this automatically when
# the image is missing.
build-base:
    docker buildx build --file Dockerfile.base --tag naru-build-base:26.04 --load .

build:
    #!/usr/bin/env bash
    set -euo pipefail
    # Ensure the prebuilt base exists (apt + rust). Built once, then reused by
    # every code build — this is what keeps the package install off the hot path.
    if ! docker image inspect naru-build-base:26.04 >/dev/null 2>&1; then
        docker buildx build --file Dockerfile.base --tag naru-build-base:26.04 --load .
    fi
    out=$(mktemp -d -t naru-build-XXXXXX)
    trap 'rm -rf "$out"' EXIT
    docker buildx build \
        --target export \
        --file Dockerfile.alloy \
        --output "type=local,dest=$out" \
        .
    mkdir -p target/release
    install -m 0755 "$out/naru" target/release/naru

# Run the workspace test suite. Excludes naru-visual-tests (development-only).
test:
    cargo test --workspace --exclude naru-visual-tests

# scripts/install.sh under sudo drops /usr/bin/naru, the session files, and
# the systemd units in place. It also defaults the Plasma theme to Breeze
# Dark and the display manager to SDDM *only when neither is already set* —
# existing, working choices are never overridden.
#
# The sudo password is prompted up front (before the slow docker build) so
# the install does not stall at a password prompt minutes later. A
# background keep-alive refreshes the sudo timestamp for the duration of the
# build, so the privileged step still runs without re-prompting even if the
# build outlasts sudo's default credential timeout.
#
# Install system-wide: build (docker buildx), then install under sudo.
install:
    #!/usr/bin/env bash
    set -euo pipefail

    # Prompt for / validate the sudo password before doing any slow work.
    sudo -v

    # Keep the sudo timestamp warm until this recipe exits, so the install
    # step below never re-prompts no matter how long the build takes.
    ( while true; do sudo -n true; sleep 50; kill -0 "$$" 2>/dev/null || exit; done ) &
    keepalive=$!
    trap 'kill "$keepalive" 2>/dev/null || true' EXIT

    just build
    sudo ./scripts/install.sh
