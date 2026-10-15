# Native SOL upgrade: security audit

Audit of the `native-sol` branch (`118aa86`) against `origin/main`. The upgrade moves Bordrless from a
bridged-SOL quote to native SOL across six programs: the DEX (`bordrless_swap`), the launchpad, the
kit, the companion, `hook_vault` and the router. About 1,018 SOL across 150 coins is at stake.

Secondary scope, on branch `native-sol` of the monorepo:

- the admin CLI (`pnpm admin native`);
- the trade, claim and keeper code in `apps/server`;
- the native builders in `packages/sdk`;
- the runbook (`docs/native-sol-runbook.md`) and this repo's `docs/native-sol-migration.md`.

Added during the audit: the held-coins branches.

- Programs `native-sol-held` (`963ad6d`), which adds the permissionless DEX instruction `buffer_all`.
- Monorepo `native-sol-held` (`a22c68a`): the keeper's held-step wrapping, and the CLI's migration
  of the held pools only.

They are covered in "Addendum: held coins and `buffer_all`" below.

This was a read-and-test audit. Nothing was deployed, and no mainnet or devnet transaction was sent.
The audit tests are in `programs/tests/tests/audit_native_sol.rs` on branch `native-sol-audit`. The
`buffer_all` tests are in `programs/tests/held/audit_buffer_all.rs`; they are not compiled there,
and the file says how to run them on `native-sol-held`.

## Verdict

**Conditional GO.** No critical or high finding. The fixes in "Fix first" below must land before
the window, and the native-launch items before `set_native_launches(true)`.

### What holds

The core lamport accounting is sound:

- **Conservation walk.** A seeded walk of 400 random operations on a native pool keeps two things
  exact after every instruction, failed or not:
  - the pool invariant, as an *equality*: lamports = rent + books − buffer + donations, and vault =
    buffer + bridged donations;
  - the total value: lamports outside the bridge plus the bridged-SOL supply.

  The walk mixes:
  - both swaps, both directions, with random lamport, wrapped and base cuts;
  - rebalances by strangers and random buffer targets;
  - liquidity in and out, and both fee collections;
  - donations of lamports and of bridged SOL;
  - migrate/revert round trips.
- **Rehearsal.** The cloned-mainnet rehearsal conserved every lamport for 150 coins (1,018,364,180,618
  lamports moved form).
- **Runtime rules.** Every direct lamport debit is on an account the debiting program owns. Every
  credited account must be writable. Every direct move that precedes a CPI passes both sides into
  it (the `trailing` account), or happens after the last CPI.
- **Admin instructions.** Every migrate and revert is admin- or upgrade-authority-only, needs the
  DEX paused, and is idempotent.
- **Accounts.** No account substitution, aliasing or fake account was found that moves value. The
  same trade costs exactly the same through `swap` and `swap_v2`, whatever the buffer.
- **Compatibility.** Error codes and events are only appended. The old instructions behave
  identically on bridged pools.
- **CLI safety.** The CLI cannot send to mainnet without `--send` *and* `--mainnet-i-am-sure`.

### What is wrong

Liveness, compatibility and operations gaps:

- a one-lamport donation permanently blocks the rollback of any coin whose kit has no holder rewards
  (11 of the 66 live kits);
- old-path buyers recover most of their own holder fee;
- new hook-vault coins cannot sell;
- liquidity providers of native pools cannot withdraw at buffer target 0;
- the old `swap`'s sells do not "keep working";
- `verify` does not check the kit's SOL;
- every native trade write-locks the bridge's global accounts;
- the runbook misses steps.

### Fix first (before the window)

1. **N-1:** kit `set_native(false)` must not require a reward vault when there are no holder
   rewards. The rollback is the safety net, and today anyone can disable it, for 1 lamport, for every
   coin whose kit has no holder rewards (11 of the 66 live kits, and any future one).
2. **O-1:** `swap_v2` builders (SDK, server, and the Rust `swap_v2_with_base_slice` default) must not
   pass the bridge accounts unless a cut needs wrapping. Otherwise every native trade platform-wide
   write-locks the bridge's SOL wrapper, SOL vault and bridged-SOL mint.
3. **O-2, O-3:** fix the runbook and `verify`:
   - pause and stop the keepers *before* `snapshot`;
   - upgrade the programs and re-upload the IDLs in the window;
   - deploy the native-aware backend, site and SDK before `unpause`;
   - make `verify` check the kit's lamports against `seen − total_claimed` and the snapshot's vault;
   - make `verify` treat missing data as FAIL.
4. **N-4:** decide the liquidity-provider policy for the 9 graduated launch pools and the hook-less
   pool. Give them a non-zero buffer target, or add a native `remove_liquidity`, and add a CLI
   `set-buffer-target`. Today the keeper unwraps every deposit and liquidity providers cannot
   withdraw.

### Fix before `set_native_launches(true)`

- **N-3:** hook vaults have no native-at-creation path.
- **N-6:** an old (300-byte) unlaunched companion cannot launch.
- **N-2:** the holder-fee timing on the old path. Fix it, or disable or document old-path buys on
  native kit coins.
