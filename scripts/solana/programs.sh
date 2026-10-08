#!/usr/bin/env bash
# Build, test and inspect the Bordrless programs. Run in WSL (Linux x86_64) after
# scripts/solana/toolchain.sh install.
#
#   programs.sh build [program...]      cargo build-sbf into <checkout>/target/deploy (all by default)
#   programs.sh test [cargo test args]  host tests: the crates' unit tests and the LiteSVM suites
#   programs.sh clippy                  cargo clippy -D warnings on the host
#   programs.sh idl [out-dir]           anchor idl build for every deployed program (default target/idl)
#   programs.sh keys <cluster>          install keys/<cluster>/<program>-keypair.json into target/deploy
#   programs.sh check-keys <cluster>    every program keypair must match its declare_id!
#
# The checkout usually lives on a Windows drive (/mnt/f/...), where cargo is slow; host builds go to
# CARGO_TARGET_DIR (default ~/.cache/bordrless/target) and only the .so files are written back into
# <checkout>/target/deploy, where the tests look for them.
#
# hook_tester is built and tested like the others but is test-only: it is never deployed
# (deploy.sh and tools/localnet/start.mjs leave it out) and gets no IDL (nothing off chain calls it,
# and scripts/sync-idl.mjs copies every IDL in target/idl into the SDK).
set -euo pipefail

TOOLS_VERSION="v1.57"
SBF_ARCH="v3"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
PROGRAMS=(bordrless_token bordrless_swap bordrless_bridge bordrless_launch bordrless_kit tax_hook half_life hook_tester)
TEST_ONLY=(hook_tester)
DEPLOY_DIR="$ROOT/target/deploy"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/bordrless/target}"
AGAVE_BIN="$HOME/.local/share/solana/install/releases/v4.3.0/solana-release/bin"
export PATH="$AGAVE_BIN:$HOME/bin:$PATH"

die() {
  echo "programs.sh: $*" >&2
  exit 1
}

package_of() {
  echo "${1//_/-}"
}

is_test_only() {
  local program
  for program in "${TEST_ONLY[@]}"; do
    [[ "$program" == "$1" ]] && return 0
  done
  return 1
}

cmd_build() {
  local -a selected=("${PROGRAMS[@]}")
  if (($# > 0)); then selected=("$@"); fi
  mkdir -p "$DEPLOY_DIR"
  for program in "${selected[@]}"; do
    [[ -d "$ROOT/programs/$program" ]] || die "no such program: $program"
    echo "==> cargo build-sbf $program"
    rm -f "$DEPLOY_DIR/$program.so"
    cargo build-sbf --tools-version "$TOOLS_VERSION" --arch "$SBF_ARCH" \
      --manifest-path "$ROOT/programs/$program/Cargo.toml" \
      --sbf-out-dir "$DEPLOY_DIR" -- --locked
    ls -la "$DEPLOY_DIR/$program.so"
  done
}

cmd_test() {
  cd "$ROOT"
  cargo test --locked "$@"
}

cmd_clippy() {
  cd "$ROOT"
  cargo clippy --locked --workspace --all-targets -- -D warnings
}

cmd_idl() {
  local out="${1:-$ROOT/target/idl}"
  mkdir -p "$out"
  command -v anchor >/dev/null || die "anchor CLI not on PATH (toolchain.sh anchor ~/bin)"
  for program in "${PROGRAMS[@]}"; do
    if is_test_only "$program"; then continue; fi
    echo "==> anchor idl build $program"
    (cd "$ROOT/programs/$program" && anchor idl build --out "$out/$program.json" --out-ts "$out/$program.ts")
  done
}

cmd_keys() {
  local cluster="${1:-}"
  [[ -n "$cluster" ]] || die "usage: programs.sh keys <cluster>"
  mkdir -p "$DEPLOY_DIR"
  for program in "${PROGRAMS[@]}"; do
    local key="$ROOT/keys/$cluster/$program-keypair.json"
    [[ -f "$key" ]] || die "missing $key"
    cp "$key" "$DEPLOY_DIR/$program-keypair.json"
  done
  echo "installed $cluster keys into $DEPLOY_DIR"
}

cmd_check_keys() {
  local cluster="${1:-}"
  [[ -n "$cluster" ]] || die "usage: programs.sh check-keys <cluster>"
  local ok=1
  for program in "${PROGRAMS[@]}"; do
    local key="$ROOT/keys/$cluster/$program-keypair.json"
    local declared
    declared="$(grep -ho 'declare_id!("[^"]*")' "$ROOT/programs/$program/src/lib.rs" | sed 's/declare_id!("\(.*\)")/\1/')"
    local actual
    actual="$(solana-keygen pubkey "$key")"
    if [[ "$declared" == "$actual" ]]; then
      echo "ok   $program $actual"
    else
      echo "BAD  $program declare_id $declared, keypair $actual"
      ok=0
    fi
  done
  [[ "$ok" == 1 ]] || die "a program keypair does not match its declare_id!"
}

case "${1:-}" in
  build) cmd_build "${@:2}" ;;
  test) cmd_test "${@:2}" ;;
  clippy) cmd_clippy ;;
  idl) cmd_idl "${2:-}" ;;
  keys) cmd_keys "${2:-}" ;;
  check-keys) cmd_check_keys "${2:-}" ;;
  *)
    sed -n '2,10p' "${BASH_SOURCE[0]}" >&2
    exit 2
    ;;
esac
