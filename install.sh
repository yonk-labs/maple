#!/usr/bin/env bash
# Build maple (release) and install it to ~/.local/bin, then report prerequisites.
set -euo pipefail

cd "$(dirname "$0")"
BIN_DIR="${MAPLE_BIN_DIR:-$HOME/.local/bin}"

# C compiler is a BUILD-time requirement since v0.3.6 (Oracle PL/SQL support
# vendors a C scanner, compiled via build.rs) — check before attempting the
# build so a missing compiler gets this message, not a raw linker error.
if ! { command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || command -v clang >/dev/null 2>&1; }; then
  echo "MISSING: a C compiler (cc/gcc/clang) is required to build maple since v0.3.6"
  echo "(Oracle PL/SQL support vendors a C scanner). Install one, e.g.:"
  echo "  Debian/Ubuntu: sudo apt install build-essential"
  echo "  macOS:         xcode-select --install"
  exit 1
fi

echo "Building maple (release)…"
cargo build --release

mkdir -p "$BIN_DIR"
install -m 0755 target/release/maple "$BIN_DIR/maple"
echo "Installed: $BIN_DIR/maple"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "NOTE: $BIN_DIR is not on your PATH — add it (e.g. export PATH=\"$BIN_DIR:\$PATH\")." ;;
esac

echo
echo "Runtime prerequisites:"
if command -v git >/dev/null 2>&1; then
  echo "  [ok]      git (used for git-aware delta indexing — maple falls back to a full"
  echo "            content hash walk without it, so this isn't strictly required)"
else
  echo "  [MISSING] git (used for git-aware delta indexing — maple falls back to a full"
  echo "            content hash walk without it, so this isn't strictly required)"
fi

echo
echo "Next:  maple index /path/to/your/repo && maple enumerate /path/to/your/repo --symbol your_function"
