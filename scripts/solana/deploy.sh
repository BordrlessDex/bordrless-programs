#!/usr/bin/env bash
# Deploys or upgrades the programs on a cluster. Run in WSL after programs.sh build.
#
#   deploy.sh <devnet|mainnet> <deployer keypair> [program...]
#
# The program keypairs come from keys/<cluster>/<program>-keypair.json (devnet: generated 2026-10-06;
# mainnet: generate fresh ones and update every declare_id! and Anchor.toml before building). A
# first deploy needs the program keypair to sign; an upgrade needs only the upgrade authority. The
# deploy is resumable: an interrupted one leaves a buffer that `solana program deploy` picks up
# again when given the same --buffer, and `solana program show --buffers` lists them.
# SOLANA_DEPLOY_FLAGS is passed on to every `solana program deploy` (on mainnet, a priority fee:
# SOLANA_DEPLOY_FLAGS="--with-compute-unit-price 50000 --use-rpc").
#
# KEYS_DIR overrides keys/<cluster> (mainnet: KEYS_DIR=~/.config/solana/bordrless-mainnet, where the
# phase 3a keypairs hook_timelock-keypair.json and hook_vault-keypair.json are). BUFFER_DIR, when set,
# makes each deploy write through the buffer keypair <BUFFER_DIR>/buffer-<program>.json (created
# with `solana-keygen new --no-bip39-passphrase -o ...` if missing), so an interrupted deploy resumes
# with the same buffer and its rent is recovered with `solana program close <buffer>`.
# An upgrade whose .so is larger than the ProgramData needs `solana program extend <id> <bytes>`
# first (this script checks and refuses). Phase 3a's order is in docs/phase3a.md §17 "Deploy
# runbook": hook_timelock -> launch -> companion -> attest backfill -> hook_vault; the companion
# and launch upgrades may equally be run as the plain `solana program deploy` commands below.
set -euo pipefail

CLUSTER="${1:-}"
DEPLOYER="${2:-}"
[[ -n "$CLUSTER" && -n "$DEPLOYER" ]] || { sed -n '2,10p' "${BASH_SOURCE[0]}" >&2; exit 2; }
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
AGAVE_BIN="$HOME/.local/share/solana/install/releases/v4.3.0/solana-release/bin"
export PATH="$AGAVE_BIN:$PATH"
case "$CLUSTER" in
  devnet) URL="${SOLANA_DEVNET_RPC:-https://api.devnet.solana.com}" ;;
  mainnet) URL="${SOLANA_MAINNET_RPC:?set SOLANA_MAINNET_RPC to a keyed mainnet RPC}" ;;
  *) echo "cluster must be devnet or mainnet" >&2; exit 2 ;;
esac
PROGRAMS=(bordrless_token bordrless_swap bordrless_bridge bordrless_launch bordrless_kit tax_hook half_life)
if (($# > 2)); then PROGRAMS=("${@:3}"); fi
KEYS="${KEYS_DIR:-$ROOT/keys/$CLUSTER}"

echo "deployer $(solana-keygen pubkey "$DEPLOYER"), balance $(solana balance -u "$URL" -k "$DEPLOYER")"
for program in "${PROGRAMS[@]}"; do
  key="$KEYS/$program-keypair.json"
  so="$ROOT/target/deploy/$program.so"
  [[ -f "$key" ]] || { echo "missing $key" >&2; exit 1; }
  [[ -f "$so" ]] || { echo "missing $so (build first)" >&2; exit 1; }
  id="$(solana-keygen pubkey "$key")"
  size="$(stat -c %s "$so")"
  echo "==> $program $id ($size bytes)"
  buffer_flags=()
  if [[ -n "${BUFFER_DIR:-}" ]]; then
    buffer="$BUFFER_DIR/buffer-$program.json"
    [[ -f "$buffer" ]] || solana-keygen new --no-bip39-passphrase --silent -o "$buffer" >/dev/null
    buffer_flags=(--buffer "$buffer")
  fi
  if solana program show -u "$URL" "$id" >/dev/null 2>&1; then
    # An upgrade: the ProgramData must hold the new code (extend it first; anyone may).
    have="$(solana program show -u "$URL" "$id" --output json | python3 -c 'import json,sys; print(json.load(sys.stdin).get("dataLen", 0))')"
    if ((size > have)); then
      echo "the ProgramData holds $have bytes; extend it first: solana program extend -u <url> -k <payer> $id $((size - have))" >&2
      exit 1
    fi
    solana program deploy -u "$URL" -k "$DEPLOYER" --program-id "$id" --upgrade-authority "$DEPLOYER" --max-sign-attempts 30 "${buffer_flags[@]}" ${SOLANA_DEPLOY_FLAGS:-} "$so"
  else
    solana program deploy -u "$URL" -k "$DEPLOYER" --program-id "$key" --upgrade-authority "$DEPLOYER" --max-sign-attempts 30 "${buffer_flags[@]}" ${SOLANA_DEPLOY_FLAGS:-} "$so"
  fi
done
echo "done; balance $(solana balance -u "$URL" -k "$DEPLOYER")"