- Revoke, or flag, approved Workshop scripts that are not native-ready (I-5 and H-1). New launches
  with a held script break once native launches are on.

### For the held coins (if `native-sol-held` ships)

- Deploy the DEX built from `native-sol-held`, and the keeper with `vaultWrapped`, *before* `migrate`
  touches the 13 held pools. Better, have the CLI prove that `buffer_all` exists first (H-2).
- `buffer_all` itself is sound (see the addendum). The fixes are off-chain only.

## Findings

Severity, then location (on `native-sol`), then the scenario, the proof and the fix. Findings N-1 to
N-8 each have a test that **passes today and asserts the faulty behaviour**, so it fails once fixed.
N-9 and N-10 rest on reasoning, and the O and H findings come from the off-chain review.

| ID | Sev | Title |
| --- | --- | --- |
| N-1 | Medium | A lamport donation to a native kit without holder rewards permanently blocks `revert_launch_to_bridged` |
| N-2 | Medium | An old-path buy on a native launch pool shares the buyer's own holder fee with it |
| N-3 | Medium | Hook vaults are created bridged, so new vault coins on native launches cannot sell |
| N-4 | Medium | LPs of a native pool cannot withdraw at buffer target 0 (permissionless rebalance unwraps their deposit) |
| N-5 | Medium | The old `swap`'s sells on native pools are effectively broken and griefable |
| O-1 | Medium | `swap_v2` builders write-lock the bridge's global accounts on every native trade |
| O-2 | Medium | `verify` never checks the kit's SOL, and has silent skips |
| O-3 | Medium | Runbook order and missing steps |
| O-4 | Medium | SDK `launch.swap` silently became `swap_v2` for native launches |
| N-6 | Low | An old (300-byte) unlaunched companion cannot launch while native launches are on |
| N-7 | Low | A partner key that can't take lamports locks every native claim of its config's launches; empty party wallets block small claims |
| N-8 | Low | On-chain ordering not enforced: reverting a pool before its launch stops every trade |
| N-9 | Low | No on-chain guard against migrating the excluded pool or any foreign-hook pool |
| N-10 | Low | `hook_vault` slot-owner rent edge cases |
| O-5 | Low | CLI and keeper smaller issues |
| H-1 | Medium | New launches with a held (non-native-ready) script are native from creation and every script step fails |
| H-2 | Low-Medium | Nothing checks that `buffer_all` and the wrapping keeper are deployed before the 13 held pools migrate |
| H-3 | Low | Keeper `vaultWrapped` ignores the launch's form and the hold list; double read; fallback CU ignores the wrap |
| H-4 | Low | Held-coin runbook and `verify` gaps |
| I-1…I-6 | Info | See the end |

---

### N-1 (Medium): a one-lamport donation blocks the rollback of a native kit without holder rewards

**Location:** `programs/bordrless_kit/src/instructions/native.rs:209-224`. Reached through
`programs/bordrless_launch/src/instructions/native.rs:246-274` (`revert_launch_to_bridged` →
`kit_set_native(false)`).

**Scenario:** `set_native(false)` computes `moved = native_vault(lamports)` (the config's lamports
above `lamports_base`). If that is above 0, it debits them and then asks for the reward vault:
`vault.as_ref().ok_or(MissingRewardVault)`. A kit with no holder rewards has no vault. Anyone can
send its config a lamport, and afterwards `revert_launch_to_bridged` fails for that coin forever.
Only a program upgrade can bring the rollback back.

In the rehearsal's account dump, 11 of the 66 kit configs have no holder rewards (modules 2, 4, 6, 10
and 14). A griefer can disable the documented rollback for each of them, and for every such future
coin, for one lamport apiece (a 1-lamport transfer to a rent-exempt account is allowed).

**Proof:** `finding_kit_donation_blocks_launch_revert_without_holder_rewards`. After a 1-lamport
system transfer to the kit config, the revert fails with `KitError::MissingRewardVault`.

**Fix:** without holder rewards there is nothing to wrap. Clear the native flag and leave the
uncounted lamports on the config: skip the move when `!config.rewards_on()`. With holder rewards, a
donation is counted as rewards and wraps back fine.

### N-2 (Medium): old-path buyers recover most of their own holder fee

