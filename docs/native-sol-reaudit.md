# Native SOL fixes: re-audit

A re-audit of the fixes for the findings in `docs/native-sol-audit.md` (branch `native-sol-audit`,
`8300fb2`):

| Repo | Branch | Commit |
| --- | --- | --- |
| Programs | `native-sol` | `d34148c` (fixes `25956b1`…`d34148c` on top of the merge `08fe0f9`) |
| Monorepo (SDK, server, keepers, `pnpm admin native`, runbook, integrators doc) | `native-sol` | `327d744` |
| Public SDK | `native-sol` | `29a4ef0` |

This was a read-and-test review. Nothing was deployed, pushed or sent to mainnet or devnet. The new
adversarial tests are in `programs/tests/tests/reaudit_native_sol.rs` on this branch.

## Verdict

**Conditional GO.** Nothing found steals lamports, breaks the pool invariant, takes a pool below
rent, pays twice, or lets anyone get more SOL than the books owe. The two new value-moving paths
(N-4, N-5) are lamport-exact. The enforced revert order can't be griefed.

Two small on-chain changes should land before the window:

- **R-1 (Low-Medium).** The N-4/N-5 lamport fallback also fires inside CPI callers that count only
  bridged SOL:
  - the router's old `swap` skips the integrator's fee and Bordrless's share on every native-pool
    sell;
  - a bridged hook vault or companion on a native pool misbooks its sale.

  The fix is to pay the fallback only at the top level, to the delivery's owner.
- **R-2 (Low).** The N-7 reserved-key list misses `Sysvar1111111111111111111111111111111111111`.

If R-1 is not fixed, two operational mitigations are needed instead:

- every companion and vault step is executed (not merely proposed) before `unpause`;
- router integrators move to the router's `swap_v2`.

R-3 to R-6 are off-chain or documentation fixes; do them before or soon after the window.

## Fix status

| ID | Status | Notes |
| --- | --- | --- |
| N-1 | Confirmed | `vault.is_some()` is exactly `rewards_on()`. Without holder rewards a donation stays on the config, uncounted; with them it is wrapped as rewards. A one-lamport donation to a burn-only kit no longer blocks the revert (also `holds_revert_order_survives_donations`). |
| N-2 | Confirmed | Only pending cuts whose account the output hook is also passed (writable) are paid early. Every later CPI passes both sides of that move: `after_swap` gets the pool and every pool extra, where all delta accounts come from, and the output transfers get the pool and the output extras. The other cuts still wait to the end. Buy and sell results and books match `swap_v2` exactly (`holds_old_and_v2_round_trips_match_on_a_kit_launch_pool`). |
| N-3 | Confirmed | `open_vault` sets native only when the launch is native; nothing is pending at open, and selling slot owners are funded to rent(0) there. |
| N-4 | Confirmed | Lamport-exact (see below). See R-1 for CPI callers. |
| N-5 | Confirmed, but R-1 and R-3 | Exact for native launches. On held or half-migrated coins the wrapped cuts have no fallback (R-3). See R-1 for CPI callers. |
| N-6 | Confirmed | `grow_for_quote_cut(.., 0)` is paid by the creator address (signed with its seeds) before `grow_for_native`, and the spare is computed after both grows. |
| N-7 | **Incomplete** | On chain, one reserved key is missing (R-2). In the server, the author-claim check is wrong (R-4). The creator claim and the CLI's partner-key check are correct. |
| N-8 | Confirmed | Order: vault and companion, then launch, then pool. It can't be griefed (below). |
| N-9 | Confirmed | `EXCLUDED_POOL` is refused whatever the arguments. A foreign hook needs `allow_foreign_hook`; the CLI hard-codes `false`. |
| N-10 | Confirmed | Migration tops up an owner below rent(0) from the admin before unwrapping `pending_sol`. A vault that opens native is funded at open. |
| H-1, I-5 | Confirmed | The companion reads the script PDA (`Account<Script>`: owner, discriminator, seeds) only when it is passed last. A script companion launching native without it is refused (`MissingAccount`), and a listed script is refused (`ScriptNotNativeReady`). The script program is fixed at `create_script`. The server refuses held scripts and uses `launchScript`. See R-5 and R-6. |
| H-2 | Confirmed | It runs a real simulation of `buffer_all` → `rebalance_pool` and requires `NATIVE_HELD_WRAP`. It fails open on errors other than 101 (I-R2). |
| H-3 | Confirmed | The keeper wraps only when the pool is native and both the launch and the companion are bridged. It reads this once, and the fallback compute limit counts the wrap. |
| H-4 | Confirmed | `bufferedAll: 4`; the held-coin revert notes; `verify` compares the bridged launch holding. |
| O-1 | Confirmed | The bridge accounts go along only when the launch is not native, so held coins keep them. The Rust default is `false`. |
| O-2 | Confirmed | Kit SOL is checked against `seen − totalClaimed` and the snapshot. Missing data is FAIL, the snapshot is keyed by pool, the excluded pool must stay bridged, and pending proposals exit 1. A minor gap remains (I-R3). |
| O-3 | Confirmed | Order: stop keepers, pause, snapshot, upgrade, IDLs, migrate, verify, set-native-launches, deploy, unpause. The CLI `revert` runs vault → companion → launch → pool with the extra accounts. Doc nits are in R-6. |
| O-4 | Confirmed | Changelog, integrators doc, `router.swapV2` example, and a native-aware `mmRun`. See R-3 for held coins. |
| O-5 | Confirmed, but R-4 | Sell-all-and-claim creates the bridged holding, and no-op rebalances are skipped. The empty-wallet check is right for creators and wrong for authors (R-4). |
| I-1, I-2 | Confirmed | `is_companion_creator` is used by the launch revert. A re-run of `migrate_pool_to_native` applies a new target and emits it. |

