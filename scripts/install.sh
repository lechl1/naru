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

# Ensure /etc/xdg/menus/applications.menu exists. On systems set up for
# Plasma only, the menus dir ships plasma-applications.menu but not the
# default applications.menu, so kbuildsycoca warns and the app-menu tree
# does not build under the naru session. Symlink the default name to the
# Plasma file when that is the situation.
MENUS_DIR="/etc/xdg/menus"
if [[ -f "$MENUS_DIR/plasma-applications.menu" && ! -e "$MENUS_DIR/applications.menu" ]]; then
    ln -s plasma-applications.menu "$MENUS_DIR/applications.menu"
    echo "Linked $MENUS_DIR/applications.menu -> plasma-applications.menu"
fi

# The user who invoked `sudo ./install.sh` — per-user config (the Plasma
# theme) belongs to them, not to root.
TARGET_USER="${SUDO_USER:-$USER}"
TARGET_HOME="$(getent passwd "$TARGET_USER" | cut -d: -f6 || true)"

# Default the Plasma global theme to Breeze Dark — but ONLY if the user has
# no valid look-and-feel set. We never override a working choice; we just
# fix the common breakage where the configured package is no longer
# installed (e.g. Kubuntu's org.kubuntudark.desktop, which is gone on newer
# releases). When its package is missing, Plasma/Qt apps launched inside the
# naru session fall back to an unstyled light default, which looks wrong.
WANT_LOOKANDFEEL="org.kde.breezedark.desktop"
if [[ -n "$TARGET_HOME" ]] && command -v plasma-apply-lookandfeel >/dev/null 2>&1; then
    kdeglobals="$TARGET_HOME/.config/kdeglobals"
    current_laf=""
    if [[ -r "$kdeglobals" ]]; then
        # Read LookAndFeelPackage from the [KDE] section specifically.
        current_laf="$(awk -F= '
            /^\[/      { section = $0 }
            section == "[KDE]" && $1 == "LookAndFeelPackage" { print $2; exit }
        ' "$kdeglobals")"
    fi

    laf_installed=false
    if [[ -n "$current_laf" ]] \
        && { [[ -d "/usr/share/plasma/look-and-feel/$current_laf" ]] \
          || [[ -d "$TARGET_HOME/.local/share/plasma/look-and-feel/$current_laf" ]]; }; then
        laf_installed=true
    fi

    if [[ "$laf_installed" == true ]]; then
        echo "Plasma look-and-feel '$current_laf' is set and installed — leaving it."
    elif [[ ! -d "/usr/share/plasma/look-and-feel/$WANT_LOOKANDFEEL" ]]; then
        echo "WARNING: '$WANT_LOOKANDFEEL' is not installed; skipping theme default." >&2
    else
        echo "No valid Plasma look-and-feel set (${current_laf:-unset}); defaulting to $WANT_LOOKANDFEEL."
        # Apply as the target user so it writes their config (and applies
        # live if their Plasma session is running). The XDG/DBus hints let
        # the live apply reach their session bus; if there's no session it
        # still updates kdeglobals for next login.
        uid="$(id -u "$TARGET_USER")"
        runuser -u "$TARGET_USER" -- env \
            XDG_RUNTIME_DIR="/run/user/$uid" \
            DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" \
            plasma-apply-lookandfeel -a "$WANT_LOOKANDFEEL" || true
        runuser -u "$TARGET_USER" -- env \
            XDG_RUNTIME_DIR="/run/user/$uid" \
            DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" \
            plasma-apply-colorscheme BreezeDark || true
    fi
fi

# Install the helpers the default config's media keys depend on. The binds use
# swayosd-client (volume/brightness OSD, started via `spawn-at-startup
# "swayosd-server"`) and playerctl (play/pause/next/prev over MPRIS). Without these
# the XF86Audio* keys silently do nothing — swayosd-client just errors out that the
# server name isn't on the bus. Only install what's missing, and only via apt.
if command -v apt-get >/dev/null 2>&1; then
    media_pkgs=()
    command -v swayosd-server >/dev/null 2>&1 || media_pkgs+=(swayosd)
    command -v playerctl     >/dev/null 2>&1 || media_pkgs+=(playerctl)
    if [[ ${#media_pkgs[@]} -gt 0 ]]; then
        echo "Installing media-key helpers: ${media_pkgs[*]}"
        apt-get update -y
        # Don't abort the whole install if one package isn't in the user's repos
        # (e.g. swayosd is only packaged on newer Ubuntu) — warn and continue.
        apt-get install -y "${media_pkgs[@]}" \
            || echo "WARNING: could not install ${media_pkgs[*]}; media keys may not work until installed." >&2
    else
        echo "Media-key helpers (swayosd, playerctl) already present."
    fi
else
    echo "WARNING: apt-get unavailable; install swayosd + playerctl manually for media keys." >&2
fi

# Default to SDDM as the display manager — but ONLY if none is configured.
# naru is just a Wayland session; if you already log in via GDM, LightDM,
# etc., that's left untouched. We only step in when no display manager is
# enabled at all, so a fresh machine can actually reach a graphical login.
if command -v systemctl >/dev/null 2>&1; then
    if systemctl is-enabled display-manager.service >/dev/null 2>&1; then
        current_dm="$(systemctl show -p Id --value display-manager.service 2>/dev/null || true)"
        echo "Display manager already enabled (${current_dm:-display-manager.service}) — leaving it."
    else
        echo "No display manager enabled; defaulting to SDDM."
        if ! command -v sddm >/dev/null 2>&1; then
            if command -v apt-get >/dev/null 2>&1; then
                apt-get update -y
                apt-get install -y sddm
            else
                echo "WARNING: sddm not installed and apt-get unavailable; skipping DM default." >&2
            fi
        fi
        if command -v sddm >/dev/null 2>&1; then
            systemctl enable sddm.service
            systemctl set-default graphical.target
        fi
    fi
fi

# Default the SDDM login-screen theme to KDE Plasma's default — Breeze — but
# ONLY when SDDM is the active login manager AND no theme is already set.
# Same fill-the-gap philosophy as the display-manager default above: an
# existing, working theme choice (e.g. a distro's own login theme) is never
# overridden.
WANT_SDDM_THEME="breeze"
if command -v systemctl >/dev/null 2>&1 \
    && [[ "$(systemctl show -p Id --value display-manager.service 2>/dev/null || true)" == "sddm.service" ]]; then

    # Find the active [Theme] Current= across SDDM's whole config search path
    # (vendor dir, then /etc/sddm.conf, then /etc/sddm.conf.d — later files
    # win). We only care whether *any* of them sets a non-empty value.
    current_theme=""
    for f in /usr/lib/sddm/sddm.conf.d/*.conf /etc/sddm.conf /etc/sddm.conf.d/*.conf; do
        [[ -r "$f" ]] || continue
        v="$(awk -F= '
            /^[[:space:]]*\[/ { section = $0 }
            section ~ /\[Theme\]/ && $1 ~ /^[[:space:]]*Current[[:space:]]*$/ {
                gsub(/^[[:space:]]+|[[:space:]]+$/, "", $2); val = $2
            }
            END { print val }
        ' "$f")"
        [[ -n "$v" ]] && current_theme="$v"
    done

    if [[ -n "$current_theme" ]] && [[ -d "/usr/share/sddm/themes/$current_theme" ]]; then
        echo "SDDM theme '$current_theme' is set and installed — leaving it."
    elif [[ ! -d "/usr/share/sddm/themes/$WANT_SDDM_THEME" ]]; then
        echo "WARNING: SDDM theme '$WANT_SDDM_THEME' is not installed; skipping login-theme default." >&2
    else
        echo "No SDDM theme set (${current_theme:-unset}); defaulting to $WANT_SDDM_THEME (KDE Plasma default)."
        install -d -m 0755 /etc/sddm.conf.d
        # Low-priority drop-in: the leading 00- sorts before KDE's own
        # kde_settings.conf, so a later explicit choice from System Settings
        # still overrides this default rather than being shadowed by it.
        cat > /etc/sddm.conf.d/00-naru-sddm-theme.conf <<EOF
[Theme]
Current=$WANT_SDDM_THEME
EOF
    fi
fi

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
