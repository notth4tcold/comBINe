#!/usr/bin/env bash
# Compila e instala o comBINe para o usuário atual (~/.local), em Arch ou Ubuntu.
# Uso: ./install.sh            instala
#      ./install.sh --uninstall remove
set -euo pipefail
cd "$(dirname "$0")"
PREFIX="${PREFIX:-$HOME/.local}"

if [[ "${1:-}" == "--uninstall" ]]; then
  rm -f "$PREFIX/bin/combine" "$PREFIX/share/applications/combine.desktop" "$PREFIX/share/icons/hicolor/scalable/apps/combine.svg"
  echo "comBINe removido."
  exit 0
fi

if ! command -v cargo >/dev/null; then
  echo "Rust não encontrado. Instale com:"
  echo "  Arch:   sudo pacman -S rust"
  echo "  Ubuntu: sudo apt install build-essential curl && curl https://sh.rustup.rs -sSf | sh"
  exit 1
fi

cargo build --release
install -Dm755 target/release/combine "$PREFIX/bin/combine"
install -Dm644 packaging/combine.desktop "$PREFIX/share/applications/combine.desktop"
install -Dm644 packaging/combine.svg "$PREFIX/share/icons/hicolor/scalable/apps/combine.svg"
update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
gtk-update-icon-cache -q "$PREFIX/share/icons/hicolor" 2>/dev/null || true
echo "Instalado em $PREFIX/bin/combine — procure \"comBINe\" no menu de aplicativos."
case ":$PATH:" in *":$PREFIX/bin:"*) ;; *) echo "Aviso: $PREFIX/bin não está no PATH." ;; esac
