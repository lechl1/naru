#!/usr/bin/env bash
# Installs the naru binary and session resources system-wide on Debian/Ubuntu.
# Run with: sudo ./scripts/install.sh
#
# This makes naru appear as a Wayland session option in your display manager.
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "Re-running with sudo..."
    exec sudo --preserve-env=PATH "$0" "$@"
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO_ROOT/target/release/naru"

if [[ ! -x "$BIN" ]]; then
    echo "ERROR: $BIN not found. Build first with:"
    echo "  cargo build --release --bin naru"
    exit 1
fi

echo "Installing naru from $REPO_ROOT ..."

install -Dm755 "$BIN"                                  /usr/bin/naru
install -Dm755 "$REPO_ROOT/resources/naru-session"     /usr/bin/naru-session
install -Dm644 "$REPO_ROOT/resources/naru.desktop"     /usr/share/wayland-sessions/naru.desktop
install -Dm644 "$REPO_ROOT/resources/naru-portals.conf" /usr/share/xdg-desktop-portal/naru-portals.conf
install -Dm644 "$REPO_ROOT/resources/naru.service"     /usr/lib/systemd/user/naru.service
install -Dm644 "$REPO_ROOT/resources/naru-shutdown.target" /usr/lib/systemd/user/naru-shutdown.target

echo
echo "Installed:"
ls -l /usr/bin/naru /usr/bin/naru-session 2>/dev/null
ls -l /usr/share/wayland-sessions/naru.desktop /usr/share/xdg-desktop-portal/naru-portals.conf 2>/dev/null
ls -l /usr/lib/systemd/user/naru.service /usr/lib/systemd/user/naru-shutdown.target 2>/dev/null

echo
echo "Done. Log out and pick 'naru' from your display manager session menu to try it."
echo "To uninstall: sudo rm /usr/bin/naru /usr/bin/naru-session \\"
echo "                     /usr/share/wayland-sessions/naru.desktop \\"
echo "                     /usr/share/xdg-desktop-portal/naru-portals.conf \\"
echo "                     /usr/lib/systemd/user/naru.service \\"
echo "                     /usr/lib/systemd/user/naru-shutdown.target"
