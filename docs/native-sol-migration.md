# Native SOL migration checklist

Moving Bordrless from bridged SOL (`A49oVhX22ExMwTEtFC6Y8nhBdZ4LJDGhdXLDn4c2f59i`) to native SOL
(lamports) across the DEX, the launchpad, the kit, companions, `hook_vault` and the router. Built on
branch `native-sol` (phases 1 to 3). Nothing here changes the token program or the bridge, and no
account address changes: a pool keeps bridged SOL as its `quote_mint` and in its seeds.

## What changes form

| Account | Bridged form | Native form | Flag |
| --- | --- | --- | --- |
| DEX pool | quote vault (bridged SOL) | pool lamports above rent, plus a bridged-SOL **buffer** in the vault | `Pool.reserved[0]`; buffer `[1..9]`, target `[9..17]` |
| Launch | `holding(bridged, launch)` | launch lamports above rent | `Launch.reserved[17]` |
| Kit reward vault | `holding(bridged, kit_config)` | kit config lamports above `lamports_base` | `KitConfig.reserved[0]`, base `[1..9]` |
| Companion | `holding(bridged, creator)` | creator PDA lamports above rent(0) | `Companion.native_sol` (1 byte past `quote_cut_claimed`; the account grows to 309) |
| hook_vault slot | `holding(bridged, slot owner)` | slot owner lamports above rent(0) | `Vault.reserved[0]` |
| Launchpad config | | new launches native | `Config.reserved[0]` (`set_native_launches`) |

Invariant of a native pool, checked on chain after every instruction that moves its quote:
`lamports >= rent + quote_reserve + protocol_fees_quote - buffer` and `quote vault >= buffer`
(equalities unless someone sent SOL from outside, which is never counted or paid out).

## Excluded

- **Pool `4PvPnTByfrnhA1nNmsaou7GrdQsqJNrkinqLHp2JHFX7`** (the P29KQ5 coin, unknown hook program
  `AwAvBeMRJLaprMPCQ85rpQtm5U9JTobjNFP2f4mQCExK`, a curve on the flat fee, about 0.005 SOL): stays on
  bridged SOL and is **never** migrated. Nobody knows what its hook does with SOL-side cuts.
- The hook-less KTEST pool `Bo2q3soUrKb2kH95aFNiMXfUf3UDhVC8JBe1YTKbHGq3` (dust) may stay bridged;
  migrating it is harmless.

Before the window, re-list every pool's `hook_program` (`getProgramAccounts` on the DEX,
`dataSize 411`, Option tag at byte 171, key at 172..204) and confirm every other pool's hook is the
launchpad `1jcBymHxBjniZDhNPy51Vgm5Nz7pLUdxa9UBHc4TavC`.

## Held coins (pool native, the rest bridged)

The coins whose companion runs a Workshop script that checks the pool's quote vault against its books and buys from the creator's
bridged SOL (PumpTek `HxsFA5…`, Turbo PumpTek `DMQE7D…`; the server's `NATIVE_SCRIPT_HOLD`) get step 1 only: the pool goes native,
the launch, kit, companion and hook vault stay bridged. Each script step is sent as one transaction `buffer_all` → step →
`rebalance_pool`: `buffer_all` (anyone, a plain wallet) wraps the pool's lamport quote into its vault, and is refused
(`NoRebalanceAfter`) unless a later top-level `rebalance_pool` of the same pool is in the transaction, which moves it back to the
target. The books, the reserves and the pool's value never change; only the quote's form moves, and back.

## Order

### 0. Upgrade, everything paused

1. Pause the DEX (`set_config`, `paused: true`). Every swap and liquidity change stops. Claims,
   payouts and companion steps that do not swap still run.
2. Upgrade the programs: **DEX first**, then the **kit**, the **launchpad**, the **companion**,
   **hook_vault** and the **router**. Verified builds, then re-upload each IDL through
   program-metadata.
3. Make sure no launchpad-native default is on yet (`Config.reserved[0] == 0`).

### 1. For every launch-pool coin (all pools except the excluded one)

In this order, all while the DEX is paused (each step is idempotent and can be re-sent):

1. `bordrless_swap::migrate_pool_to_native(pool, buffer_target)`: the DEX admin. The pool unwraps its
   books onto its account and keeps `min(buffer_target, books)` in the vault as the buffer.
   A target of 0 is recommended, with a keeper running `rebalance_pool` (below).
2. `bordrless_launch::migrate_launch_to_native(launch)`: the launchpad admin, a plain wallet. It
   needs the pool native. The launch's fee holding is unwrapped onto the launch account. The kit's
   reward vault (when there is a kit) moves to lamports on the kit config through the kit's
   `set_native`, which the launchpad's kit-caller PDA signs. `acc_per_share`, `seen` and every
   holder's bytes stay unchanged.
3. Only for a **companion coin**: `bordrless_companion::migrate_companion_to_native(companion)`,
   signed by the companion program's upgrade authority. It needs the launch native. The account
   grows by one byte (by nine for one made before `quote_cut_claimed`); the admin pays the rent.
   The creator's spare lamports (unspent launch funding) are booked to `pending_beneficiary`, and
   the creator's bridged SOL is unwrapped onto the creator PDA.
4. Only for a coin with a **hook vault**: `hook_vault::migrate_vault_to_native(vault)`, signed by
   hook_vault's upgrade authority. It needs the coin's launch native. Each slot's `pending_sol` is
   unwrapped onto its owner.

The ordinary (non-launch) pools need only step 1.

### 2. Reopen

1. Verify every migrated account: pool invariant, launch lamports above rent equal to the old holding,
   kit `seen` unchanged, companion `set_aside() == creator lamports - rent(0)`, vault slots'
   `pending_sol` equal to the owner lamports above rent(0).
2. `bordrless_launch::set_native_launches(true)`: new launches (companion coins included) are native
   from creation.
3. Unpause the DEX.
4. Start the keeper: `rebalance_pool` on any native pool whose buffer is away from its target (old-path
   buys grow it). Old-path buys on a pool with no lamports yet fail with `LamportsShort` until a
   `swap_v2` trade or a rebalance gives the pool some.

### Rollback (before reopening, or later under a pause)

Reverse order per coin, each idempotent. The programs enforce it: `revert_launch_to_bridged` takes
the coin's hook-vault address (and a companion coin's companion) and refuses while either is native
(`VaultStillNative`, `CompanionStillNative`); `revert_pool_to_bridged` of a launch pool takes the
launch and refuses while it is native (`LaunchStillNative`).

