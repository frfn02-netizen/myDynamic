#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'MSG'
Usage: scripts/uninstall.sh

Removes the per-user binary, desktop entry, icon, and autostart entry.
Environment: DESTDIR, BIN_DIR, XDG_DATA_HOME, XDG_CONFIG_HOME.
MSG
}

for arg in "$@"; do
    case "$arg" in
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown argument: $arg" >&2; usage >&2; exit 2 ;;
    esac
done

DESTDIR="${DESTDIR:-}"
BIN_DIR="${BIN_DIR:-${XDG_BIN_HOME:-$HOME/.local/bin}}"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
CONFIG_HOME="${XDG_CONFIG_HOME:-$HOME/.config}"
APPLICATIONS_DIR="$DESTDIR$DATA_HOME/applications"
ICONS_DIR="$DESTDIR$DATA_HOME/icons/hicolor/scalable/apps"
AUTOSTART_DIR="$DESTDIR$CONFIG_HOME/autostart"

rm -f "$DESTDIR$BIN_DIR/glass-island"
rm -f "$APPLICATIONS_DIR/glass-island.desktop"
rm -f "$AUTOSTART_DIR/glass-island.desktop"
rm -f "$ICONS_DIR/glass-island.svg"

if [[ -z "$DESTDIR" ]]; then
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$APPLICATIONS_DIR" >/dev/null 2>&1 || true
    fi
    if command -v kbuildsycoca6 >/dev/null 2>&1; then
        kbuildsycoca6 >/dev/null 2>&1 || true
    fi
fi

echo "Glass Island desktop files and binary removed."
