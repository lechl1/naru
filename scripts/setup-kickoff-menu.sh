#!/usr/bin/env bash
# Bootstrap the Kickoff-style start menu (krunner + waybar K-button) on Kubuntu/naru.
# Idempotent: safe to re-run. Skips work that's already done.

set -euo pipefail

log()  { printf '\033[1;34m::\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*" >&2; }
ok()   { printf '\033[1;32m✓\033[0m %s\n' "$*"; }

need_cmd() { command -v "$1" >/dev/null 2>&1; }

NARU_CFG="$HOME/.config/naru/config.kdl"

# ----------------------------------------------------------- 1. krunner
log "Checking krunner"
if need_cmd krunner; then
    ok "krunner already installed at $(command -v krunner)"
else
    log "Installing krunner from apt"
    sudo apt update
    sudo apt install -y krunner
fi

# ------------------------------------------------------------- 2. qdbus6
if ! need_cmd qdbus6; then
    log "Installing qt6-tools (provides qdbus6)"
    sudo apt install -y qt6-tools-dev-tools || sudo apt install -y qttools5-dev-tools
fi
QDBUS_BIN="$(command -v qdbus6 || command -v qdbus || true)"
if [[ -z "$QDBUS_BIN" ]]; then
    warn "Neither qdbus6 nor qdbus found — the waybar K-button won't toggle krunner"
else
    ok "Using $QDBUS_BIN for the D-Bus toggle"
fi

# ------------------------------------- 3. naru config: krunner --daemon
if [[ ! -f "$NARU_CFG" ]]; then
    warn "$NARU_CFG not found — skipping autostart + keybind injection"
else
    if grep -qE '^spawn-at-startup[[:space:]]+"krunner"' "$NARU_CFG"; then
        ok 'spawn-at-startup "krunner" already present'
    else
        log 'Adding `spawn-at-startup "krunner" "--daemon"` after existing spawn-at-startup lines'
        cp -- "$NARU_CFG" "$NARU_CFG.bak.$(date +%s)"
        awk -v ins='spawn-at-startup "krunner" "--daemon"' '
          { lines[NR]=$0 }
          /^spawn-at-startup/ { last=NR }
          END {
            if (!last) { for (i=1;i<=NR;i++) print lines[i]; print ins; exit }
            for (i=1;i<=NR;i++) { print lines[i]; if (i==last) print ins }
          }
        ' "$NARU_CFG" > "$NARU_CFG.tmp" && mv -- "$NARU_CFG.tmp" "$NARU_CFG"
        ok "krunner --daemon added to spawn-at-startup"
    fi

    # ------------------------------- 4. naru config: Mod+Space binding
    if grep -qE 'org\.kde\.krunner.*display' "$NARU_CFG"; then
        ok "Mod+Space krunner binding already present"
    elif grep -qE '^binds[[:space:]]*\{' "$NARU_CFG"; then
        log "Inserting Mod+Space krunner binding into binds block"
        cp -- "$NARU_CFG" "$NARU_CFG.bak.$(date +%s)"
        BIND_LINE='    Mod+Space hotkey-overlay-title="Application Launcher" { spawn "'"${QDBUS_BIN:-qdbus6}"'" "org.kde.krunner" "/App" "display"; }'
        awk -v ins="$BIND_LINE" '
          /^binds[[:space:]]*\{/ { in_binds=1; print; next }
          in_binds && /^\}/ { if (!done) { print ins; done=1 } in_binds=0 }
          { print }
        ' "$NARU_CFG" > "$NARU_CFG.tmp" && mv -- "$NARU_CFG.tmp" "$NARU_CFG"
        ok "Mod+Space krunner binding added"
    else
        warn 'No `binds { ... }` block found in naru config — skipping keybind'
    fi
fi

# ------------------------------------------- 5. reload waybar + naru
if pgrep -x waybar >/dev/null; then
    pkill -SIGUSR2 waybar || true
    ok "waybar reloaded"
else
    warn "waybar not running — start it: waybar &"
fi

if need_cmd naru && pgrep -x naru >/dev/null; then
    if naru msg action reload-config 2>/dev/null; then
        ok "naru config reloaded"
    else
        warn "naru is running but config reload failed — restart your session if the keybind doesn't work"
    fi
fi

# ------------------------------------- 6. start krunner daemon now
if ! pgrep -x krunner >/dev/null; then
    log "Starting krunner --daemon for this session"
    ( krunner --daemon & disown ) >/dev/null 2>&1
    sleep 0.4
    pgrep -x krunner >/dev/null && ok "krunner daemon is running" \
        || warn "krunner did not start — try running it manually to see errors"
else
    ok "krunner daemon already running"
fi

# ------------------------------------- 7. smoke test the D-Bus toggle
if [[ -n "$QDBUS_BIN" ]]; then
    if "$QDBUS_BIN" org.kde.krunner /App display >/dev/null 2>&1; then
        ok "D-Bus toggle works — close krunner with Esc"
        sleep 0.3
        "$QDBUS_BIN" org.kde.krunner /App display >/dev/null 2>&1 || true
    else
        warn "D-Bus call failed — krunner may need another second to register on the bus"
    fi
fi

ok "Done. Click the blue K on waybar, or press Mod+Space."
