#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'MSG'
Usage: scripts/install.sh [--autostart]

Installs the release binary, desktop entry, and icon for the current user.
Autostart is opt-in and is only written with --autostart.

Environment:
  DESTDIR          Optional staging root, useful for package inspection.
  BIN_DIR          Binary destination (default: ${XDG_BIN_HOME:-$HOME/.local/bin})
  XDG_DATA_HOME    Desktop/icon destination (default: ~/.local/share)
  XDG_CONFIG_HOME  Autostart destination (default: ~/.config)
MSG
}

AUTOSTART=false
for arg in "$@"; do
    case "$arg" in
        --autostart) AUTOSTART=true ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown argument: $arg" >&2; usage >&2; exit 2 ;;
    esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DESTDIR="${DESTDIR:-}"
BIN_DIR="${BIN_DIR:-${XDG_BIN_HOME:-$HOME/.local/bin}}"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
APPLICATIONS_DIR="$DESTDIR$DATA_HOME/applications"
ICONS_DIR="$DESTDIR$DATA_HOME/icons/hicolor/scalable/apps"
AUTOSTART_DIR="$DESTDIR$CONFIG_HOME/autostart"
BINARY="$ROOT/target/release/glass-island"
INSTALLED_BINARY="$DESTDIR$BIN_DIR/glass-island"

if [[ ! -x "$BINARY" ]]; then
    echo "glass-island release binary not found at $BINARY" >&2
    echo "Build it first: cargo build --release" >&2
    exit 1
fi

exec_escape() {
    local value="$1"
    value="${value//\\/\\\\}"
    value="${value//\"/\\\"}"
    value="${value//\`/\\\`}"
    value="${value//\$/\\\$}"
    printf '"%s"' "$value"
}
sed_escape() {
    local value="$1"
    value="${value//\\/\\\\}"
    value="${value//&/\\&}"
    value="${value//|/\\|}"
    printf '%s' "$value"
}

EXEC_LINE="$(exec_escape "$BIN_DIR/glass-island")"
EXEC_REPLACEMENT="$(sed_escape "$EXEC_LINE")"

install -Dm755 "$BINARY" "$INSTALLED_BINARY"
install -d "$APPLICATIONS_DIR" "$ICONS_DIR"
sed "s|@EXEC@|$EXEC_REPLACEMENT|g" \
    "$ROOT/packaging/glass-island.desktop" > "$APPLICATIONS_DIR/glass-island.desktop"
chmod 644 "$APPLICATIONS_DIR/glass-island.desktop"
install -Dm644 "$ROOT/packaging/icons/glass-island.svg" "$ICONS_DIR/glass-island.svg"

if $AUTOSTART; then
    install -d "$AUTOSTART_DIR"
    sed "s|@EXEC@|$EXEC_REPLACEMENT|g" \
        "$ROOT/packaging/glass-island-autostart.desktop" > "$AUTOSTART_DIR/glass-island.desktop"
    chmod 644 "$AUTOSTART_DIR/glass-island.desktop"
fi

if [[ -z "$DESTDIR" ]]; then
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$APPLICATIONS_DIR" >/dev/null 2>&1 || true
    fi
    if command -v kbuildsycoca6 >/dev/null 2>&1; then
        kbuildsycoca6 >/dev/null 2>&1 || true
    fi
fi

cat <<MSG
Glass Island installed:
  binary:    $BIN_DIR/glass-island
  desktop:   $DATA_HOME/applications/glass-island.desktop
  icon:      $DATA_HOME/icons/hicolor/scalable/apps/glass-island.svg
  autostart: $($AUTOSTART && echo "$CONFIG_HOME/autostart/glass-island.desktop" || echo "not installed (use --autostart)")

The application was not started. Ensure $BIN_DIR is in your PATH.
MSG
