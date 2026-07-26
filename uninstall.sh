#!/usr/bin/env bash
# Remove the csm binary from your PATH (the mirror of install.sh).
#
#   ./uninstall.sh            Remove the installed csm symlink/binary
#   ./uninstall.sh --purge    Also delete csm's index (~/.local/share/csm/index.redb)
#
# Note: --purge only removes csm's own bookkeeping. It never touches claude's
# session transcripts under ~/.claude/projects/.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

purge=false
for arg in "$@"; do
  case "$arg" in
    --purge) purge=true ;;
    -h|--help) sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "uninstall.sh: unknown option '$arg'" >&2; exit 2 ;;
  esac
done

bindir="${CSM_BINDIR:-$HOME/.local/bin}"
target="$bindir/csm"
removed=false

if [ -L "$target" ]; then
  dest="$(readlink "$target")"
  case "$dest" in
    "$here/target/release/csm")
      rm -f "$target"
      echo "==> Removed symlink: $target"
      removed=true
      ;;
    *)
      echo "==> Skipped: $target is a symlink to '$dest' (not this checkout)."
      echo "    Remove it manually if you're sure."
      ;;
  esac
elif [ -e "$target" ]; then
  rm -f "$target"
  echo "==> Removed: $target"
  removed=true
else
  echo "==> Nothing to remove at $target"
fi

if $purge; then
  data="${XDG_DATA_HOME:-$HOME/.local/share}/csm"
  legacy="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/csm"
  purged=false
  if [ -d "$data" ]; then
    rm -rf "$data"
    echo "==> Purged csm data: $data"
    purged=true
  fi
  if [ -d "$legacy" ]; then
    rm -rf "$legacy"
    echo "==> Purged legacy csm data: $legacy"
    purged=true
  fi
  $purged || echo "==> No csm data to purge"
fi

if $removed; then
  echo "Done. (Build artifacts left in place; run 'cargo clean' to remove them.)"
fi
