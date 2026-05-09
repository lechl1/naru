#!/usr/bin/env bash
# Installs the system dev packages required to build naru on Debian/Ubuntu.
# Run with: sudo ./scripts/install-deps.sh
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "Re-running with sudo..."
    exec sudo --preserve-env=PATH "$0" "$@"
fi

PACKAGES=(
    libwayland-dev
    libinput-dev
    libseat-dev
    libgbm-dev
    libpipewire-0.3-dev
    libegl-dev
    libgles-dev
    libdisplay-info-dev
    libxkbcommon-dev
    libdrm-dev
    libpango1.0-dev
    libgtk-4-dev
    libadwaita-1-dev
    pkg-config
    build-essential
    clang
    libclang-dev
)

apt-get update
apt-get install -y "${PACKAGES[@]}"

echo
echo "Verifying pkg-config can find each library..."
PROBES=(
    wayland-server wayland-client libinput libseat gbm libpipewire-0.3
    egl glesv2 libdisplay-info xkbcommon libdrm pango pangocairo
    gtk4 libadwaita-1
)
fail=0
for p in "${PROBES[@]}"; do
    if pkg-config --exists "$p" 2>/dev/null; then
        printf '  %-20s OK\n' "$p"
    else
        printf '  %-20s MISSING\n' "$p"
        fail=1
    fi
done

if [[ $fail -ne 0 ]]; then
    echo
    echo "Some libraries still missing. Check distro package names."
    exit 1
fi
echo
echo "All build dependencies present. You can now run: cargo build --release"