**Location:** `programs/bordrless_swap/src/instructions/swap.rs:524-536` (an old-path buy queues its
lamport cuts in `pending`) and `:852-855` (they are paid after every CPI). Also
`programs/bordrless_kit/src/instructions/callbacks.rs:71-75` (a native kit's vault is its lamports).

**Scenario:** the runtime rule that forces the old swap to defer its lamport cuts also defers the
holder fee to after step 6.

1. On an old-path buy of a native launch pool with holder rewards, the base-out transfer (step 6)
   runs the kit hook first. The kit syncs without this swap's holder fee and settles the buyer at its
   new, larger balance.
2. Then the fee lands on the kit config.
3. The next sync shares it with every holder, the buyer included.

On `swap_v2`, and on any bridged pool, the fee reaches the vault in step 2, before the buyer's tokens
arrive, so the buyer gets none of it.

A buyer that ends up with a share `s` of the eligible supply recovers about `s` of its own holder fee
from the existing holders. The cost is a wrap, which is 1:1.

**Proof:** `finding_old_path_buyer_shares_its_own_holder_fee`. A 10 SOL buy pays a 0.1 SOL holder
fee. Through the old swap the buyer claims back 69,503,271 lamports (about 70%). Through `swap_v2` it
claims 0.

**Fix:** pay the holder (buy-side) lamport cut before the output transfer on the old path. Two ways:

- **Unwrap through the trader.** Unwrap the trader's bridged-SOL cut with the trader as the bridge
  user, then send it with a system transfer. Every move is then a CPI. This needs the trader writable;
  fall back to today's behaviour when it isn't.
- **Pay right after step 2.** Pay the pending cuts right after step 2, and pass the pool and every
  pending destination into each later CPI.

Until fixed, the site and every integrator should use `swap_v2`.

### N-3 (Medium): new hook-vault coins on native launches cannot sell

**Location:**

- `programs/hook_vault/src/instructions/create.rs:216` (`v.reserved = [0; 64]`: a new vault is
  always bridged);
- `programs/hook_vault/src/instructions/native.rs:44-90` (the only way to native needs the
  upgrade authority and the whole DEX paused);
- `programs/hook_vault/src/instructions/steps.rs:98-126` (a bridged vault sells through the old
  swap).

**Scenario:** once `set_native_launches(true)` is on, a coin made with a hook vault launches native
(pool, launch, kit), but its vault stays bridged. Every slot sale then goes through the old swap,
which pays from the pool's buffer. At target 0 nothing ever fills a fresh native pool's buffer, so
every sale fails with `BufferShort`.

The coin's hook cut piles up unsold in the slot owner's holding. No SOL is lost, but the vault's
policy (sell-for-SOL, buy-burn) is dead until someone pauses the whole DEX to migrate it. There are
no hook vaults on mainnet today, so this affects only coins launched after the switch.

**Proof:** `finding_a_new_vault_coin_on_native_launches_cannot_sell`. The launch is native, the vault
bridged, and `execute` fails with `BufferShort`.

**Workaround:** if `native-sol-held` ships, a crank can send `buffer_all` → `execute` →
`rebalance_pool` in one transaction.

**Fix:** make `create_vault` (or `open_vault`) native when the coin's launch is native, as the
companion's `native_at_launch` does. Or allow `migrate_vault_to_native` on a vault with no pending
SOL without a DEX pause.

### N-4 (Medium): LPs of a native pool cannot withdraw at buffer target 0

**Location:**

- `programs/bordrless_swap/src/instructions/liquidity.rs:435-437` (`remove_liquidity` pays only from
  the buffer);
- `programs/bordrless_swap/src/instructions/pool.rs:948-988` (`rebalance_pool` is permissionless);
- monorepo `apps/server/src/ops/native.ts:219` (`--buffer-target` defaults to 0);
- monorepo `apps/server/src/keeper/rebalance.ts:34-40` (the keeper drives buffers to their targets).

**Scenario:** there is no native `remove_liquidity`. A deposit's bridged SOL joins the buffer. The
next rebalance, by the keeper or anyone, unwraps it to lamports toward target 0. The LP's
`remove_liquidity` then fails with `BufferShort`.

The LP has two ways out:

- the admin raises the buffer target and someone rebalances;
- the LP buys through the old swap to refill the buffer, removes, and sells back, paying two swaps'
  fees.

**Who it hits on mainnet**, per the off-chain review's read-only scan:

- the 9 graduated launch pools (anyone can add liquidity after graduation);
- the hook-less KTEST pool;
- pool `S8wErr…`, which has about 0.0027 SOL of third-party liquidity.

**Proof:** `finding_an_lp_of_a_native_pool_cannot_withdraw_at_target_zero`. Also `native_sol.rs`
`liquidity_on_a_native_pool_goes_through_the_buffer`.

**Fix:** pick one of:

- add a `remove_liquidity_v2` that pays lamports;
- or let `remove_liquidity` on a native pool top up its buffer shortfall from the pool's lamports,
  wrapped by the provider (a signer), as `rebalance` does;
- or give pools with outside liquidity providers a target that covers withdrawals.

Also add `native set-buffer-target` to the CLI, and make `apps/server/src/liquidity/prepare.ts` check
the buffer and explain the refusal.

### N-5 (Medium): the old `swap`'s sells on native pools are effectively broken and griefable

**Location:** `programs/bordrless_swap/src/instructions/swap.rs:731-738` (`BufferShort`) and
`pool.rs:948` (`rebalance_pool`, permissionless). The doc claims are in `docs/native-sol-migration.md`
§"What keeps working unchanged".

**Scenario:** at the recommended target of 0, an old-path sell succeeds only while bridged SOL from
earlier old-path buys sits in the buffer. Anyone, including the keeper, can rebalance it away first,
so the sell fails with `BufferShort`. Old-path *buys* on a fresh native launch pool fail with
`LamportsShort` until a `swap_v2` trade has given the pool lamports.

So "the old `swap` keeps working forever via the buffer" is not true for sells, nor for the first
buys. Integrators, bots, and the not-yet-redeployed site will see failures. No value moves: the books
never change.

**Proof:** `finding_anyone_can_empty_the_buffer_before_an_old_path_sell`. Also `native_sol.rs`
`the_old_swap_trades_a_native_pool_through_its_buffer` and `native_launch.rs`
`an_old_buy_on_a_fresh_native_launch_waits_for_lamports`.

**Fix:** pick one of:

- when the buffer is short and the trader is writable, have the old sell wrap the shortfall from the
  pool's lamports through the trader (as `rebalance` does);
- or keep a non-zero target sized to old-path volume.

Also correct the documentation, and tell integrators (aggregators) to move to `swap_v2` before the
window. If `native-sol-held` ships, an old-path seller can also send `buffer_all` → `swap` →
`rebalance_pool` in one transaction.

### N-6 (Low): an old unlaunched companion cannot launch while native launches are on

**Location:**

- `programs/bordrless_companion/src/state.rs:348-362` (`grow_for_native` requires
  `quote_cut_claimed.stored`);
- `programs/bordrless_companion/src/instructions/create.rs:243-260` (`process_launch` calls
  `native_at_launch` without growing for `quote_cut_claimed` first).

**Scenario:** a companion created before 2026-10-15 (300 bytes) that has not launched cannot launch
once `set_native_launches(true)` is on. The launch fails with `MissingAccount`.

The rehearsal's account dump has three such companions: `5apB2g9u…`, `2RM2ETFb…` and `5zbaQjwH…`.
Their funding is recoverable with `withdraw`, which the mint's signature allows before launch.

**Proof:** `finding_an_old_unlaunched_companion_cannot_launch_once_launches_are_native`. The launch
fails with `MissingAccount` when native launches are on, and succeeds (bridged) when they are off.

**Fix:** in `native_at_launch`, call `grow_for_quote_cut(.., 0)` first (nothing has accrued before
launch), paid by the creator address or the launcher.

### N-7 (Low): partner keys and empty wallets can block native claims

**Location:**

- `programs/bordrless_launch/src/instructions/claim.rs:567-583` (`pay_sol`: the recipient must be
  writable and end at or above rent);
- `claim.rs:613-650` (`partner_wallet_of`);
- `programs/bordrless_launch/src/instructions/launch_config.rs:345` (any non-default partner key is
  accepted).

**Scenario:** the `_sol` claims pay the partner's part as lamports straight to `PartnerFee.partner`.
If that key can never be writable (a sysvar or another reserved key, which the runtime demotes to
read-only), every `claim_creator_fees_sol` and `distribute_fees_sol` of every native launch from that
config fails. The creator's fees stay locked until the admin reverts the launch, which needs a DEX
pause.

The bridged claims paid the partner's *holding*, which exists for any owner. A malicious config
author can set this trap. An honest partner whose key is a program-owned PDA receives lamports it may
not be able to use.

Separately, an author or partner wallet with 0 lamports makes every claim whose part for it is below
rent(0) fail (`RecipientBelowRent`), until fees grow. This also applies to the creator's wallet on
the author's claim.

**Proof:** `finding_partner_key_that_cannot_take_lamports_locks_native_claims` (partner = the Clock
sysvar: `PartnerAccountsMissing`, and `ConstraintMut` on `distribute_fees_sol`, with the fees still
locked). Also `native_launch.rs` `author_and_partner_claims_pay_sol_to_every_wallet` (the
`RecipientBelowRent` case).

**Fix:**

- When a party can't be paid as lamports (not writable, executable, or below rent), keep its part
  owed on the launch (a counter) instead of failing the whole claim.
- Or refuse such partner keys in `create_partner_config`.
- Before the window, check every existing partner key.

### N-8 (Low): the on-chain revert order is not enforced

**Location:** `programs/bordrless_swap/src/instructions/pool.rs:896-940` (`revert_pool_to_bridged`
never looks at the pool's launch). The same holds for `revert_launch_to_bridged` and the companion
and vault.

**Scenario:** if the operator reverts a pool while its launch is still native, the launch hook
keeps naming the launch account (lamports) as the cut of a bridged pool. Every swap of the coin then
fails: `InvalidDeltaAccount` on the old swap, `NotNativePool` on `swap_v2`. This lasts until the pool
is migrated again. No value moves.

**Proof:** `finding_reverting_a_pool_before_its_launch_stops_every_trade`.

**Fix:** have `revert_pool_to_bridged` refuse while the pool's hook is the launchpad and the launch is
native (pass the launch, read-only). Do the same along the chain: launch before companion and vault.

### N-9 (Low): no on-chain guard against migrating the excluded or a foreign-hook pool

**Location:** `programs/bordrless_swap/src/instructions/pool.rs:852-889`.

**Scenario:** `migrate_pool_to_native` accepts any bridged-SOL pool. Pool `4PvPnTBy…`, with the
unknown hook `AwAvBe…`, would then pay quote-side cuts in lamports to whatever the hook names, and
nobody knows what that hook does. Only the CLI's deny list prevents it.

**Fix:** require `hook_program ∈ {None, launchpad}` on chain. If that is too rigid, add an explicit
`allow_foreign_hook` argument.

### N-10 (Low): hook_vault slot-owner rent edge cases

**Location:** `programs/hook_vault/src/instructions/native.rs:71-86` and `common.rs:314-321`
(`sol_of`).

**Scenario:** a slot owner's SOL is its lamports above rent(0).

- **Migration.** If the owner was never funded, migrating unwraps exactly `pending_sol`. A
  `pending_sol` below rent(0) fails the whole vault's migration (rent-state rule). One above it
  leaves `sol_of` short of `pending_sol` by rent(0), so later payouts or the revert can fail.
- **First sale.** The first native sale of an unfunded owner below rent(0) fails with
  `RecipientBelowRent`. A larger one silently keeps rent(0) of the proceeds as the owner's rent.

There are no hook vaults on mainnet, so this is latent.

**Fix:** fund each slot owner to rent(0) at `open_vault` (or migration), paid by the opener or the
admin, and treat that rent as never pending.

### O-1 (Medium): `swap_v2` builders write-lock the bridge's global accounts on every trade

**Location:**

- monorepo `packages/sdk/src/instructions.ts:904-910` (`withBridge` defaults to true);
- monorepo `apps/server/src/trade/prepare.ts:116-121`;
- `programs/bordrless_launch/src/client.rs` (`swap_v2_with_base_slice` defaults to the bridge).

**Scenario:** every native trade on the site write-locks the SOL wrapper, the bridge's SOL vault and
the bridged-SOL mint. These are platform-wide accounts. Every native trade on every pool then
serializes on them and shares one account's per-block write budget, and also contends with
wrap/unwrap users.

The launchpad hook never names a wrapped cut on a native launch, so the bridge accounts are never
needed there.

**Fix:** pass the bridge accounts only when the pool is native and its launch is not (a half-migrated
coin) or the hook is not the launchpad. The default should be false.

### O-2 (Medium): `verify` never checks the kit's SOL, and has silent skips

**Location:** monorepo `apps/server/src/ops/native.ts:179-182` (kit `seen` and `accPerShare` only),
`:258`, `:358-364`, `:402`, `:412`.

**Scenario:** a `set_native` that lost reward SOL would pass `verify`. `readCoin` never reads the kit
config's lamports, and the snapshot doesn't record the reward-vault balance.

Further skips:

- a kit or launch account that fails to load passes;
- a pool that fails to decode is skipped silently;
- `bufferQuote > books` passes;
- the snapshot is keyed by mint, so a second pool of the same mint would null the checks;
- `companionSetAside` and `vaultPending` in the snapshot are never compared;
- a native denied or foreign-hook pool isn't flagged.

On mainnet's rent (5,080 lamports a byte), the strict vault-slot equality (`:187-193`) would report
false failures. There are no vaults today.

Steps proposed to Squads are counted as done (`:297-299`, `:472-475`), so the run exits 0 with
companions or vaults still bridged.

**Fix:**

- assert `kit lamports − lamportsBase ≥ seen − totalClaimed` and `≥` the snapshot's vault;
- key the snapshot by pool;
- treat missing data as FAIL;
- compare every snapshot field;
- assert the excluded pool stays bridged;
- exit non-zero while proposals are pending.

### O-3 (Medium): runbook order and missing steps

**Location:** monorepo `docs/native-sol-runbook.md:29-30`, and this repo's
`docs/native-sol-migration.md`.

**Order:** `snapshot` runs before `pause`. Trades, claims and keeper steps between them, and the
claims and companion steps that still run while the DEX is paused, move kit `seen` and launch
holdings. `verify --snapshot` then reports false failures.

**Missing steps:**

- the program upgrades and IDL re-upload inside the window;
- deploying the native-aware backend, site and SDK *before* `unpause` (the old site's sells go down
  the old swap and fail; its claims fail);
- stopping the old keepers;
- setting `NATIVE_REBALANCE=1` and `CRANK_SECRET_KEY` for the rebalance keeper.

**Contradictions:**

- the runbook says to dry-run `migrate` "against today's chain", but `ops/native.ts:449` refuses
  unless the DEX is paused;
- the programs doc says KTEST "may stay bridged", but `verify` without `--partial` fails on it.

**Fix:** pause, stop the keepers, then snapshot. Add the missing steps and resolve the two
contradictions.

### O-4 (Medium): SDK `launch.swap` silently became `swap_v2` for native launches

**Location:**

- monorepo `packages/sdk/src/instructions.ts:879`;
- `cli/mmRun.ts:182-202`;
- `apps/web/.../SdkPages.tsx:444-451`;
- the SDK `CHANGELOG.md` ("behaviour unchanged").

**Scenario:** callers that wrap SOL and then call `launch.swap` now pay lamports again. The wrapped
SOL is left over: no loss, but a surprise. The SDK's own market-maker CLI does this, and measures
sells in bridged SOL that no longer moves. The documented `router.swap(launch.swap(…))` now throws.

**Fix:**

- make `mmRun` native-aware;
- update the docs example to `router.swapV2`;
- flag the breaking change in the changelog, or make the switch opt-in.

### O-5 (Low): CLI and keeper smaller issues

These come from the off-chain review (monorepo `native-sol`):

- **Half-migrated coins.** "Sell all and claim" on a half-migrated coin (pool native, launch
  bridged) appends `kit.claim` without creating the bridged-SOL holding (`trade/prepare.ts:116-121`).
- **Empty wallets.** Claims don't check for empty author, partner or creator wallets before building
  (`rewards/creator.ts:185`, `marketplace/author.ts:103`). See N-7.
- **No-op rebalances.** The rebalance keeper re-sends no-op transactions when the target is above
  what the pool's lamports can reach (`keeper/rebalance.ts:34-40, 72`). They cost fees only: the
  keeper's own funds are safe, since the 1:1 wrap returns exactly what the pool credits.
- **Unrelated red test.** The server suite has one failure, unrelated to native SOL:
  `studio/knowledge.test.ts` has a stale `corpus.md`.

### Info

- **I-1.** `bordrless_launch::constants::is_companion_creator` (`constants.rs:171`) and
  `LaunchError::CompanionLaunch` (`error.rs:133`) are dead code left from phase 2. Since phase 3,
  companion coins are native from launch.
- **I-2.** `migrate_pool_to_native` returns early on a native pool (`pool.rs:860-862`), so a re-run
  with a new `buffer_target` silently keeps the old one. Use `set_buffer_target`.
- **I-3.** Mainnet's rent (5,080 lamports a byte) is below the legacy 6,960, so existing accounts
  carry excess rent. It surfaces as follows; each amount is tiny (about 0.001–0.002 SOL a coin):
  - a pool's excess counts as quote at graduation (`finalize_curve`, `pool.rs:536-544`);
  - a launch's excess is claimable by its creator (`claimable_sol`);
  - a kit's excess joins `lamports_base`;
  - a companion creator's excess (240,640 lamports) is booked to the beneficiary as spare.

  `verify`'s `≥` hides it, and that is acceptable.
- **I-4.** A rent *increase* would make `check_native` fail and halt native trading until someone
  donates. This is theoretical.
- **I-5.** Approved Workshop scripts that are not native-ready (PumpTek's `HxsFA5…`, `DMQE7D…`) keep
  their approval. A companion launched with one after `set_native_launches(true)` is native from
  launch, and every step fails. The CLI's hold list only covers existing coins.
- **I-6.** Since migration, partners and authors are paid lamports at their key instead of bridged SOL
  in their holding. Notify partners whose key is a program PDA.

## Addendum: held coins and `buffer_all`

Held coins are the 13 mainnet coins whose companion runs a third-party Workshop script that is not
native-ready:

- 8 run PumpTek `HxsFA5…`;
- 5 run `DMQE7D…`.

The scripts trade through the classic `swap` with the creator's bridged SOL, and check that the
pool's quote vault holds its books. Their pools hold about 215 SOL of books in total; the largest
holds 121.5 SOL.

On `native-sol-held`, only their pools go native; the launch, kit, companion and vault stay bridged.
The keeper sends each script step as one transaction: `buffer_all` → step → `rebalance_pool`.

### `buffer_all` (programs `963ad6d`, `programs/bordrless_swap/src/instructions/pool.rs:991-1076`)

It wraps all of a native pool's lamport quote into its quote vault, with the signer's wallet as the
wrap user (as `rebalance_pool` does). It is refused with `NoRebalanceAfter` unless the instructions
sysvar shows a later top-level instruction of this program with all three of:

- the `rebalance_pool` discriminator;
- the pool as its third account;
- this program as its program id.

**Verdict: sound.** No finding on chain. What was checked, each against the code and in tests:

- **Introspection.**
  - The sysvar is pinned by key, so a look-alike account is refused.
  - `rebalance_follows` follows the runtime's layout exactly: a u16 count, u16 offsets, per
    instruction a u16 account count, then 33-byte metas (flags and key), the program id, a u16 data
    length and the data. It reads the current index from the last two bytes.
  - It starts searching *after* the current instruction.
  - It needs this program's id, so a look-alike program with the same discriminator is refused.
  - Malformed data is refused (`None`).
  - Refused cases:
    - `buffer_all` alone;
    - a rebalance only before it;
    - a rebalance of another pool;
    - `[buffer_all A, rebalance A, buffer_all A]`;
    - `[buffer_all A, buffer_all B, rebalance A]`;
    - no sysvar, or another account in its place;
    - a bridged pool.
- **CPI callers.**
  - The sysvar's current index is the *top-level* instruction, so a program that calls `buffer_all`
    by CPI still needs a later top-level `rebalance_pool`.
  - A rebalance it CPIs itself in the same instruction does not count, which is stricter.
  - The window, from `buffer_all` to the rebalance, is the rest of the transaction up to that later
    instruction, whoever calls.
  - Atomicity guarantees the rebalance runs, or nothing does.
- **Inside the window.**
  - Books, reserves and price don't depend on the quote's form. The same old sell and `swap_v2` buy
    give the trader exactly the same result inside a window as outside it, and leave the same books.
  - The old swap's sells are covered by the buffer, so they work.
  - A `swap_v2` sell can't be paid, since the pool holds no lamport quote, and the whole transaction
    fails.
  - LP withdrawals are covered.
  - `finalize_curve` counts lamports plus buffer, so it is unchanged.
  - `collect_protocol_fees_sol` unwraps what it lacks.
  - No sandwich or arbitrage opens: the form never changes the price.
- **Exit state.**
  - The closing rebalance moves the buffer back to the target.
  - If window trades spent the buffer below the target, the rebalance re-wraps only as far as the
    pool's lamports go. The pool is then exact, only in another form.
  - In every case the invariant is exact.
  - The signer's lamports end unchanged: the pool credits it, and the bridge's 1:1 wrap takes the
    same amount back.
- **Two pools.** Each pool needs its own later rebalance.
- **Bridge paused.** The wrap fails, so `buffer_all` fails and held steps stall. The closing unwrap
  would work. This is liveness only.

**Proof:** `ruled_out_buffer_all_needs_a_later_rebalance_of_the_same_pool` and
`ruled_out_trades_inside_the_buffer_all_window_are_value_neutral`, in
`programs/tests/held/audit_buffer_all.rs`, both pass on `native-sol-held`, plus the branch's own unit
test of `rebalance_follows`.

**Upside:** because `buffer_all` is permissionless, anyone can send `buffer_all` → old-path sell or
`remove_liquidity` → `rebalance_pool` in one transaction. That is a self-service way out of N-4 and
N-5, and the site's liquidity builder could use it for native pools.

### H-1 (Medium): new launches with a held script break once native launches are on

**Location:** monorepo `native-sol-held`:

- `apps/server/src/launch/prepare.ts:195-264`;
- `:207` (the `8jYBwc…` pair config may only launch with its pair's script);
- `NATIVE_SCRIPT_HOLD`, which is read only by `ops/native.ts`.

**Scenario:** after `set-native-launches on`, a new coin that runs a held script, or any launch from
the `8jYBwc…` pair config, is native from creation, companion included. Every script step then fails
with `Settlement` (6007). The keeper's `vaultWrapped` returns the bare instruction for a native
companion, so the wrap never applies.

The pot builds up, but the script never runs until an admin reverts and re-migrates the coin. This
is the same root cause as I-5.

**Fix:** move the hold list into shared code and refuse held scripts and pair configs while native
launches are on. Alternatively, make the launchpad or companion keep such a coin's launch and
companion bridged at creation.

### H-2 (Low-Medium): nothing checks that `buffer_all` is deployed before held pools migrate

**Location:** monorepo `apps/server/src/ops/native.ts:129`. `migrate` without `--mint` now always
includes the 13 held pools.

**Scenario:** the DEX is deployed from the `native-sol` build instead of `native-sol-held`, or the
keeper with `vaultWrapped` isn't live before `unpause`. All 13 held scripts then fail every step
until the coins are reverted. No funds are lost.

**Fix:**

- Before a held pool's step, simulate `buffer_all` → `rebalance_pool` on it, or require an explicit
  `--held-pools`.
- Add "DEX with `buffer_all` and keeper deployed" to the runbook's window order.

### H-3 (Low): keeper `vaultWrapped` and the fallback CU limit

**Location:** monorepo `apps/server/src/keeper/scripts.ts:157-162`, `:238`, `:254`.

**Wraps too broadly.** `vaultWrapped` wraps any bridged companion on a native pool, whatever the
launch's form. With the launch native but the companion still bridged (a failed or Squads-pending
companion step), the classic buy's lamport cut runs after `buffer_all`, and the pool has no lamport
quote left. The step fails with `NativeInvariant`/`LamportsShort`. It is atomic, so nothing is lost.

**Reads the pool twice.** The size fit (`:238`) and the real transaction (`:254`) each read the pool,
so a flip between them makes the transaction oversized or unwrapped. It is refused and retried.

**Fallback CU.** The fallback compute limit (`:254`) doesn't count `buffer_all` + `rebalance_pool`.
It applies only when simulation is unavailable; an under-provisioned step can then fail on chain and
still pay the fee.

**Fix:** wrap only when `!launch.isNative`, compute it once, and add the wrap's CU to the fallback, or
don't send unsimulated.

### H-4 (Low): held-coin runbook and `verify` gaps

**Revert.** `revert --mint` on a held coin makes it fully bridged, pool included, and a non-partial
`verify` then fails until `migrate --mint` runs again. The runbook says revert "puts it back".

**Launch holding.** For held coins, `verify` doesn't compare the bridged launch holding with the
snapshot. The launch check runs only on native launches. No false pass or fail was found in the
mixed state.

**SDK constant.** `POOL_QUOTE_FORM_KIND` (`packages/sdk/src/native.ts:264`) lacks `bufferedAll: 4`.

**Bridge accounts.** Held pools need the bridge accounts on every site trade, because their creator
and holder fees go to the bridged launch and kit (wrapped cuts). The O-1 fix must keep `withBridge`
whenever the launch isn't native.

**What checks out:**

- The keeper's transaction is `buffer_all` → step → `rebalance_pool` on the coin's launch pool.
- The instructions sysvar is the first remaining account.
- The SDK's `bufferAll` is byte-identical to the Rust client.
- The keeper wallet is a plain signer and ends where it started.
- Steps are simulated before sending.
- On the 13 real held coins the transaction is 1,125 bytes with the lookup table, inside the 1,232
  limit.
- The CLI still refuses the excluded and foreign-hook pools before the held check, and migrates only
  the pool of a held coin.
- The revert order is unchanged.

## Ruled out (with proof)

| Attack | Result | Proof |
| --- | --- | --- |
| Lamport theft or creation through rounding, buffer/lamports double counting, donations, rebalance or revert round trips | Invariant exact and value conserved over 400 random operations | `ruled_out_seeded_walk_conserves_every_lamport_on_a_native_pool` |
| Hook cut paid to the pool, its quote or base vault, the trader's wallet or delivery account, an executable, or a non-holding token account (bridged-SOL mint), both swaps and both directions | All refused, nothing moves | `ruled_out_native_cuts_to_the_pool_vaults_trader_or_executables`; `native_sol.rs` `swap_v2_refusals` |
| Sell delivering lamports to the pool, a vault, a token account or an executable | Refused | same; `swap_v2_refusals` |
| Arbitrage between `swap` and `swap_v2`, or value from moving the buffer | Identical outputs and books, whatever the buffer and target | `ruled_out_swap_and_swap_v2_price_identically_whatever_the_buffer` |
| Window stopped between steps 1 and 2 and the DEX reopened (pool native, launch bridged) | Trades exactly (the hook names bridged holdings, wrapped) | `ruled_out_pool_native_launch_bridged_still_trades_exactly` |
| Permissionless rebalance draining or paying the caller | The signer is credited only to wrap the same amount into the pool's own vault (address-checked, bridge-checked); a non-system signer fails the wrap and reverts everything; the direction is always toward the admin's target | reasoning (`pool.rs:948-988`, `native.rs:183-211`); `native_sol.rs` `rebalance_moves_the_buffer_toward_its_target` |
| Fake pool, launch, kit or companion in migrate/revert/claims | Address-, seeds-, owner- or discriminator-bound everywhere (`QuoteForm`, `LaunchForm`, `CompanionForm`, `VaultForm`, `ClaimSol`, `ClaimCreatorFeesSol` …). The kit's `set_native` needs the launchpad kit-caller PDA, which only `LaunchForm` and `create_launch` sign for. ProgramData is address- and owner-checked | reasoning |
| CPI balance rule (direct move before a CPI that passes one side only) | Every `move_lamports` before a CPI passes the debited account as `trailing` (`native.rs:124-131`, kit `native.rs:213-224`, launch revert). All other direct moves follow the last CPI except `emit_cpi`, which passes neither side | reasoning; `native_sol.rs` wrapped-cut tests |
| Companion claim split on native (`quote_cut_claimed`, `from_hook`) | Arithmetic unchanged; only the measure (`sol_held`/`fees_waiting`) changes form; set-aside equals creator lamports minus rent(0) after migrate and claims | `companion.rs` `a_companion_migrates_mid_life_and_keeps_every_lamport`; rehearsal `verify` |
| Script custody on lamports | `custody_intact` refuses an assigned or allocated creator; spending is capped by `pending_pot`; rent(0) can't be spent | reasoning (`script.rs:176-229`, `:620-660`) |
| Kit share/claim race, donations | `claim_sol` pays at most `owed.min(vault)` after the CPI; `seen + total_claimed` stays consistent; a donation to a rewards kit is shared, as on bridged kits | reasoning; `native_launch.rs` tests |
| Migration idempotency and exact revert | Every step returns early when already in the target form; revert moves back exactly what the books hold (donations stay uncounted) | walk test (migrate/revert round trips); rehearsal revert/remigrate conservation |
| Old instructions on bridged pools | `native == None` and `quote_native() == false`: same accounts, same checks, same transfers; error codes and events only appended; token and bridge programs untouched | diff review; full suite green |

## Test results

- **Build:** `scripts/solana/programs.sh build`, with the Agave v4.3.0 toolchain, platform-tools
  v1.57 and SBPF v3. All 15 programs build.
- **Suite (native-sol):** `scripts/solana/programs.sh test` (LiteSVM). **851 passed, 0 failed,
  3 ignored** (pre-existing ignores: a CU estimate and two spec notes).
- **Suite (native-sol-audit):** with `audit_native_sol.rs` added, the same suite gives **863 passed,
  0 failed, 3 ignored**.
- **Audit tests:** `audit_native_sol.rs` has 12 tests, all passing: 7 `finding_` and 5 `ruled_out_`.
  `cargo clippy -p bordrless-program-tests --tests -D warnings` is clean.
- **Monorepo:**
  - SDK: 412/412 pass;
  - server: 791 pass and 1 fails (`studio/knowledge.test.ts`, a stale corpus, not native);
  - targeted native/keeper/trade/rewards suites: 174/174;
  - `tsc --noEmit` is clean.
- **native-sol-held:** this suite with `audit_buffer_all` (the audit file plus the two `buffer_all`
  tests): **866 passed, 0 failed, 3 ignored**.
- **Monorepo `native-sol-held`:** SDK 412/412; server 791 pass, 1 skipped, and the same unrelated
  failure; typecheck clean.
- **Rehearsal artifacts:** the conservation summaries hold (`conserved: true`; bridge backing 4,113
  lamports before and after), and so does the revert/remigrate round trip.
