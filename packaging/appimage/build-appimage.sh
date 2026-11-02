#!/usr/bin/env bash
# Monta DeckScreen-<versão>-<arch>.AppImage a partir de um binário já compilado.
#   packaging/appimage/build-appimage.sh <binário> [pasta-de-saída]
# Precisa do appimagetool (baixado automaticamente se não estiver no PATH).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$(realpath "${1:?uso: build-appimage.sh <binário> [saída]}")"
OUT="$(realpath -m "${2:-$ROOT/release}")"
ARCH="${ARCH:-$(uname -m)}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/host/Cargo.toml" | head -1)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

APPDIR="$WORK/DeckScreen.AppDir"
install -Dm755 "$BIN" "$APPDIR/usr/bin/deckscreen-host"
install -Dm644 "$ROOT/host/assets/deckscreen.svg" "$APPDIR/deckscreen.svg"
install -Dm644 "$ROOT/host/assets/deckscreen.svg" "$APPDIR/usr/share/icons/hicolor/scalable/apps/deckscreen.svg"
ln -s deckscreen.svg "$APPDIR/.DirIcon"
cat > "$APPDIR/deckscreen.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=DeckScreen
Comment=Steam Deck como segunda tela e controle do PC
Exec=deckscreen-host
Icon=deckscreen
Terminal=false
Categories=Utility;
DESKTOP
cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
HERE="${APPDIR:-$(dirname "$(readlink -f "$0")")}"
exec "$HERE/usr/bin/deckscreen-host" "$@"
APPRUN
chmod +x "$APPDIR/AppRun"

TOOL="$(command -v appimagetool || true)"
if [ -z "$TOOL" ]; then
  TOOL="$WORK/appimagetool"
  curl -fsSL -o "$TOOL" "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$(uname -m).AppImage"
  chmod +x "$TOOL"
fi
mkdir -p "$OUT"
DEST="$OUT/DeckScreen-$VERSION-$ARCH.AppImage"
ARCH="$ARCH" APPIMAGE_EXTRACT_AND_RUN=1 "$TOOL" --no-appstream "$APPDIR" "$DEST"
echo "AppImage: $DEST"
