#!/bin/sh
# Installs the built .deb (asks for sudo) and removes an older per-user AppImage install, whose
# menu entry would hide the system one. Run `npm run dist` first. Undo: sudo apt remove av1opt-desktop
set -eu
cd "$(dirname "$0")"
deb=$(ls -t dist/av1opt-desktop_*.deb 2>/dev/null | head -n 1)
[ -n "$deb" ] || { echo "no .deb in dist/; run npm run dist first" >&2; exit 1; }
sudo apt install --reinstall -y "./$deb"

old="$HOME/.local/opt/av1opt"
entry="${XDG_DATA_HOME:-$HOME/.local/share}/applications/av1opt.desktop"
if [ -f "$entry" ] && grep -q "$old/av1opt.AppImage" "$entry"; then
  rm -f "$entry"
  rm -rf "$old"
  command -v update-desktop-database >/dev/null && update-desktop-database "$(dirname "$entry")" 2>/dev/null || true
  echo "removed the old AppImage install ($old)"
fi
echo "installed: run av1opt from the app menu, or av1opt-desktop"