1. `hook_vault::revert_vault_to_bridged`.
2. `bordrless_companion::revert_companion_to_bridged`. The spare booked to the beneficiary stays
   booked; it is the beneficiary's either way.
3. `bordrless_launch::revert_launch_to_bridged` (the admin wallet wraps; it ends where it started),
   which reverts the kit too.
4. `bordrless_swap::revert_pool_to_bridged` (the admin wallet wraps).
5. `set_native_launches(false)`.

## What keeps working unchanged

- The DEX's old `swap`, `add_liquidity`, `remove_liquidity` and `collect_protocol_fees`, on bridged and
  native pools alike, with the same accounts. On a native pool they move bridged SOL through its
  buffer, with these differences:
  - an old-path **sell** or a **`remove_liquidity`** is paid from the buffer as far as it goes, and the
    rest as **lamports to the signer's wallet** (the trader or the provider, which must be writable;
    it is when it pays the fee). So a rebalance that empties the buffer never fails them. Integrators
    that measure only the bridged-SOL holding see part of the proceeds arrive as SOL instead; the
    `Swapped` event's `delivered_out` counts both.
  - an old-path **buy** whose hook names a lamport destination (a native launch's creator and holder
    fees) pays it from the pool's lamports. On a fresh native launch pool that has no lamport quote
    yet, it fails with `LamportsShort` until a `swap_v2` trade gives the pool some.
  - `collect_protocol_fees` (bridged SOL) still needs the buffer to cover it (`BufferShort`); use
    `collect_protocol_fees_sol`.

  Integrators should move to `swap_v2` before the window.
- The launchpad's old claims (they pay whatever sits in the old holding), and the kit's `claim` and
  `share` on bridged kits.
- The router's old `swap`.
- Every companion step's old client builder: a native companion's steps look their accounts up by
  key; `bordrless_companion::client::native` appends the extra accounts its native paths call.

## New entry points (for the SDK)

- DEX: `swap_v2`, `create_native_pool`, `migrate_pool_to_native(buffer_target, allow_foreign_hook)`
  (a pool whose hook is neither absent nor the launchpad needs `allow_foreign_hook`; pool
  `4PvPnTBy…` is refused whatever; a re-run on a native pool applies a new target),
  `revert_pool_to_bridged` (a launch pool passes its launch, client `revert_launch_pool_to_bridged`),
  `rebalance_pool`, `set_buffer_target`, `buffer_all`; events `SwappedNative`, `PoolQuoteFormChanged`.
- Launchpad: `claim_creator_fees_sol`, `claim_author_fees_sol`, `distribute_fees_sol`,
  `set_native_launches`, `migrate_launch_to_native`, `revert_launch_to_bridged`; client `swap_v2`.
- Kit: `claim_sol`, `share_sol`, `set_native` (launchpad only).
- Companion: `migrate_companion_to_native`, `revert_companion_to_bridged`; wallet payouts may name a
  holding of bridged SOL (its owner passed, paid in bridged SOL). A script companion launches with
  `client::launch_script` (its script's address appended): while native launches are on, a script on
  `NOT_NATIVE_READY_SCRIPTS` is refused (`ScriptNotNativeReady`). An old 300-byte companion grows for
  `quote_cut_claimed` at a native launch.
- hook_vault: a vault opened (`open_vault`) for a coin launched native is native from the start.
- Launchpad: `create_partner_config` refuses a reserved key (sysvar, builtin program) as the partner.
- hook_vault: `migrate_vault_to_native`, `revert_vault_to_bridged`.
- Router: `swap_v2` (fees in SOL to the integrator's wallet and the DEX treasury).