## New findings

### R-1 (Low-Medium): the lamport fallback reaches CPI callers that count only bridged SOL

**Location:**

- The DEX: `bordrless_swap/src/instructions/swap.rs:761-768, 890` and `liquidity.rs:443, 504`, which
  pay what the buffer lacks as lamports to the *signer*.
- The client: `client.rs:246, 286` now pass the trader and provider writable, and every program that
  builds the old swap with this client inherits that.

**Scenario:** before the fix, an old-path sell the buffer couldn't cover failed with `BufferShort`,
which was safe. Now it succeeds and part or all of the delivery arrives as lamports on the trader.
Programs that call the old swap by CPI and measure only the bridged-SOL holding misbook that part:

- **Router (live at unpause).** `bordrless_router::swap` charges the integrator's fee, and Bordrless's
  20% share, on the trader's bridged-SOL gain (`lib.rs:408-413`). At buffer target 0 that gain is 0,
  so a routed old sell on a native pool pays no fee and still succeeds.

  The docs say "the router's old `swap` keeps working". Any app still on it loses its fee, and the
  treasury loses its share.
- **hook_vault.** A vault left bridged on a native pool is one opened before native launches whose
  `migrate_vault_to_native` is still a Squads proposal at unpause. Its `execute` (permissionless)
  sells through the old swap with the slot owner as a writable trader (`steps.rs:123`).

  The vault measures `sol_of` as bridged SOL only (`:323-326`). It books `got = 0`, pays its wallet
  nothing, and the proceeds stay stranded on the slot owner; no later step counts them.
- **Companion.** A bridged Snowball on a native pool sells through `creator_sell` (`snowball.rs:434`)
  and measures `sol_held` as bridged SOL, so the pot gets nothing. The lamports become the creator
  address's spare, which `turn_native` (`native.rs:74`) books to the *beneficiary* at the
  companion's later migration.

  So the dev can turn the pot's sales into their own SOL by cranking sells in that state. Third-party
  scripts that pass the creator writable to the old swap behave the same way (the companion's step
  checks allow lamports to rise).
- **Delegates.** A delegate that sells a user's tokens to the user's holding (`trader ≠ recipient`)
  now receives the lamport part itself.

**Proof:**

- `finding_a_routed_old_sell_on_a_native_pool_pays_no_integrator_fee`: the sale delivered more than
  1 SOL, all as lamports; `IntegratorFeePaid` shows 0/0/0.
- `finding_a_bridged_vault_on_a_native_pool_strands_its_sale`: `SlotSold.got == 0`, the wallet got
  nothing, and the slot owner's lamports rose by exactly `delivered_out`.

**Fix:** pay the fallback only when the instruction is top level
(`get_stack_height() == TRANSACTION_LEVEL_STACK_HEIGHT`) and, for a sell, the trader is the delivery
holding's owner (`recipient`). Otherwise return `BufferShort` as before. CPI callers then fail safe,
as they did before the fix. Their native paths (`swap_v2`) are unaffected.

