#!/usr/bin/env bash
# Roda todos os testes e gera os pacotes em ./release:
#   DeckScreen-<v>-x86_64.AppImage     PC, qualquer distro (precisa de glibc 2.34+; o da release do GitHub é estático)
#   deckscreen-host-<v>-x86_64.tar.gz  PC: binário + install.sh
#   DeckScreen-decky.zip               Steam Deck (Decky > Desenvolvedor > Instalar plugin de ZIP)
# Requisitos: cargo, python3, node e tsc.
set -euo pipefail
cd "$(dirname "$0")"
TARGET="${TARGET:-}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' host/Cargo.toml | head -1)"

echo "==> Rust: testes e build"
(cd host && cargo test --release --locked -q && cargo build --release --locked ${TARGET:+--target "$TARGET"})
BIN="host/target/${TARGET:+$TARGET/}release/deckscreen-host"

echo "==> Plugin do Deck: build e testes"
(cd deck && rm -rf dist && tsc -p tsconfig.json && node ../tests/frontend_smoke.mjs)
python3 -m unittest discover -s tests -q
python3 tests/integration.py "$BIN"

echo "==> Pacotes"
rm -rf release build && mkdir -p release build/DeckScreen/dist "build/deckscreen-host-$VERSION"
cp deck/plugin.json deck/package.json deck/main.py LICENSE build/DeckScreen/
cp deck/dist/index.js build/DeckScreen/dist/
mkdir -p build/DeckScreen/py_modules && cp deck/py_modules/*.py build/DeckScreen/py_modules/
(cd build && python3 -m zipfile -c ../release/DeckScreen-decky.zip DeckScreen)

H="build/deckscreen-host-$VERSION"
install -m755 "$BIN" "$H/deckscreen-host"
install -m755 install.sh "$H/install.sh"
cp README.md LICENSE "$H/"
tar -C build -czf "release/deckscreen-host-$VERSION-x86_64.tar.gz" "deckscreen-host-$VERSION"

packaging/appimage/build-appimage.sh "$BIN" release
rm -rf build
(cd release && sha256sum * > SHA256SUMS)
ls -la release
