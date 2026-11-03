#!/bin/sh
# Installs the built AppImage for the current user and adds av1opt to the application menu.
# Run `npm run dist` first. Undo: rm -r ~/.local/opt/av1opt ~/.local/share/applications/av1opt.desktop
set -eu
cd "$(dirname "$0")"
app=$(ls -t dist/av1opt-*.AppImage 2>/dev/null | head -n 1)
[ -n "$app" ] || { echo "no AppImage in dist/; run npm run dist first" >&2; exit 1; }
dest="${XDG_DATA_HOME:-$HOME/.local/share}"
opt="$HOME/.local/opt/av1opt"
mkdir -p "$opt" "$dest/applications"
cp "$app" "$opt/av1opt.AppImage.new" && mv "$opt/av1opt.AppImage.new" "$opt/av1opt.AppImage"
chmod +x "$opt/av1opt.AppImage"
cp icon.png "$opt/icon.png"
cat > "$dest/applications/av1opt.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=av1opt
Comment=Shrink videos to AV1 at a target quality
Exec="$opt/av1opt.AppImage" %U
Icon=$opt/icon.png
Terminal=false
Categories=AudioVideo;Video;
StartupWMClass=av1opt
DESKTOP
command -v update-desktop-database >/dev/null && update-desktop-database "$dest/applications" 2>/dev/null || true
echo "installed $opt/av1opt.AppImage and $dest/applications/av1opt.desktop"
