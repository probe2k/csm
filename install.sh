#!/usr/bin/env bash
# Build csm in release mode and symlink it onto your PATH.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"

echo "==> Building csm (release)…"
cargo build --release

bindir="${CSM_BINDIR:-$HOME/.local/bin}"
mkdir -p "$bindir"
ln -sf "$here/target/release/csm" "$bindir/csm"

echo "==> Installed: $bindir/csm -> $here/target/release/csm"
case ":$PATH:" in
  *":$bindir:"*) ;;
  *) echo "    NOTE: $bindir is not on your PATH. Add it to your shell profile." ;;
esac
echo "Done. Try:  csm --help"
