#!/usr/bin/env bash
# Installs the pinned Solana toolchain from release archives checked against pinned sha256 digests.
# Linux x86_64 only (run it in WSL on this machine).
#
#   toolchain.sh install        Agave CLI 4.3.0 and platform-tools v1.57, from verified archives
#   toolchain.sh anchor <dir>   the prebuilt Anchor CLI 1.2.0 into <dir>/anchor
#   toolchain.sh path           the Agave bin directory, for PATH
set -euo pipefail

AGAVE_VERSION="v4.3.0"
AGAVE_SHA256="c97289a8abb1d0efb497d8b5cb285baabd9b7f8ea6647f5d145c5dc8ff3611e8"
PLATFORM_TOOLS_VERSION="v1.57"
PLATFORM_TOOLS_SHA256="b0f7af104adf726fff2a6a09ea2eb2f2d2965c92295f4d7388c08d140e0c2b00"
ANCHOR_CLI_VERSION="1.2.0"
ANCHOR_CLI_SHA256="0c9c41a3292c281cc6eadb78d6e1c8224d8324a34b0736a89d640fd314db05b7"

AGAVE_URL="https://github.com/anza-xyz/agave/releases/download/$AGAVE_VERSION/solana-release-x86_64-unknown-linux-gnu.tar.bz2"
PLATFORM_TOOLS_URL="https://github.com/anza-xyz/platform-tools/releases/download/$PLATFORM_TOOLS_VERSION/platform-tools-linux-x86_64.tar.bz2"
ANCHOR_CLI_URL="https://github.com/otter-sec/anchor/releases/download/v$ANCHOR_CLI_VERSION/anchor-$ANCHOR_CLI_VERSION-x86_64-unknown-linux-gnu"

AGAVE_DIR="$HOME/.local/share/solana/install/releases/$AGAVE_VERSION/solana-release"
TOOLS_DIR="$HOME/.cache/solana/$PLATFORM_TOOLS_VERSION/platform-tools"
ARCHIVE_DIR="$HOME/.cache/bordrless-toolchain"

die() {
  echo "toolchain.sh: $*" >&2
  exit 1
}

fetch() {
  echo "fetch: $1"
  curl -sSfL --retry 3 -o "$3" "$1"
  if ! echo "$2  $3" | sha256sum --check --quiet; then
    rm -f "$3"
    die "sha256 mismatch for $1 (expected $2)"
  fi
}

extract() {
  local archive="$1" member dest tmp
  if (($# == 3)); then
    member="$2"
    dest="$3"
  else
    member=""
    dest="$2"
  fi
  mkdir -p "$(dirname "$dest")"
  tmp="$(mktemp -d "$(dirname "$dest")/.extract.XXXXXX")"
  mkdir "$tmp/x"
  tar --no-same-owner -xjf "$archive" -C "$tmp/x"
  rm -rf "$dest"
  mv "$tmp/x${member:+/$member}" "$dest"
  rm -rf "$tmp"
}

archive() {
  local url="$1" sha="$2" file="$ARCHIVE_DIR/$3"
  mkdir -p "$ARCHIVE_DIR"
  if [[ -f "$file" ]] && ! echo "$sha  $file" | sha256sum --check --quiet >/dev/null 2>&1; then
    rm -f "$file"
  fi
  [[ -f "$file" ]] || fetch "$url" "$sha" "$file"
  ARCHIVE="$file"
}

cmd_install() {
  [[ "$(uname -sm)" == "Linux x86_64" ]] || die "pinned archives exist for Linux x86_64 only"
  if [[ -x "$AGAVE_DIR/bin/cargo-build-sbf" && -d "$TOOLS_DIR" && "${FORCE:-}" != "1" ]]; then
    echo "toolchain already installed (FORCE=1 re-extracts)"
  else
    archive "$AGAVE_URL" "$AGAVE_SHA256" "solana-release-$AGAVE_VERSION.tar.bz2"
    extract "$ARCHIVE" solana-release "$AGAVE_DIR"
    archive "$PLATFORM_TOOLS_URL" "$PLATFORM_TOOLS_SHA256" "platform-tools-$PLATFORM_TOOLS_VERSION.tar.bz2"
    extract "$ARCHIVE" "$TOOLS_DIR"
  fi
  "$AGAVE_DIR/bin/cargo-build-sbf" --version
}

cmd_anchor() {
  local dir="${1:-}"
  [[ -n "$dir" ]] || die "usage: toolchain.sh anchor <dir>"
  mkdir -p "$dir"
  fetch "$ANCHOR_CLI_URL" "$ANCHOR_CLI_SHA256" "$dir/anchor"
  chmod +x "$dir/anchor"
  "$dir/anchor" --version
}

case "${1:-}" in
  install) cmd_install ;;
  anchor) cmd_anchor "${2:-}" ;;
  path) echo "$AGAVE_DIR/bin" ;;
  *)
    sed -n '2,7p' "${BASH_SOURCE[0]}" >&2
    exit 2
    ;;
esac
