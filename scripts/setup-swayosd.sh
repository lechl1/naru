#!/usr/bin/env bash
# Set up SwayOSD as the volume/brightness OSD on Kubuntu/naru.
# Plasma-like overlay: pops up on volume keys, scroll-on-icon, and click-to-mute.
# Idempotent — safe to re-run.

set -euo pipefail

log()  { printf '\033[1;34m::\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*" >&2; }
ok()   { printf '\033[1;32m✓\033[0m %s\n' "$*"; }
need_cmd() { command -v "$1" >/dev/null 2>&1; }

NARU_CFG="$HOME/.config/naru/config.kdl"
WAYBAR_CFG="$HOME/.config/waybar/config.jsonc"
OSD_CSS="$HOME/.config/swayosd/style.css"

# ---------------------------------------------------------- 1. install
if need_cmd swayosd-client; then
    ok "swayosd already installed"
else
    log "Installing swayosd from apt"
    sudo apt update
    sudo apt install -y swayosd
fi

# -------------------------------------------------- 2. Breeze CSS theme
if [[ -f "$OSD_CSS" ]]; then
    ok "swayosd style.css already present (leaving it alone)"
else
    log "Writing Breeze-Dark style.css for swayosd"
    mkdir -p "$(dirname "$OSD_CSS")"
    cat > "$OSD_CSS" <<'CSS'
/* SwayOSD — Breeze Dark to match Plasma's volume OSD */
window {
    border-radius: 12px;
    border: 1px solid #5d5e5f;
    background: rgba(35, 38, 41, 0.94);
    box-shadow: 0 6px 20px rgba(0, 0, 0, 0.45);
    padding: 10px 14px;
}
#container { margin: 4px; }
image, label { color: #eff0f1; }
progressbar {
    min-height: 8px;
    border-radius: 4px;
    background: #31363b;
}
progressbar > trough > progress {
    background: #3daee9;
    border-radius: 4px;
    min-height: 8px;
}
CSS
    ok "wrote $OSD_CSS"
fi

# ----------------------------- 3. waybar: route click/scroll to swayosd
if [[ ! -f "$WAYBAR_CFG" ]]; then
    warn "$WAYBAR_CFG not found — skipping waybar edits"
elif grep -q 'swayosd-client' "$WAYBAR_CFG"; then
    ok "waybar already wired to swayosd-client"
else
    log "Patching waybar pulseaudio module → swayosd-client"
    cp -- "$WAYBAR_CFG" "$WAYBAR_CFG.bak.$(date +%s)"
    # Plasma-style click semantics:
    #   left-click  → open mixer (pavucontrol-qt)
    #   right-click → mute toggle (with OSD)
    #   scroll      → volume up/down (with OSD)
    sed -i 's|"on-click": "wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle"|"on-click": "pavucontrol-qt"|' "$WAYBAR_CFG"
    sed -i 's|"on-click-right": "pavucontrol-qt"|"on-click-right": "swayosd-client --output-volume mute-toggle"|' "$WAYBAR_CFG"
    # Append scroll handlers after the (now-rewritten) on-click-right line.
    awk '
      /"on-click-right": "swayosd-client --output-volume mute-toggle"/ && !done {
        sub(/,?[[:space:]]*$/, ",")
        print
        print "        \"on-scroll-up\": \"swayosd-client --output-volume raise\","
        print "        \"on-scroll-down\": \"swayosd-client --output-volume lower\""
        done=1
        next
      }
      { print }
    ' "$WAYBAR_CFG" > "$WAYBAR_CFG.tmp" && mv -- "$WAYBAR_CFG.tmp" "$WAYBAR_CFG"
    ok "waybar pulseaudio module updated"
fi

# ----------------------------- 4. naru: spawn-at-startup + media keys
if [[ ! -f "$NARU_CFG" ]]; then
    warn "$NARU_CFG not found — skipping naru edits"
else
    if grep -qE '^spawn-at-startup[[:space:]]+"swayosd-server"' "$NARU_CFG"; then
        ok 'spawn-at-startup "swayosd-server" already present'
    else
        log "Adding swayosd-server to spawn-at-startup"
        cp -- "$NARU_CFG" "$NARU_CFG.bak.$(date +%s)"
        awk -v ins='spawn-at-startup "swayosd-server"' '
          { lines[NR]=$0 }
          /^spawn-at-startup/ { last=NR }
          END {
            if (!last) { for (i=1;i<=NR;i++) print lines[i]; print ins; exit }
            for (i=1;i<=NR;i++) { print lines[i]; if (i==last) print ins }
          }
        ' "$NARU_CFG" > "$NARU_CFG.tmp" && mv -- "$NARU_CFG.tmp" "$NARU_CFG"
        ok "swayosd-server added"
    fi

    if grep -q 'swayosd-client' "$NARU_CFG"; then
        ok "naru already has swayosd-client bindings"
    elif grep -qE '^binds[[:space:]]*\{' "$NARU_CFG"; then
        log "Inserting XF86Audio* + brightness bindings into binds block"
        cp -- "$NARU_CFG" "$NARU_CFG.bak.$(date +%s)"
        BINDS=$(cat <<'KDL'

    // SwayOSD: media keys with Plasma-like overlay.
    XF86AudioRaiseVolume allow-when-locked=true { spawn "swayosd-client" "--output-volume" "raise"; }
    XF86AudioLowerVolume allow-when-locked=true { spawn "swayosd-client" "--output-volume" "lower"; }
    XF86AudioMute        allow-when-locked=true { spawn "swayosd-client" "--output-volume" "mute-toggle"; }
    XF86AudioMicMute     allow-when-locked=true { spawn "swayosd-client" "--input-volume"  "mute-toggle"; }
    XF86MonBrightnessUp   { spawn "swayosd-client" "--brightness" "raise"; }
    XF86MonBrightnessDown { spawn "swayosd-client" "--brightness" "lower"; }
KDL
)
        export BINDS
        awk -v ins="$BINDS" '
          /^binds[[:space:]]*\{/ { in_binds=1; print; next }
          in_binds && /^\}/ { if (!done) { print ins; done=1 } in_binds=0 }
          { print }
        ' "$NARU_CFG" > "$NARU_CFG.tmp" && mv -- "$NARU_CFG.tmp" "$NARU_CFG"
        ok "media key bindings added"
    else
        warn 'No binds { } block found — skipping keybinds'
    fi
fi

# ------------------------------------------------ 5. reload + start now
if pgrep -x waybar >/dev/null; then
    pkill -SIGUSR2 waybar || true
    ok "waybar reloaded"
fi
if need_cmd naru && pgrep -x naru >/dev/null; then
    naru msg action reload-config 2>/dev/null && ok "naru config reloaded" \
        || warn "naru reload failed — restart your session if keys don't work"
fi
if ! pgrep -x swayosd-server >/dev/null; then
    log "Starting swayosd-server"
    ( swayosd-server & disown ) >/dev/null 2>&1
    sleep 0.4
    pgrep -x swayosd-server >/dev/null && ok "swayosd-server is up" \
        || warn "swayosd-server didn't start — run it manually to see errors"
else
    ok "swayosd-server already running"
fi

# ------------------------------------------------------- 6. smoke test
if need_cmd swayosd-client && pgrep -x swayosd-server >/dev/null; then
    log "Triggering a test raise — you should see the OSD"
    swayosd-client --output-volume raise || true
    sleep 0.3
    swayosd-client --output-volume lower || true
fi

ok "Done. Click the speaker → OSD; scroll on it → adjust; volume keys also trigger OSD."
