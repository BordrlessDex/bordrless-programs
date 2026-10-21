#!/usr/bin/env bash
# Dumps the venue programs the SOL switch's tests run (docs/token-quotes.md §7.5.2) from mainnet into
# programs/tests/fixtures/venues/*.so (git-ignored, ~15 MB). Run in WSL with the Solana CLI on PATH:
#
#   SOLANA_RPC_URL=<a mainnet RPC> scripts/solana/venue-fixtures.sh [--check]
#
# The RPC comes from the environment only (never written to a file). The pools' accounts
# (fixtures/venues/*.accounts) are in the tree, read on 21 Oct 2026; the dumps below are the
# programs as they ran then (sha256 of each dump). A venue upgraded since dumps to another hash:
# the script says so, and the tests then run the upgraded program against the 21 Oct accounts.
# --check only compares the dumps already there.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
DIR="$ROOT/programs/tests/fixtures/venues"

# name, program id, sha256 of the 21 Oct 2026 dump.
VENUES=(
  "raydium_amm_v4 675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8 3bcf0828cf38e5f703adbf31dc1bf81f0a1254dcb176ca950ae52231c20e6d14"
  "raydium_cpmm CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C b6dbea8cefc2e7d490775ce6115e34a2ba480c0853c359e0bb4043b9381ef09a"
  "raydium_clmm CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK b1e8b3eb1c0027b3c05b059dec773661cbe93e2b304c1166e575b502ee2ca6d2"
  "pump_amm pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA 6a7ad34596d83b4c435b32f458a56e718e9f82eb4ad38e4316616d15d24d8ff7"
  "pump_fees pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ 4f2955d0f6b40cc9ff7daa18ccad7d19119ba1249759f4e7e3bb691beee6b35e"
)

check_only=0
if [[ "${1:-}" == "--check" ]]; then check_only=1; fi
if ((check_only == 0)); then
  [[ -n "${SOLANA_RPC_URL:-}" ]] || { echo "SOLANA_RPC_URL: a mainnet RPC (from the environment)" >&2; exit 2; }
  command -v solana >/dev/null || { echo "the Solana CLI is not on PATH" >&2; exit 2; }
  mkdir -p "$DIR"
fi

status=0
for entry in "${VENUES[@]}"; do
  read -r name id want <<<"$entry"
  so="$DIR/$name.so"
  if ((check_only == 0)); then
    solana program dump --url "$SOLANA_RPC_URL" "$id" "$so" >/dev/null
  fi
  if [[ ! -f "$so" ]]; then
    echo "missing $name ($id)"
    status=1
    continue
  fi
  got="$(sha256sum "$so" | cut -d' ' -f1)"
  if [[ "$got" == "$want" ]]; then
    echo "ok       $name $(stat -c %s "$so") bytes"
  else
    echo "UPGRADED $name: $got (the tests' accounts are of $want's day)"
    status=1
  fi
done
exit "$status"