### R-2 (Low): `RESERVED_ACCOUNT_KEYS` misses the sysvar owner id

**Location:** `bordrless_launch/src/constants.rs:197`.

**Scenario:** the list has 30 keys. agave-reserved-account-keys 4.3 has 31, all active, including
`sysvar::id()` (`Sysvar1111111111111111111111111111111111111`). hook_vault's own `RESERVED_KEYS` has
all 31.

`create_partner_config` therefore accepts that key as the partner. Every SOL claim of every native
launch from the config then fails, and the creator's fees stay locked until an admin revert (the
original N-7 scenario).

**Proof:** `finding_the_sysvar_owner_key_is_accepted_as_a_partner_and_locks_claims`. The Clock sysvar
is refused and `Sysvar111…` is accepted. The runtime demotes the key, so both claims fail with
`PartnerAccountsMissing` (with and without the partner's accounts) and the fees are unchanged.

**Fix:**

- Add the key, or share one 31-key list between the two programs.
- An executable program id is still accepted as a partner. Whether crediting one fails depends on
  the runtime's executable-lamport rule. The CLI flags such keys; keep that check for every existing
  config.

### R-3 (Low): old-path sells of held or half-migrated coins fail at target 0

**Location:** `swap.rs:761` (`require!(buffer >= wrapped_cuts)`). Off chain: SDK `launch.swap` and
`mmRun.ts:180`, and `native-sol-integrators.md:84-89` and the public `04-trading.md` ("never fails").

**Scenario:** with the launch bridged, the launchpad names wrapped cuts (bridged holdings). The old
swap has no bridge accounts and no fallback for them. Outside a `buffer_all` window, every old-path
sell of the 13 held coins fails with `BufferShort`. The SDK's `launch.swap` and the market maker build
exactly that; the site (`swap_v2` with the bridge) is fine.

**Proof:** `finding_an_old_sell_of_a_held_coin_fails_at_target_zero`. The old sell fails;
`swap_v2_with_bridge` sells.

**Fix:** have the SDK build `swap_v2` with the bridge whenever the pool is native, and correct both
docs.

### R-4 (Low): author claims refused whenever a party's wallet is empty

**Location:** monorepo `apps/server/src/marketplace/author.ts:116-117`.

**Scenario:** a native author claim is dropped if the creator's or partner's wallet holds 0 lamports,
whatever that party's share. On chain, `pay_sol` only needs `lamports + part ≥ rent`, so claims that
would succeed are refused. The message ("claim once more fees have accrued") doesn't help when the
share is already above rent.

**Fix:** use `walletsLeftBelowRent` with each party's share, as `creator.ts` does.

### R-5 (Low): a script companion's launch can exceed the transaction size limit

**Location:** monorepo `apps/server/src/launch/prepare.ts:354`.

**Scenario:** appending the script PDA adds 33 bytes. A launch from a config whose hook has the
maximum four registry extras, with the longest metadata, measured 1,246 bytes, over the 1,232 limit.
It fails with a misleading "list fewer accounts" message. The Lossback hook (three extras) still fits.

**Fix:** append the script PDA only while native launches are on, or cap the extras or metadata for
script launches.

### R-6 (Info): stale documentation

- **Script companions.** SDK `README.md:561` and `SdkPages.tsx:740` still say to use
  `companion.launch(...)`. Followed as written, the launch fails with `MissingAccount` once native
  launches are on; it should be `companion.launchScript`.
- **Old-path sells.** SDK `native.ts:16, :87` and the site's `prompt.ts:50` still say they fail with
  `BufferShort`.
- **Runbook:**
  - line 67 points to `ops/native.ts` for `NATIVE_SCRIPT_HOLD`, which now lives in shared code;
  - the rollback never says `unpause`;
  - "dry-run `migrate` against today's chain" holds only after the upgrade.

### Info

- **I-R1 (pre-existing).** A buffer target at or above a native launch pool's books puts all its
  quote in the buffer. Then:
  - `swap_v2` sells fail (`LamportsShort`);
  - old sells and old buys fail on their lamport cuts;
  - a rebalance doesn't help, since it moves toward the target.

  With the new CLI `set-buffer-target`, never set a launch pool's target near its books. Consider
  letting `swap_v2` unwrap a shortfall from the buffer (the pool is the `unwrap_sol` user).
- **I-R2.** H-2's simulation counts any error other than 101 (for example an unfunded payer) as "deployed".
  Expect the specific refusal instead.
- **I-R3.** `pause`, `unpause` and `set-native-launches` proposed to Squads exit 0. `snapshot` refuses
  unless the DEX is paused, which limits the risk.
- **I-R4.** `NOT_NATIVE_READY_SCRIPTS` is a fixed list of two programs. Workshop approval must check
  native readiness for new scripts, or the list needs an upgrade.
- **I-R5.** Off-chain leftovers:
  - `swap.swapFor` never passes the bridge, so it can't trade held pools;
  - a half-migrated sell-all-and-claim leaves the kit rewards as bridged SOL;
  - `snapshot` aborts on a launch-hook pool without a launch account.

  Rehearse on the cloned validator.

## The new value-moving paths (N-4, N-5)

Each question below was checked against the code and in tests.

- **Exact amounts.** The amount owed is fixed by the books before its form is chosen:
  - a sell's `to_deliver` is computed from the curve, the cuts and `held_back`;
  - a withdrawal's `quote_out` comes from `withdraw_for_lp`.

  The split is integer arithmetic with no rounding:
  - a sell takes `from_buffer = min(to_deliver, buffer − wrapped_cuts)` and
    `to_trader = to_deliver − from_buffer`;
  - a withdrawal takes `held = min(quote_out, buffer)` and `rest = quote_out − held`.

  The buffer falls by the vault's *measured* decrease (sell) or by `held` (withdrawal). The pool's
  lamports fall by exactly `to_trader` or `rest` plus the lamport cuts. The bridged and lamport parts
  always add up to the delivery, which is identical to `swap_v2`'s for the same trade.

  Tests: `holds_old_sell_with_a_partial_buffer_is_lamport_exact` (six buffer and cut mixes, each
  against `swap_v2`) and `holds_lp_withdrawal_with_a_partial_buffer_is_lamport_exact`.
- **Invariant and rent.** When the fallback pays anything, the buffer is exhausted. So the lamport
  part is at most the pool's lamport quote, and `lamports ≥ rent + books − buffer` and
  `vault ≥ buffer` are re-checked (`check_native`) after the move. A pool can't drop below rent. The
  recipient is rent-checked first (`RecipientBelowRent`, nothing moved).
- **No double pay; the shortfall can't be inflated.** `to_trader` is moved once, after every CPI.
  `delivered_out` counts both parts, and `min_amount_out` holds against the sum. Naming the pool's
  quote vault as the delivery holding gains the trader nothing beyond its delivery
  (`holds_old_sell_delivered_to_the_quote_vault_pays_nothing_extra`). The trader is in the
  forbidden list for cuts.
- **Signer and writable.** The lamport recipient is the instruction's signer and must be writable,
  else `BufferShort`. Read-only still works while the buffer covers the delivery
  (`holds_old_sell_shortfall_refusals_move_nothing`).
- **Reentrancy.** The runtime refuses DEX → hook → DEX. The buffer is read after the last pool-hook
  CPI. Only bridged-SOL token transfers run between that read and the measurement, and bridged SOL
  has no token hook. The direct lamport move follows every CPI, and only `emit_cpi`, which passes
  neither side, comes after it.
- **Conservation under stress.** `holds_walk_of_the_lamport_fallbacks_conserves_value` runs 300
  random steps:
  - old sells and withdrawals, sometimes read-only, at random targets (zero included, up to above
    the books), with random wrapped and lamport cuts;
  - rebalances;
  - donations of lamports and of bridged SOL;
  - old and new buys and new sells.

  After each step the invariant is an equality with the donations, and lamports plus the bridged
  supply are conserved over every account involved. Both fallbacks and the read-only refusals ran.
  The audit's 400-step walk still passes on the fixed code.
- **Arbitrage.** `swap` and `swap_v2` price off the same books, and the form never changes the
  result. On a kit launch pool, a buy and a sell through the old path match `swap_v2` exactly at
  buffers of 0, 0.1, 1 and 3 SOL: the trader's net, its tokens, the books, the holder fees and the
  creator fees (`holds_old_and_v2_round_trips_match_on_a_kit_launch_pool`).

## Revert order (N-8): not griefable

- **Pool.** `revert_pool_to_bridged` of a launchpad pool reads the launch at its PDA:
  - it is refused while the launch is native, and also when the launch isn't passed;
  - a launch the launchpad doesn't own (none exists) has nothing to wait for;
  - an outsider can only fund the address, which leaves it system-owned and unblocks the step.
- **Launch.** `revert_launch_to_bridged` reads the vault PDA always, and the companion PDA for a
  companion coin:
  - only hook_vault and the companion can create accounts there;
  - `create_vault` needs the mint's keypair before launch;
  - a vault turns native only at its open (the coin's own hook) or through the upgrade authority.
- **Each revert step.** Every step only adds, or wraps what was added:
  - the vault wraps `pending_sol`, and only the vault can debit slot owners;
  - the companion wraps its spare;
  - the kit skips the move without rewards (N-1);
  - the launch and the pool wrap books or lamports above rent.

  Donations can't make any of them fail.
- **Flag reads.** The launch-flag parser agrees with Borsh for 2,000 random layouts and refuses short
  data or bad tags (`holds_the_launch_flag_parser_matches_borsh`). The vault and companion offsets
  are pinned by `native_sol.rs`.

**Proof:** `holds_revert_order_survives_donations`. Lamports were donated to the launch, a burn-only
kit, the pool, the vault address and the companion address, and bridged SOL to the pool's vault.
Then:

- out of order, each step is refused (`LaunchStillNative`; `VaultStillNative` without the vault
  address);
- in order, every step lands;
- the coin re-migrates and trades.

## Tests

- **Build:** Agave 4.3.0, platform-tools v1.57, SBF v3. All 15 programs and the 6 test fixtures were
  built fresh from `d34148c`.
- **Suite on `d34148c`:** `scripts/solana/programs.sh test` gives **873 passed, 0 failed, 3 ignored**.
- **With `reaudit_native_sol.rs`:** **885 passed, 0 failed, 3 ignored**. The file has 12 tests:
  - 8 `holds_`;
  - 4 `finding_` (R-1 twice, R-2, R-3), each asserting today's behaviour.
- **Lint:** `programs.sh clippy` (workspace, `-D warnings`) is clean, and the new file is
  rustfmt-clean. The workspace's `cargo fmt --check` shows earlier diffs in about two dozen untouched files,
  which looks like rustfmt drift on the `stable` channel.
- **Monorepo `327d744`:**
  - SDK: 414/414;
  - shared: 188/188;
  - server: 797 passed, 1 failed, 1 skipped (the known stale `studio/knowledge.test.ts` corpus);
  - `tsc --noEmit` is clean for the SDK, shared and the server.
- **Public SDK `29a4ef0`:**
  - SDK 414/414 and shared 188/188;
  - tsc is clean;
  - all 11 IDLs are byte-identical to the programs repo's and the monorepo's.

## Fix status (after the re-audit)

| ID | Status | Where |
| --- | --- | --- |
| R-1 | Fixed | DEX: the old `swap`'s lamport fallback pays only a top-level instruction (`get_stack_height() == TRANSACTION_LEVEL_STACK_HEIGHT`) whose trader is the delivery holding's owner; `remove_liquidity`'s likewise only a top-level provider owning the quote holding. Otherwise `BufferShort`, nothing moved. Tests: `holds_a_bridged_vault_on_a_native_pool_fails_safe`, `holds_a_routed_old_sell_on_a_native_pool_fails_safe` (the same sell sent top level still lands). |
| R-2 | Fixed | `RESERVED_ACCOUNT_KEYS` has 31 keys, `Sysvar111…` included, equal to hook_vault's list. Test: `holds_the_sysvar_owner_key_is_refused_as_a_partner`. |
| R-3 | Fixed off chain | SDK `launchKeysOf(l, hook, { poolNative })`: `launch.swap` builds `swap_v2` with the bridge for every native pool, held coins included; the market maker passes it. Chosen over a nonzero buffer target because a target needs the keeper to keep the buffer topped up, and anything above the books locks `swap_v2` sells (I-R1); `swap_v2` needs no state. The old sell still fails safe. Test: `holds_a_held_coin_sells_through_swap_v2_at_target_zero`. Docs corrected. |
| R-4, R-5, R-6, I-R2, I-R3 | Fixed | Monorepo (server, CLI, SDK, docs). |
