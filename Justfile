# naru — common dev tasks. Run `just` for a list.
#
# Recipes:
#   just setup    — install build dependencies (auto-detects apt / dnf / pacman / apk)
#   just build    — release build of the naru binary
#   just test     — run the workspace test suite (excludes naru-visual-tests)
#   just install  — build and install system-wide via scripts/install.sh

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

# Release build.
build:
    cargo build --release --bin naru

# Run the workspace test suite. Excludes naru-visual-tests (development-only).
test:
    cargo test --workspace --exclude naru-visual-tests

# Install system-wide. Requires a prior `just build`; runs scripts/install.sh under sudo.
install: build
    sudo ./scripts/install.sh
