#!/usr/bin/env bash
# DeckScreen — instalação no PC a partir desta pasta (CachyOS/Arch ou qualquer distro).
#
#   ./install.sh            instala/atualiza (pode rodar de novo)
#   ./install.sh --yes      sem perguntas
#
# Usa o binário pronto que vem no pacote de release (./deckscreen-host); se não
# houver, compila com o Rust (cargo). Depois roda "deckscreen-host install", que
# cuida de dependências, Sunshine, serviços, widget do KDE, firewall e Wake-on-LAN.
set -euo pipefail
cd "$(dirname "$0")"

if [ "$(id -u)" -eq 0 ]; then
  echo "Rode como seu usuário normal, NÃO com sudo." >&2
  exit 1
fi

if [ -x ./deckscreen-host ]; then
  BIN=./deckscreen-host
else
  if ! command -v cargo >/dev/null; then
    echo "==> O Rust (cargo) é necessário para compilar o DeckScreen."
    if command -v pacman >/dev/null; then
      read -rp "  Instalar com 'sudo pacman -S rust'? [S/n] " r
      [[ -z "$r" || "$r" =~ ^[sSyY] ]] && sudo pacman -S --needed --noconfirm rust
    fi
    command -v cargo >/dev/null || { echo "Instale o Rust (https://rustup.rs) ou use o AppImage." >&2; exit 1; }
  fi
  echo "==> Compilando (leva menos de um minuto)…"
  (cd host && cargo build --release --locked)
  BIN=host/target/release/deckscreen-host
fi

exec "$BIN" install "$@"
