# Hook protocol v2 and programmable launches

Status: specification, revised 2026-10-07 after a security, a runtime, an economics and a product
review, a fact check of Token-2022 transfer hooks against the version on mainnet (v11.0.0), and a
second check of the revision (coverage, attack, money simulation). Nothing in this document is
deployed. It extends `docs/architecture.md`; where they differ, this document wins. Section 0 lists
what the reviews changed and why.

## Why

### What a Token-2022 transfer hook is

Checked against Token-2022 v11.0.0 (the mainnet build) and Solana's own documentation. These are
the only statements about transfer hooks the site and the docs may make.

1. It runs only on transfers. Minting, burning and withdrawing withheld fees never call it, and a
   transfer to the same account skips it.
2. It runs after the balances have changed. It gets the transfer's accounts read-only, and no
   account reaches it as a signer, so it can only sign for its own PDAs.
3. It cannot change the amount, take part of it, burn it or send it elsewhere. Over the transfer
   itself it can only let it stand or make the whole transaction fail. It can write to its own
   accounts and call other programs, for example to charge a separate fee in another token from an
   account the sender approved in advance.
4. It is not told why a transfer happened: no buy or sell flag, no price, no counterpart asset. It
   can guess, by comparing addresses with pool vaults it was configured with, or by reading the
   top-level instructions. A venue it does not know about looks like a gift.
5. Remembering anything per holder takes a separate account per holder, owned by the hook, which
   someone has to create and fund before it is used, and which every transfer has to carry.
6. The only fee Token-2022 takes from the tokens moved is the separate transfer-fee extension: one
   rate per mint, charged on every transfer alike (buys, sells and gifts), withheld in the
   recipient's account and collected later.
7. Venues treat hooks as a risk. Meteora's DBC revokes a token's hook when it graduates, and
   DAMM v2 accepts a hook mint permissionlessly only once its hook is revoked. Orca lists hook mints
   case by case, and its criteria forbid hooks that charge fees or move tokens. Raydium's CPMM and
   CLMM refuse hook mints unless an admin registers them.

Sources: solana.com/docs/tokens/extensions/transfer-hook,
solana.com/docs/tokens/extensions/transfer-hook-integration,
solana.com/docs/tokens/extensions/transfer-fees, github.com/solana-program/token-2022
(program@v11.0.0, `program/src/processor.rs`), docs.orca.so/developers/architecture/token-extensions,
docs.raydium.io/reference/token-2022-support,
docs.meteora.ag/core-products/damm-v2/token-2022-support,
docs.meteora.ag/core-products/dbc/transfer-hook-pools.

### What a Bordrless hook is

It keeps the safety rule that matters: it never gets the user's signature. What it wants done to an
operation, it answers, and the token program or the DEX applies the answer.

- Token hooks run on transfers, mints and burns. Pool hooks run on pool creation, liquidity and
  swaps.
- A pool hook sees the whole trade: direction, input, output, reserves, both mints, the trader and
  the wallet that receives.
- A `before_*` callback can take up to three cuts from the amount, each to an account it names. A
  pool hook can also burn part of it and set the swap's LP fee.
- A token hook keeps 64 bytes of state for each holder inside the holding itself. The token program
  writes it from the hook's answer. No extra account per holder.
- The token program never applies more than the amount being moved, and the DEX checks every
  account a hook names.

### What it buys a launch

| Rule | Status | Runs in | Could a Token-2022 transfer hook do it? |
| --- | --- | --- | --- |
| Sniper fee: up to 80% LP fee in the first 30 s, kept by the pool | live (v1) | pool hook | No: it cannot set a pool's fee |
| Creator fee in SOL from each trade on the launch pool | live (v1) | pool hook | Only as a separate charge from an account the trader approved in advance, and by guessing which transfers are trades |
| Trades pay, wallet-to-wallet transfers don't | live (v1) | pool hook | Only by guessing which transfers are trades, and only as a separate pre-approved charge; the transfer-fee extension charges every transfer alike |
| Holder rewards: a share of each trade paid to holders in SOL, claim any time | v2 | pool hook (fee) + kit (accounting) | Only as a separate pre-approved charge, by guessing which transfers are trades, with an extra account per holder; it can take no part of the trade itself |
| Share with holders: anyone sends SOL that every holder receives pro rata | v2 | kit | Only with an extra account per holder, created and funded before use and carried by every transfer |
| Burn: a share of each buy and sell on the launch pool is destroyed | v2 | pool hook | No: it cannot change an amount or burn |
| Different rates for buys and sells | v2 | pool hook | Only by guessing which transfers are buys and sells, and only for a separate charge; it cannot take or burn part of the tokens |
| Early-buyer lock: tokens bought in the first seconds can't move until a set time | v2 | kit, state in each holding | Only with an extra account per holder, and by guessing which transfers are buys |
| Max wallet: no wallet above N% of supply until graduation | v2 | kit | Yes: it can refuse a transfer |
| Creator wallet lock: the creator's wallet can't sell or send until a set time | v2 | kit | Yes: it can refuse a transfer |

Where a transfer hook can do one of these at all, it needs an account the trader approved in
advance, an extra account per holder, or a guess about which transfers are trades. Here each is
part of the trade. The last two rows are safety rails: a transfer hook could enforce them too; here
they are standard, bounded on chain and fixed at launch.

## 0. What the reviews changed

1. Every account a kit entry point reads is bound on chain (§4.6). Without that, anyone could
   inflate rewards and drain a vault, or freeze a token.
2. The kit is called only through a per-mint PDA of the launch program, `["kit-caller", mint]`,
   which has no other power. The launch program hard-codes the kit's id. The launch's
   `["hook-authority"]` and the Launch PDA are never passed to the kit.
3. The reward math keeps an exact scaled remainder instead of a lamport carry, which paid rounding
   twice. Claims may be partial.
4. Eligible supply is a counter in `KitConfig`. The pool vault and the launch reserve are no longer
   callback accounts.
5. With holder rewards on, no program-owned account can hold the token (§4.5), so every pool trade
   happens on the launch pool. That closes a second pool that skips the holder fee and rewards
   stranded with owners that cannot claim.
6. A token callback never answers a burn. Burning stays with pool hooks, where the DEX controls
   whether the mint is writable.
7. A launch with kit rules gets its mint created with the kit as hook and no hook authority. A
   launch without them gets no hook and no hook authority.
8. Hook data is 64 bytes.
9. Max wallet always lifts at graduation, and its cap is fixed at launch.
10. The behavior is "holder rewards", never "yield", APR or APY.
11. The pool hook is told the swap's recipient. The creator's first-buy exemption needs
    recipient == creator, and it is a one-time flag rather than "the first swap".
12. A ceiling on creator + holder + burn per side, and separate buy and sell rates.
13. `Swapped` carries every delta with its destination. The launch keeps holder-fee and burn
    counters.
14. The DEX takes its protocol fee in the pool's quote token on both sides of every swap (§3.1), so
    no protocol wallet ever holds a launched token. The kit's excluded owners are the pool and the
    launch only; the earlier draft's fee-collector exclusion let that wallet claim holders' rewards,
    skip the locks and break when the DEX admin changed it.
15. Registry creation tolerates an address someone has already funded, and the site never re-signs
    a broadcast mint key.
16. The off-chain mirror of rewards is a pure function of account state.
17. New: share with holders (streamed over an hour, so it cannot be sandwiched), the early-buyer
    lock, separate buy and sell rates.
18. A stated upgrade policy. The site does not say "immutable" while a program can be upgraded.
19. `claim` refuses excluded owners and never touches the early-buyer bytes; a supply below 1,000
    base units is refused; the kit's registry always has two extras so Anchor's optional accounts
    work; graduation of a kit launch always calls the kit.

## 1. Protocol v2 (`crates/bordrless-hook`)

### 1.1 Answers

```rust
pub const MAX_DELTAS: usize = 3;
pub const HOOK_DATA_LEN: usize = 64;

pub struct Delta {
    pub amount: u64,
    /// Index in the callback's account list (prefix first, then extras). Must be an extra.
    pub account: u8,
}

pub struct HookReturn {
    pub deltas: Vec<Delta>,                              // at most MAX_DELTAS
    pub burn: u64,                                       // pool swap callbacks only
    pub lp_fee_bps: Option<u16>,                         // before_swap only
    pub source_hook_data: Option<[u8; HOOK_DATA_LEN]>,   // token callbacks only
    pub destination_hook_data: Option<[u8; HOOK_DATA_LEN]>,
}
```

v1's `delta` and `delta_account` are gone. The struct is not wire-compatible with v1, so
`tax_hook` is ported. Return data stays under about 200 bytes (the limit is 1,024).

The caller enforces, after Borsh decoding:

- It reads the answer only when the mint's or the pool's flags allow one for that callback (§1.4),
  and only when the return data is the hook program's own (v1 rule).
- `deltas.len() <= MAX_DELTAS`; every amount is above zero; no account appears twice; sums are
  checked, and an overflow is `DeltaTooLarge`.
- A field the callback or the flags do not allow must be empty, zero or `None`, else
  `UnsupportedHookReturn`:
  - deltas: token `before_transfer` with `TRANSFER_RETURNS_DELTA`; pool `before_swap` and
    `after_swap` with their `RETURNS_DELTA` flag;
  - burn: pool `before_swap` and `after_swap` with their `RETURNS_DELTA` flag;
  - `lp_fee_bps`: `before_swap` with `BEFORE_SWAP_OVERRIDES_FEE`;
  - hook data: token callbacks of a mint with `WRITES_HOOK_DATA`; no source data on `before_mint`,
    no destination data on `before_burn`.

### 1.2 Token callback arguments

`TokenHookArgs` gains `source_hook_data: [u8; 64]` and `destination_hook_data: [u8; 64]` (the
holdings' current data; zeros for the side a mint or burn lacks). `delta` becomes the sum of the
deltas applied (known in the `After` phase only).

### 1.3 Pool callback arguments

`PoolHookArgs` gains `recipient: Pubkey`: for a swap, the owner of the holding the output is
delivered to; for liquidity, the owner of the holding that receives (the LP holding on add, the
base holding on remove); the default key on initialize.

### 1.4 Flags and shared constants

Token: new `WRITES_HOOK_DATA = 1 << 7`; `token_flags::ALL` becomes 255. `TRANSFER_RETURNS_DELTA`
allows deltas only. Pool flags keep their bits; `BEFORE_SWAP_RETURNS_DELTA` and
`AFTER_SWAP_RETURNS_DELTA` now also allow `burn`. In `packages/shared/src/programs.ts`,
`TOKEN_HOOK_FLAGS` gains the new bit and `PROGRAM_IDS` gains
`kit: 14RJQXPdJfkehit6ezktjd3xujamf8nVSKw2shKamaEH`.

### 1.5 Registry creation and pre-funded addresses

`write_registry` accepts an address that already holds lamports: it tops the account up to rent,
allocates and assigns it, as Anchor's `init` does. (`create_holding` already uses
`init_if_needed`, and `KitConfig` is created with Anchor `init`.) Funding an address costs at least
the rent-exempt minimum, and each funded address costs two more instruction-trace entries, so an
attacker who funds many of a launch's 13 new addresses could still push it over the 64-entry limit.
What protects a launch is that the site makes a fresh mint keypair for every attempt and never
re-signs a launch with a mint key that has been broadcast.

### 1.6 Delta recipients

A token-level delta credits a holding without calling the hook for that holding. A hook that keeps
per-holder state must only name holdings it leaves out of that state. The docs page says so for
third-party hooks.

## 2. Token standard (`programs/bordrless_token`)

### 2.1 Hook data in every holding

`Holding` gains `hook_data: [u8; 64]` after `frozen`. The layout changes (nothing is deployed), so
every reader is rebuilt against it: the DEX, the launch, the bridge, the kit, the SDK coders and the
backend.

### 2.2 Writing hook data

Only the mint's hook program can change it, in two ways:

1. By answering `source_hook_data` / `destination_hook_data` from `before_transfer`, `before_mint`
   (destination only) or `before_burn` (source only), when the mint has `WRITES_HOOK_DATA`. The
   token program writes it with the balances, after applying the operation.
2. By the new instruction `write_hook_data(data: [u8; 64])`. Accounts: `hook_signer` (signer),
   `mint` (`Account<Mint>`), `holding` (mut, `Account<Holding>`, `has_one = mint`). Checks:
   `mint.hook_program == Some(P)`, `mint.hook_flags & WRITES_HOOK_DATA != 0`, and
   `hook_signer == find_program_address(["hook-authority"], P).0` (canonical bump). It calls no
   hook. Event `HookDataWritten { mint, holding, owner, data }`. The kit's `claim` uses it.

### 2.3 Closing a holding

`close_holding` gains the `mint` account, with `has_one = mint` on the holding. It refuses a
holding whose data is not all zero when the mint has a hook with `WRITES_HOOK_DATA`
(`HookDataNotEmpty`): a hook's record of what a holder is owed must not vanish with the account.
Without a hook, or without that flag, nobody can ever clear the data, so the holding closes
regardless.

### 2.4 Transfers

`transfer(amount)`, when `before_transfer` answers:

- Deltas (with `TRANSFER_RETURNS_DELTA`): `sum(deltas) <= amount`, else `DeltaTooLarge`. Each delta
  account is a writable holding of the mint, not frozen, neither the source nor the destination,
  and appears once (`InvalidDeltaAccount`).
- A burn: `UnsupportedHookReturn`.
- Hook data (with `WRITES_HOOK_DATA`): written for each side answered.
- The source loses `amount`; the destination gains `amount - sum(deltas)`; each delta holding gains
  its amount.
- `mint` stays read-only in `transfer`, so no transfer write-locks a mint.
- Event `Transferred`: `amount`, `deltas: Vec<DeltaApplied { holding, owner, amount, post }>`,
  `source_post`, `destination_post` and the rest as in v1. Hook data is not in events (§4.13).

`mint_to` accepts destination hook data from `before_mint`, and `burn` accepts source hook data
from `before_burn`; nothing else.

## 3. DEX (`programs/bordrless_swap`)

### 3.1 Swaps

The protocol fee is always taken in the pool's quote token. On a buy (quote in) it comes from what
reaches the vault, as in v1; on a sell (base in) it comes from the curve's output. On an ordinary
pool the LP fee stays on the input side in both directions and compounds into the reserves; on a
launch pool it is Bordrless's, in SOL (below). `Pool.protocol_fees_base` no longer exists.

Two protocol fee models (`Pool.fee_model`, fixed at creation; 2026-10-07, the product owner's
decision: "we earn 25% of their total fees, not 0.25%"). The DEX config holds `protocol_fee_bps`
(1%, `policy::PROTOCOL_FEE_BPS`) for ordinary pools, the ones anyone creates (`FEE_MODEL_FLAT`: a
flat rate of the quote), and `launch_protocol_share_bps` (2,500 = a quarter,
`policy::LAUNCH_PROTOCOL_SHARE_BPS`) for launch pools: a pool created as a curve by a hook caller
(`virtual_base > 0 || virtual_quote > 0` with the hook's authority signing), which is how the
launchpad opens every launch (`FEE_MODEL_SHARE`). Under the share model Bordrless takes no flat
rate: it takes a share of what the launch's rules collect on each swap, the hooks' cuts, in the
quote (SOL). The cuts are what the hooks took that someone receives, measured by the DEX:
`cuts_in = amount_in - burn_in - received` on the input side (the pool hook's deltas, plus any cut
the input mint's own token hook took from those transfers) and `cuts_out = amount_out - burn_out -
held - delivered` on the output side (`held` is the share a sell held back, below). Base-side cuts
are valued at the swap's own price (`quote_value`: `ceil(cut * quote_leg / base_leg)` over the
quote and base the curve exchanged). Burns and the LP fee are not fees anyone collects, so they
are not shared. **A launch whose rules collect nothing pays Bordrless nothing: there is no floor.**

**The LP fee of a launch pool is Bordrless's** (upgrade of 2026-10-08, the product owner's
decision: the 0.3% "is meant to be taken by protocol", paid to the treasury in SOL). A launch pool's
liquidity is locked for ever (the first LP is minted to the launch, which cannot spend it), so a fee
compounding into it paid nobody. Under the share model (`swap_amounts_shared` in `bordrless-core`)
the LP fee, at the rate the pool hook sets (0.3%, or the sniper fee in the first 30 seconds), is
taken in the quote (SOL) and added to the protocol fee: on a buy from what reached the vault,
before the curve; on a sell from the curve's output, before `after_swap` is told the rest. Nothing
compounds: a buy's reserve gains `received - lp_fee - share`, a sell's input reserve gains all of
`received`. `Swapped` reports `lp_fee: 0` and `protocol_fee` = the LP fee plus the share;
`lp_fee_bps` is still the rate charged. Ordinary pools (`FEE_MODEL_FLAT`) are unchanged.

The fees accrue as bridged SOL in the pool's quote vault (`protocol_fees_quote`). **Anyone** may
send `collect_protocol_fees_sol` for a pool quoted in bridged SOL (`NotBridgedSol` otherwise): the
pool signs the bridge's `unwrap_sol` of `protocol_fees_quote`, the lamports land in the pool
account, and the DEX moves them to the config's `fee_collector` (`address`-checked, `WrongHolding`
otherwise), a wallet. The reserves, the pool's own lamports and every other balance are unchanged;
`ProtocolFeesCollected` is emitted. The admin-only `collect_protocol_fees` (into the collector's
holding of the quote) still works for every pool. The backend's worker sends the SOL collection
every 10 minutes for pools holding at least 0.01 SOL of fees (`apps/server/src/keeper/fees.ts`).
`create_pool` writes `fee_model`, `protocol_fee_bps` (0 under the share model) and
`protocol_share_bps` (0 under the flat model) into the pool, which keeps them for life, so a launch
pool shares its cuts on its curve and after graduation. Quotes, the indexer and the site read the
pool's own fields. The flat rate is bounded by `MAX_PROTOCOL_FEE_BPS` (1,000), the share by
`MAX_PROTOCOL_SHARE_BPS` (10,000); `ConfigArgs` and `ConfigSet` carry both; the share took two of
the config's reserved bytes (`reserved` is 62), the pool's two new fields three of its (`reserved`
is 60).

Rounding of the share: each side's share is `protocol_share(value, share_bps) =
ceil(value * share_bps / 10_000)`, so it rounds up and is never more than the cuts' value (a share
is at most 10,000). `protocol_fee` in `Swapped` is the two sides' shares added.

A buy (quote in, base out), in this order:

1. `before_swap` answer, taken from the input: each delta is a token transfer from the trader's
   input holding to the named holding (the trader signed the swap; the input mint's token hook
   runs with the input hook accounts); then the burn, a token burn from the trader's input holding
   (the input mint must have been passed writable, else `MintNotWritable`).
2. The rest (`received`) reaches the quote vault; `cuts_in` is measured.
   `lp_fee = fee_amount(received, lp_fee_bps)`. Flat model: `protocol_fee = fee_amount(received,
   protocol_fee_bps)`. Share model: `protocol_in = lp_fee + protocol_share(cuts_in, share_bps)` (the
   LP fee is Bordrless's). The curve runs on `received - lp_fee - protocol_share(cuts_in)`; the
   protocol fee is kept apart from the reserves (the trader pays it). Only the flat model's LP fee
   enters the reserve.
3. `after_swap` answer, taken from the curve's output: each delta from the output vault to the
   named holding (the pool signs); then the burn from the output vault (the pool signs; the output
   mint writable, else `MintNotWritable`).
4. The rest goes to the recipient's holding; `cuts_out` is measured.
5. Share model: `protocol_out = protocol_share(quote_value(cuts_out, net_in, out_gross),
   share_bps)` is set aside from the quote reserve, the only quote the pool holds once the swap has
   run (the output is base; on a kit launch `cuts_out` is always 0, since the kit takes no deltas
   and the launch hook only burns there; it is above 0 only with a creator's own hook, §5.8).

A sell (base in, quote out), in this order:

1. `before_swap` answer, taken from the input, as above (for launch pools: the burn).
2. The rest (`received`) reaches the base vault; `cuts_in` is measured.
   Flat model: `lp_fee = fee_amount(received, lp_fee_bps)`; the curve runs on `received - lp_fee`.
   Share model: the curve runs on all of `received`. It gives `out_gross` in quote.
3. Flat model: `protocol_fee = fee_amount(out_gross, protocol_fee_bps)`. Share model:
   `protocol_in = fee_amount(out_gross, lp_fee_bps) + protocol_share(quote_value(cuts_in,
   out_gross, net_in), share_bps)` (the LP fee, Bordrless's, plus a share that is 0 on a kit
   launch). It stays in the quote vault, apart from the reserves. `after_swap` is told
   `amount_out = out_gross - protocol_in`.
4. `after_swap` answer, taken from that output, as above (for launch pools: the creator and holder
   fees).
5. Share model: `held = protocol_share(sum(deltas_out), share_bps)` is held back from the delivery
   (the trader pays it). The rest goes to the recipient's holding; `cuts_out` is measured, and any
   share of it beyond `held` (a quote-side token hook's own cut on the delivery; bridged SOL has
   none) is set aside from the quote reserve. `protocol_fee = protocol_in + protocol_out`.

Rules:

- `sum(deltas) + burn < amount` on each side, in checked arithmetic, else `DeltaTooLarge`; the
  delivery after the hook's cut and the held-back share must be above zero, else
  `FeeExceedsOutput`.
- Delta accounts: writable holdings of that side's mint, none of the two vaults or the trader's two
  holdings, each once.
- Reserves: `received` (less the buy's protocol fee) enters the input reserve; `out_gross` leaves
  the output reserve; everything that goes to deltas, burn, the held-back share and the sell's
  input-side share is part of the output; a share set aside from the quote reserve moves from the
  reserve to `protocol_fees_quote`. After every swap the quote vault holds `quote_reserve +
  protocol_fees_quote` and the base vault `base_reserve`.
- `min_amount_out` is checked against what the recipient's holding actually gained.
- `PoolHookArgs.recipient` is the owner of `trader_out`; `PoolHookArgs.protocol_fee_bps` is the
  pool's flat rate (0 under the share model).
- `TokenAccounts` gains a burn helper that takes the hook program, its extra accounts and signer
  seeds (the pool signs a vault burn).
- Clients pass a mint writable only when the pool's hook may burn that side. For launch pools the
  SDK marks the base mint writable when the launch has any burn rate; bridged SOL is never writable
  in a swap.

Event `Swapped` gains `deltas_in: Vec<DeltaPaid { holding, amount }>`, `deltas_out`, `burn_in`,
`burn_out`, `cuts_in`, `cuts_out` and `recipient`; v1's `delta_in` and `delta_out` are removed.
`protocol_fee` is always in the quote token. `PoolCreated` gains `fee_model` and
`protocol_share_bps`.

The reference math is `bordrless_core::swap_amounts` (flat), `swap_amounts_shared` and
`output_share` (share), `quote_value` and `protocol_share`; the TypeScript mirror is pinned to them
through `programs/tests/vectors/launch-fees.json`.

Liquidity callbacks still answer nothing.

### 3.2 Protocol fees

`collect_protocol_fees(quote_hook_accounts: u8)`: moves `protocol_fees_quote` to the DEX config's
fee collector, passing the quote mint's token-hook slice (hook program first) as `swap` does. No
launched token ever passes through it, so the fee collector is never a holder of one, and the DEX
admin may change the fee collector freely.

## 4. The kit (`programs/bordrless_kit`, new)

Program id `14RJQXPdJfkehit6ezktjd3xujamf8nVSKw2shKamaEH` (mainnet, devnet and localnet), keypair
kept off this machine's repo checkout (see `keys/README.md`).

### 4.1 Trust

- The kit is a token hook with four fixed, bounded modules. Only the launchpad installs it:
  `KitConfig` is created only by `init`, which must be signed by
  `PDA(["kit-caller", mint], LAUNCH_ID)`. The kit knows `LAUNCH_ID` as a constant (no crate
  dependency). That PDA signs nothing else and holds nothing.
- The launch program depends on the kit crate (no-entrypoint feature) for `KIT_ID`, `KitConfig` and
  the instruction builders. There is no config field for the kit's address.
- A kit launch's mint is created with `hook_program = KIT_ID` and `hook_authority = None`, so which
  program runs is fixed from the token's first instruction. The kit's own code can change while it
  has an upgrade authority (§4.14).
- Anyone can create a mint that names the kit as its hook. Without a `KitConfig`, which only a
  launch can create, every callback for that mint fails, so such a mint can never move.

### 4.2 Modules and flags

| Bit | Module | Parameters |
| --- | --- | --- |
| 1 | Holder rewards | the reward vault and the accounting below; the fee itself is set in the pool hook |
| 2 | Max wallet | `max_wallet_amount`, fixed at `init`; lifts at graduation |
| 4 | Creator wallet lock | `creator_unlock_at` |
| 8 | Early-buyer lock | `early_window_end`, `early_unlock_at` |

Mint flags: `BEFORE_TRANSFER`, plus `BEFORE_BURN | WRITES_HOOK_DATA` when module 1 or 8 is on. A
launch with none of these modules gets no hook at all; burn alone runs in the pool hook.

### 4.3 `KitConfig` at `["kit", mint]`

| Field | Type | What |
| --- | --- | --- |
| `version`, `bump`, `kit_caller_bump` | u8 | `kit_caller_bump` is the bump of `PDA(["kit-caller", mint], LAUNCH_ID)` |
| `modules` | u8 | bit set of §4.2 |
| `graduated` | bool | set by `graduate` |
| `eligible` | u64 | sum of the balances of every holding whose owner is not excluded |
| `min_eligible` | u64 | `supply / 1_000` at `init` (at least 1, since supply is at least 1,000) |
| `mint`, `launch`, `pool`, `creator` | Pubkey | from `init` |
| `reward_mint` | Pubkey | the launch's quote mint (bridged SOL, which has no hook) |
| `reward_vault` | Pubkey | `holding(reward_mint, kit_config)`; default when module 1 is off |
| `supply_at_init` | u64 | |
| `max_wallet_bps` | u16 | 0 when off |
| `max_wallet_amount` | u64 | `supply_at_init * max_wallet_bps / 10_000` in u128; 0 when off |
| `creator_unlock_at` | i64 | 0 when off |
| `early_window_end`, `early_unlock_at` | i64 | 0 when off |
| `acc_per_share` | u128 | rewards per eligible base unit, scaled by `SCALE = 1e12` |
| `rem` | u128 | scaled remainder of the last division, below the eligible supply it was divided by |
| `held` | u64 | lamports waiting while `eligible < min_eligible` |
| `seen` | u64 | `reward_vault.amount + total_claimed` at the last sync, counting streamed shares as seen |
| `stream_remaining` | u64 | shared lamports not yet released to holders |
| `stream_last`, `stream_end` | i64 | the stream's last release and its end |
| `total_distributed`, `total_claimed`, `total_shared` | u64 | lamports |
| `created_at` | i64 | |
| `reserved` | [u8; 64] | |

The launch's pool hook deserializes `KitConfig` through the kit crate to read `eligible` and
`min_eligible`.

### 4.4 Registry and hook data

Registry `["bordrless-hook-accounts", mint]` under the kit. It always lists two extras after the 5
prefix accounts: `kit_config` (writable), then `reward_vault` (readonly) when module 1 is on, or
the kit's own program id when it is off (Anchor's `None` for an optional account).

Hook data (64 bytes per holding, little-endian):

| Bytes | Field | Module |
| --- | --- | --- |
| 0..16 | `snapshot: u128`, the `acc_per_share` at the last settle | 1 |
| 16..24 | `owed: u64`, rewards earned and not claimed | 1 |
| 24..32 | `early_locked: u64`, tokens bought in the early window | 8 |
| 32..64 | reserved, zero | |

All zero for a holding the kit has never written, and written back as all zero whenever nothing is
left to keep, so the holding can close.

### 4.5 Owners

- Excluded owners: `pool` and `launch`. Their holdings are not settled, not capped and not counted
  in `eligible`, and they cannot claim. Every other owner is a holder.
- Refused destinations (`DestinationNotAllowed`): `launch` (after `init` the reserve only ever
  sends), `kit_config`, `Pubkey::default()` (the system program's address), and the ids of the
  token, DEX, bridge, launch and kit programs.
- Holder rewards on: a transfer to an owner that is not excluded must go to an address on the
  ed25519 curve, else `DestinationNotAllowed`. Checked with the curve25519 validate-point syscall
  (`Pubkey::is_on_curve` compiles to it; 159 CU). So no program-owned account can hold the token:
  no second pool, vault, escrow or multisig, and every pool trade happens on the launch pool and
  pays its rules. Trades that never hold the token in a program (a two-signer swap between wallets,
  an order book that moves tokens wallet to wallet as a delegate) are not prevented and pay no
  launch fees; max wallet and the locks still apply to them.
- Holder rewards off: anyone can open another pool for the token. Burn, the creator fee and the buy
  and sell rates apply only on the launch pool, and max wallet caps another pool's vault like any
  wallet until graduation.

`eligible` changes in every callback: holder to holder, unchanged; excluded to holder,
`+= amount`; holder to excluded, `-= amount`; a burn from a holder, `-= amount`; anything between
or from excluded holdings, unchanged. The kit answers no deltas, so what leaves is what arrives.
At `init` the whole supply sits in the launch reserve, so `eligible` starts at 0.

### 4.6 Account binding

Callbacks (`before_transfer`, `before_burn`):

- The prefix accounts are `UncheckedAccount`s: for a burn the token program puts the mint in the
  destination slot. Balances, owners and hook data come from `TokenHookArgs`.
- `hook_signer` is a signer and equals the token program's hook authority, a constant
  (`BDDa1JTNkJSLAnwmUyqh43Pn4nsscKxmsYwUXdnN7gfb`).
- `kit_config` is an `Account<KitConfig>` (owner and discriminator checked) with
  `kit_config.mint == args.mint`, and the prefix mint is `args.mint`. Only `init` creates a
  `KitConfig`, and only at `["kit", mint]`, so this binds it without a derivation.
- `reward_vault` is an optional account: present (with `address = kit_config.reward_vault`) exactly
  when module 1 is on, else `MissingRewardVault`.
- `args.op == Mint`, or the `After` phase, is refused.

`claim` and `share` are bound as listed in §4.10. `write_hook_data` is bound in the token
program (§2.2).

### 4.7 Sync

At the start of every callback when module 1 is on, and in `claim` and `share`:

```
// the shared stream releases linearly until stream_end
if stream_remaining > 0:
    released = if now >= stream_end { stream_remaining }
               else { stream_remaining * (now - stream_last) / (stream_end - stream_last) }   // u128, floor
    stream_remaining -= released;  stream_last = now
else: released = 0
total = reward_vault.amount + total_claimed
fresh = total - seen + released           // checked; a shortfall is a bug
seen  = total
if eligible > 0 and eligible >= min_eligible:
    pot = fresh + held;  held = 0
    if pot > 0:
        scaled = pot * SCALE + rem        // u128
        inc    = scaled / eligible
        rem    = scaled - inc * eligible
        acc_per_share += inc
        total_distributed += pot
else:
    held += fresh
```

`eligible` is the value before this operation. Anyone may send bridged SOL to the reward vault
directly; the next sync distributes it at once (the site always uses `share`, which streams).

### 4.8 Settle (one holder, at its balance before the operation)

Only holders are settled; excluded owners never are.

```
owed += balance * (acc_per_share - snapshot) / SCALE      // floor, u128
snapshot = acc_per_share
```

When the holder's balance after the operation is 0, `snapshot` is written as 0 (it no longer
matters), so the data is all zero once `owed` and `early_locked` are zero.

### 4.9 Callbacks

`before_transfer`, in this order:

1. Refused destination, or the holder-rewards curve check (§4.5): `DestinationNotAllowed`.
2. Sync (module 1).
3. Creator wallet lock (module 4): `now < creator_unlock_at` and `source_owner == creator`, whoever
   signs (owner or delegate): `CreatorLocked`.
4. Early-buyer lock (module 8), source a holder: `now < early_unlock_at` and
   `source_balance - amount < early_locked(source)`: `EarlyLocked`.
5. Settle the source (module 1, a holder) at `source_balance`.
6. Settle the destination (module 1, a holder) at `destination_balance`.
7. Max wallet (module 2, not `graduated`, destination a holder):
   `destination_balance + amount > max_wallet_amount`: `MaxWalletExceeded`.
8. Early-buyer bookkeeping (module 8): when `source_owner == pool`, `now < early_window_end` and the
   destination is a holder, the destination's `early_locked += amount`. Once
   `now >= early_unlock_at`, `early_locked` is written as 0 on each side it touches.
9. `eligible` (§4.5).
10. Answer the hook data of each side that changed. No deltas, no burn.

`before_burn`: sync; the early-buyer check on the source; settle the source when it is a holder;
`eligible -= amount` when the source is a holder; answer the source's data. The creator lock does
not apply to burns: a burn extracts nothing.

What follows: sells go to the launch pool's vault, which is excluded, so max wallet never blocks a
sell into the launch pool. The cap is fixed at `init`, so burns never tighten it. A sell with burn
on burns first and transfers second, so a locked wallet's sell still reverts on its transfer.

### 4.10 Instructions

`init(args: KitInitArgs)`, called by the launch program inside `create_launch`. Accounts:

- `kit_caller` (signer): equals
  `create_program_address(["kit-caller", mint, [args.kit_caller_bump]], LAUNCH_ID)`.
- `payer` (signer, mut): the creator.
- `mint`: a token-program `Mint` with `hook_program == KIT_ID`, `hook_authority == None`,
  `mint_authority == None`, `max_supply == supply`, `1_000 <= supply <= 1e16`, and `hook_flags`
  equal to §4.2's flags for `args.modules`.
- `launch_reserve`: the token-program holding of `mint` owned by `args.launch`, with
  `amount == supply`, so `eligible` starts at 0.
- `kit_config` (init at `["kit", mint]`), `registry` (mut, `["bordrless-hook-accounts", mint]`).
- `reward_mint` (a token-program `Mint` with no hook program), `reward_vault` (mut): when module 1
  is on, the vault is created as `holding(reward_mint, kit_config)` through the token program.
- The token program, its event authority, the system program, the kit's event authority.

`KitInitArgs { launch, pool, creator, reward_mint, modules, max_wallet_bps, creator_unlock_at,
early_window_end, early_unlock_at, kit_caller_bump }`. The launch passes them; they are trusted
because the kit-caller PDA signed. Hard bounds are checked again here: `modules` is not zero and
within 0b1111; `1 <= max_wallet_bps <= 9_999` exactly when module 2 is on;
`now < creator_unlock_at <= now + 365 days` exactly when module 4 is on;
`now < early_window_end < early_unlock_at <= now + 30 days` exactly when module 8 is on. Sets
`min_eligible`, `max_wallet_amount`, `eligible = 0`, `seen = 0`. Event `KitInstalled` with every
parameter.

`graduate()`. Accounts: `kit_caller` (signer, equal to the PDA derived from `kit_config.mint` and
`kit_config.kit_caller_bump`), `kit_config` (mut), the kit's event authority. Sets `graduated` and
refuses a second time. Event `KitGraduated { mint, ts }`.

`claim()`. Accounts:

- `owner` (signer); neither `kit_config.pool` nor `kit_config.launch`, else `NotAHolder`.
- `kit_config` (mut); module 1 on, else `RewardsOff`.
- `mint` (`address = kit_config.mint`).
- `holding` (mut): a token-program `Holding` with `holding.mint == kit_config.mint` and
  `holding.owner == owner`. Holdings exist only at `["holding", mint, owner]`, so this binds it.
- `reward_mint` (`address = kit_config.reward_mint`), `reward_vault` (mut,
  `address = kit_config.reward_vault`).
- `destination` (mut): a holding of `kit_config.reward_mint` owned by `owner`.
- `kit_hook_authority`: `PDA(["hook-authority"], KIT_ID)`, which signs `write_hook_data`.
- The token program, its hook authority (constant), its event authority, the kit's event authority.

Then: sync; settle the holder at `holding.amount`; `pay = min(owed, reward_vault.amount)`;
`NothingToClaim` when `pay == 0`; transfer `pay` from the vault to `destination` (the kit config
signs as the vault's owner); `owed -= pay`; `total_claimed += pay`; write the holder's data through
`write_hook_data`, changing bytes 0..24 only (bytes 24..64 are written back exactly as read, so a
claim never unlocks an early buyer; they are all zero if nothing is left to keep); event
`RewardsClaimed { mint, owner, amount, owed_left, total_claimed }`.

`share(amount)`. Accounts: `sharer` (signer), `kit_config` (mut; module 1 on), `source` (mut, a
bridged-SOL holding the sharer may spend), `reward_mint`, `reward_vault` (mut, address-checked),
the token program, its hook authority, its event authority, the kit's event authority. Refuses
`amount < 1_000_000` lamports (0.001 SOL) and refuses while `eligible < min_eligible`
(`NoEligibleHolders`). Then: sync; transfer `amount` to the vault; `seen += amount` (so it is not
fresh); `stream_remaining += amount`, `stream_last = now`, `stream_end = now + SHARE_STREAM_SECS`
(3,600); `total_shared += amount`; event `RewardsShared { mint, from, amount, total_shared }`. The
share reaches holders over the next hour, pro rata to what they hold as it is released, so buying
just before a share and selling after it gains nothing.

`before_transfer`, `before_burn`: §4.9, signed by the token program's hook authority.

### 4.11 Solvency and overflow

Invariant after every instruction: the reward vault holds at least what every holder could claim
plus `held` plus `stream_remaining`. It rests on four facts:

1. `eligible` equals the sum of the balances that will be settled. Every balance change of the
   token passes through a kit callback (transfers and burns), minting is impossible
   (`mint_authority == None`), and excluded owners are never settled and cannot claim.
2. Every balance change of a holder is settled first, at the balance before the change.
3. Rounding always favours the vault: `inc` floors with the remainder kept exactly, `owed` floors,
   and stream releases floor.
4. Shared lamports enter `seen` when they arrive and reach the pot only as they are released.

Overflow: `1_000 <= supply <= 1e16`, total inflows below 2^64, and
`eligible >= min_eligible = supply / 1_000 >= 1` give
`balance * (acc_per_share - snapshot) <= 2^64 * 1e12 * 1_000 ~ 1.8e34 < 2^128`, and
`scaled < 2^128`. Arithmetic is checked; a revert there is a bug.

### 4.12 Runtime rules

- Callbacks make no CPIs and emit no events (no `#[event_cpi]` on callback contexts). While a
  callback runs, the token program is on the stack, so the kit cannot call it for any mint
  (A→B→A is refused).
- `claim` and `share` call the token program for the reward mint, which `init` checked has no hook,
  and `claim` calls `write_hook_data`, which calls no hook; neither re-enters the kit.
- Heights: swap → token → kit is 3; a router on top makes 4. With no events in callbacks, a routed
  swap never reaches the limit of 5. Claim and share reach 3 (the token program's event).
- Compare known addresses with constants instead of deriving them on chain (the token program's
  hook authority, the kit-caller PDA from its stored bump).

### 4.13 Off-chain mirror

A pure function of account state. The backend reads `KitConfig`, the reward vault and the holder's
holding with `getMultipleAccounts`:

```
released = stream release at `now`, as in §4.7
pending  = vault.amount + total_claimed - seen + released
acc'     = acc_per_share
if eligible > 0 and eligible >= min_eligible and pending + held > 0:
    acc' = acc_per_share + ((pending + held) * SCALE + rem) / eligible
claimable = 0 for pool and launch;
            otherwise owed + balance * (acc' - snapshot) / SCALE           // floors
```

`snapshot` and `owed` come from the holding's hook data. A claim pays `min(claimable, vault)`.
Distributed = `total_distributed` (plus `pending + held` when it would sync), claimed =
`total_claimed`, shared = `total_shared`, still streaming = `stream_remaining - released`. The
indexer also records the kit's `KitInstalled`, `KitGraduated`, `RewardsClaimed` and `RewardsShared`
events.

### 4.14 Upgrade policy and trust

The kit, token, DEX, launch and bridge programs keep their upgrade authority (the deployer key) on
localnet and devnet. Before the first mainnet launch each authority moves to a multisig behind a
public timelock, and the kit's is revoked once it is audited. The docs page and every token's rules
panel show each program's upgrade authority. Until the kit's is revoked, the site says "fixed at
launch: the creator can't change these", never "immutable" or "nobody can change them". The DEX's
pause flag stops every swap; it moves to the same multisig and the docs say so. Every program's IDL
is published on chain at deploy so explorers decode its accounts, including `KitConfig`. Before
mainnet, counsel reviews the holder-rewards wording.

### 4.15 A verified kit token

The site and the indexer treat a token as a Bordrless launch with kit rules only when a `Launch`
exists at `["launch", mint]`, `mint.hook_program == KIT_ID`, `hook_authority == None`,
`mint_authority == None`, the hook flags match §4.2 for the config's modules, and a `KitConfig` at
`["kit", mint]` has `launch` equal to that `Launch` and `pool` equal to its pool. The indexer lists
launches only from `LaunchCreated`, so look-alike mints never reach the board.

## 5. Launchpad (`programs/bordrless_launch`)

### 5.1 Arguments

`CreateLaunchArgs` gains `rules: LaunchRules`:

```rust
pub struct LaunchRules {
    pub holder_fee_buy_bps: u16,   // holder rewards are on when either side is above 0
    pub holder_fee_sell_bps: u16,
    pub burn_buy_bps: u16,
    pub burn_sell_bps: u16,
    pub max_wallet_bps: u16,       // 0 = off; lifts at graduation
    pub creator_lock_secs: u32,    // 0 = off; counted from the launch
    pub early_window_secs: u32,    // 0 = off
    pub early_lock_secs: u32,      // counted from the launch; above early_window_secs
}
```

Kit modules: 1 when a holder fee is set, 2 when `max_wallet_bps > 0`, 4 when
`creator_lock_secs > 0`, 8 when `early_window_secs > 0`.

### 5.2 Bounds

The launch `Config` (`init_config`, `set_config`) gains these bounds; `create_launch` refuses
anything outside them:

| Field | Policy |
| --- | --- |
| `max_holder_fee_bps` | 200 |
| `max_burn_bps` | 100 |
| `max_rules_fee_bps`: creator + holders + burn on one side | 300 |
| `min_max_wallet_bps`, `max_max_wallet_bps` | 100, 500 |
| `max_creator_lock_secs` | 90 days |
| `max_early_window_secs` | 300 |
| `max_early_lock_secs` | 7 days |

`init_config` and `set_config` also enforce ceilings no config can raise: holder fee 500, burn 500,
creator + holders + burn 1,000 per side, max wallet below 10,000, creator lock 365 days, early
window 3,600 s, early lock 30 days, and a launch supply of at least 1,000 base units. The kit's id
is a constant; there is no config field for it.

### 5.3 `create_launch`

`CreateLaunch` gains these accounts, present exactly when a kit module is on (Anchor optional
accounts: the launch program's id stands for an absent one): `kit_program` (address `KIT_ID`),
`kit_config` (mut, `PDA(["kit", mint], KIT_ID)`), `kit_registry` (mut,
`PDA(["bordrless-hook-accounts", mint], KIT_ID)`), `reward_vault` (mut; module 1 only),
`kit_caller` (`PDA(["kit-caller", mint], LAUNCH_ID)`), and the kit's event authority.

1. The launch fee, as v1.
2. `token.create_mint`: with any kit module on, `hook_program = KIT_ID`, the flags of §4.2 and
   `hook_authority = None`; otherwise no hook and no hook authority. Mint authority the launch;
   freeze and metadata authorities none (v1).
3. The launch's holdings of the token and of the quote, as v1.
4. `mint_to` the supply into the reserve (the kit's program account is passed because the mint has
   a hook; no callback runs, as the kit does not subscribe to mints), then revoke the mint
   authority.
5. With a kit: `kit.init`, signed by the kit-caller PDA, the creator paying.
6. The curve pool, as v1, now passing the base token-hook slice (`[kit, kit_config, reward_vault or
   the kit's id]`) so the deposit runs the kit; launch to pool is excluded to excluded.
7. The pool registry (§5.4).
8. The `Launch` account (§5.6).
9. `LaunchCreated` with the rules and the kit config.

### 5.4 Pool hook

Registry of a launch pool, after the 5 prefix accounts: `launch` (w), `launch quote holding` (w),
`holder vault` (w), `kit_config` (r), at indices 5, 6, 7 and 8. The last two are written as fixed
keys: `holder_vault = holding(quote_mint, PDA(["kit", mint], KIT_ID))` and
`kit_config = PDA(["kit", mint], KIT_ID)`, both derived on chain in `create_launch` whatever the
modules. `HookCallback` declares them with `address = launch.holder_vault` and
`address = launch.kit_config`; the kit config is read only when rewards are on.

Fees on each swap. `fee_amount` rounds up (v1), burns round down. The DEX's own take on a launch
pool is a quarter of what the hook collects (`policy::LAUNCH_PROTOCOL_SHARE_BPS`, copied from the
DEX config's `launch_protocol_share_bps` when the pool is created, §3.1): of the creator and
holder fees below, in SOL, on the curve and after graduation, and nothing when the launch's rules
collect nothing. The ordinary flat 1% is for pools anyone opens on the DEX:

- Buy, `before_swap`: the LP fee (sniper schedule). `creator = fee_amount(in, creator_fee_bps)`;
  `holder = fee_amount(in, holder_fee_buy_bps)` when rewards are on and
  `kit_config.eligible >= kit_config.min_eligible`, else 0. If `creator + holder >= in`, neither is
  taken. Deltas: creator to 6, holder to 7, each only when above 0.
- Buy, `after_swap`: `burn = out * burn_buy_bps / 10_000`; none if that is not below `out`.
- Sell, `before_swap`: the LP fee; `burn = in * burn_sell_bps / 10_000`.
- Sell, `after_swap`: creator and holder fees from the SOL output the DEX reports (the curve's
  output, less the DEX's share of any base-side cut on the way in, which is 0 for a kit token), as
  for a buy, with `holder_fee_sell_bps`. The DEX then holds its quarter of the two fees back from
  the delivery.
- The creator's own first buy: `actor == creator`, `recipient == creator`,
  `!launch.creator_bought` and `now < created_at + sniper_window_secs` give the normal LP fee
  instead of the sniper fee, once; the hook then sets `creator_bought`. A bot that buys first no
  longer takes the exemption from the creator. The buy pays the creator fee to the creator, and
  the holder fee only if someone is already eligible.
- The hook keeps `creator_fees_accrued`, `holder_fees_accrued` and `burned_on_trades` on the
  `Launch`.

### 5.5 Graduation

A launch is ready when its pool's quote reserve reaches `graduation_quote` **or its curve has sold
out** (`base_reserve == 0`). Since the LP fee no longer compounds, the reserve reaches the threshold
only as the last curve tokens are bought, and a creator's own hook cutting buys could keep it just
short for ever; the sell-out rule closes that. The reserve's top-up is `min(reserve tokens, what
the price needs)`, as before.

As v1, plus, for a launch with a kit (`launch.modules != 0`): the accounts `kit_program`,
`kit_config` (mut, `address = launch.kit_config`), `reward_vault` (address-checked when rewards are
on, else the kit's id), the kit-caller PDA and the kit's event authority are required exactly then
(`KitAccountsMissing` otherwise). The reserve's top-up transfer and its burn pass the kit's hook
accounts. After the curve is finalized, the launch always calls `kit.graduate`, which lifts max
wallet. The accounting does not change across graduation: the top-up moves between excluded
holdings and the reserve burn is from an excluded holding.

### 5.6 `Launch` and events

`Launch` gains `rules: LaunchRules`, `modules: u8`, `kit_config: Pubkey` and
`holder_vault: Pubkey` (both always derived), `creator_unlock_at`, `early_window_end` and
`early_unlock_at` (i64, 0 when off), `creator_bought: bool`, `holder_fees_accrued: u64`,
`burned_on_trades: u64`, and (§5.7, §5.8) `config: Pubkey` (the `LaunchConfig` it was made from;
the default key for inline rules), `custom_hook: Option<Pubkey>` and `custom_hook_flags: u16`.
`LaunchCreated` gains the rules, the modules, the kit config, `config: Option<Pubkey>`,
`custom_hook: Option<Pubkey>` and `custom_hook_flags`. `Graduated` is unchanged.

### 5.7 Launch configs (build your own)

A `LaunchConfig` is an account of the launch program that anyone creates with the SDK
(`create_config`), fixed once created and usable for any number of launches: the rules
(`LaunchRules`), `creator_fee_bps`, `custom_hook: Option<Pubkey>` with `custom_hook_flags`, a
`label` (at most 32 bytes) and the `creator` who made it. It is a keypair account (`init` on a
signer, no seeds), so its key is what a creator shares and pastes into the launch form; the site
reads it, shows every rule and the hook, checks it against the bounds and launches with it. The
presets and the mix-and-match "Custom" rules stay inline (`CreateLaunchArgs.rules`); a config is
the "Build your own" path. A config can never touch the protocol's share: the DEX applies it from
its own config when the pool is created (§3.1).

`create_config` accounts: `creator` (signer, pays), `config` (the launch program's `["config"]`,
for the bounds), `launch_config` (the new keypair account, signs), `hook_program?` (present
exactly when `custom_hook` names a program), the system program, the event authority and the
program. Checks: the label's length (`InvalidLabel`), the creator fee against the config's
maximum (`CreatorFeeTooHigh`), the rules against the config's bounds (`check_rules`, the §5.2
errors), and the custom hook's rules (§5.8). Event `LaunchConfigCreated` with every field.

`create_launch` gains the optional account `launch_config` (after the kit accounts; this
program's id for none). When present, the rules, the creator fee and the hook come from it, and
`CreateLaunchArgs.rules` and `creator_fee_bps` must equal the config's (`ConfigMismatch`): what
the site showed is what launches. The bounds and the creator fee are checked again at every launch,
since the launch config may have changed since the `LaunchConfig` was made. `Launch.config` keeps
the key.

**Listed configs and the author's share (the marketplace; upgrade of 2026-10-08).**
`create_listed_config(args, author_share_bps)` makes a config exactly as `create_config` does, plus
the author's share of the creator fee: 1 to `MAX_AUTHOR_SHARE_BPS` (5,000, half) basis points of
it, in `LaunchConfig.author_share_bps` (two of its reserved bytes; a plain config keeps 0), fixed
for ever. It emits `ConfigListed { config, author, author_share_bps, ts }` after
`LaunchConfigCreated`. A launch made from it by anyone but its author snapshots the share into
`Launch.author_share_bps` (the author launching from their own config, or any other config, gets
0); `Launch.author_fees_paid` totals what the author has been paid (ten of the launch's reserved
bytes). The creator fee still accrues in the launch's quote holding; every claim splits what it
takes, `floor(amount * share / 10_000)` to the author's holding of the quote and the rest to the
creator's, whoever claims:

- `claim_creator_fees`, signed by the creator, takes the `LaunchConfig` and the author's holding as
  its remaining accounts when the launch has a share (`AuthorAccountsMissing`, `WrongHolding`);
  without a share it is unchanged.
- `claim_author_fees`, signed by the config's author (`NotAuthor`), takes the config, both holdings
  and the token program's accounts; refused for a launch without a share (`NoAuthorShare`).

Both emit `CreatorFeesClaimed` (the creator's part) and `AuthorFeesPaid { launch, mint, config,
author, amount, paid_total, slot, ts }`. The share is of the creator fee only: Bordrless's share and
the holders' are untouched. Tested in `programs/tests/tests/marketplace.rs`.

### 5.8 A creator's own token hook

A `LaunchConfig` may name a hook program (`custom_hook`) with its token flags. The owner's
decision: "someone could make their own function with the hook the same way anyone can do anything
with Uniswap v4 hooks on eth". The launchpad does not vet its code; the site labels the token
"Custom hook, unverified" everywhere (§7.4). Rules, checked by `create_config` and again by
`create_launch`:

- No kit module may be on in that config (`CustomHookWithKitRules`): one token hook per mint.
  Burn and the creator fee are pool hook rules and may be combined with it.
- The hook is none of the protocol's programs (`PROTOCOL_PROGRAMS`: the token program, the DEX,
  the bridge, the launch, the kit), not the system program and not the default key, and it is
  executable (`InvalidCustomHook`; the program account is passed to `create_config` and checked).
- The flags name at least one callback and no unknown bit (`InvalidCustomHookFlags`); without a
  hook they are 0.

The mint is created with that hook and `hook_authority = None`, so which program runs is fixed
from the token's first instruction. Because the mint does not exist before the launch, the hook
must be prepared for the mint address beforehand: its registry at
`["bordrless-hook-accounts", mint]` under its program (plus its own per-mint accounts; `tax_hook`'s
`prepare(mint, collector, fee, cap)` is the worked example, `hook_tester`'s `init_script` the test
one). `create_launch` with a custom hook takes, as remaining accounts, the hook program, the token
program's signer for it (`["hook-authority", hook]` under the token program), the registry and the
registry's extras, resolved by the client as for any token instruction. Before anything is
created it checks: the program is the config's and executable (`InvalidCustomHook`), the signer
(`WrongHookSigner`), the registry at its address, owned by the hook and decoding
(`HookRegistryMissing`: "prepare the hook for the mint first"), and exactly the registry's number
of extras (`HookExtrasMismatch`); without a custom hook there may be no remaining accounts
(`UnexpectedCustomHookAccounts`), with one they are required (`CustomHookAccountsMissing`). The
slice `[hook, signer, extras...]` then goes along on the supply `mint_to` (a custom hook may
subscribe to mints; the kit's slice does not, since the kit does not) and as the base token-hook
slice of the pool deposit, as the kit's does. `graduate` takes the same slice without the registry
as remaining accounts for the reserve's top-up and burn. Every swap passes it as the base side's
token-hook slice (`bordrless_launch::client::swap_with_base_slice`, `custom_hook_slice`).

What such a hook can do, and the DEX stays sound through: refuse transfers (a honeypot that lets
buys through and refuses sells: the swap fails whole, nothing moves, the pool's books balance), or
take deltas of its own from every transfer of the token (then the DEX measures them as cuts and
takes Bordrless's share of their value, §3.1). It cannot touch SOL, the reserve or anyone else's
token: the token program's delta rules (§2.4) hold for it as for the kit.

## 6. Transactions

- Every transaction the backend prepares is v0. The protocol lookup table
  (`PROTOCOL_LOOKUP_TABLE`) holds fixed addresses that no top-level instruction invokes: event and
  hook authorities, config PDAs, the bridged-SOL mint and the SOL wrapper's accounts, the kit
  program, the system and associated-token programs. Programs a transaction calls at top level
  (compute budget, token, bridge, DEX, launch, the kit for claims and shares) stay in the static
  keys: a v0 message cannot load an invoked program from a table. Per-mint accounts cannot be in it.
- `pnpm admin init` creates and extends the table, and extends it when a config address changes.
  The table is frozen before mainnet; a change after that means a new table and a new environment
  value.
- The site signs v0 transactions with the wallet first and the launch's mint key after it.
  `apps/web/src/lib/tx.ts` refuses a versioned transaction with extra signers today; it must
  co-sign v0 transactions. A fresh mint keypair is made for every launch attempt.
- Compute: the backend simulates each prepared transaction (signatures not verified, blockhash
  replaced) and requests the units used plus 15%. Fallbacks when simulation fails: create_launch
  600k, a trade 400k, a trade with graduation 700k, claims 100k per mint.
- The creator's first buy is its own transaction after the launch lands. It is never merged into the
  launch transaction (about 75 trace entries) and never called from `create_launch` (re-entry).
- "Sell 100%" appends a claim (and unwrap) to the sell only when the mirror shows something
  claimable; a claim of nothing reverts.
- Claims pack about 4 mints per transaction.

Budget per path, estimated; the tests measure each and assert a ceiling:

| Path | Keys | v0 bytes | Trace entries | CPI height | Compute |
| --- | --- | --- | --- | --- | --- |
| create_launch, every module | ~32 | ~950 | ~45 (about 71 if all 13 new addresses were pre-funded) | 4 | ~400k |
| Buy with SOL in, every rule | ~30 | ~850 | ~32 | 3 | ~320k |
| Sell with SOL out, every rule | ~30 | ~850 | ~30 | 3 | ~300k |
| Sell 100% with claim and unwrap | ~36 | ~1,000 | ~40 | 3 | ~380k |
| Buy and graduate | ~36 | ~1,000 | ~48 | 4 | ~520k |
| Claim one mint and unwrap | ~20 | ~700 | ~10 | 3 | ~70k |
| Wallet-to-wallet transfer | ~12 | ~500 | 3 | 2 | ~45k |

Limits on mainnet today: 64 trace entries, invoke height 5, 1,232 bytes, 64 account locks.

## 7. Policy and site rules


**Who may upgrade a custom hook (upgrade of 2026-10-08).** A token's hook runs on every transfer,
so a hook someone could upgrade could be swapped for other code after the token launched.
`create_config` and `create_listed_config` therefore take the hook's ProgramData account
(`PDA([hook], BPFLoaderUpgradeab1e...)`) as their one remaining account, and refuse
(`HookUpgradeable`) a hook whose upgrade authority is anyone other than:
- no one (immutable; the BPF loader 2, or a finalized loader-v4 program, likewise);
- Bordrless Studio's upgrade key `CS1NRyXNCPxEUP4CRoa26cHQSeSJCxXh5SPijwFhDW6W`, which holds every
  "managed" Studio hook and upgrades it only to a version Studio reviewed and rebuilt;
- the protocol's upgrade authority `5xsibKwtiN6ruxsYrEyWVpV3KcwuzSPbQd1n28a7spEd` (Half-Life,
  tax_hook).

These are `HOOK_UPGRADE_AUTHORITIES`. A missing or wrong ProgramData account is
`HookProgramDataMissing`. `create_launch` doesn't check again: once a config is made, only those keys,
or no one, can change who upgrades its hook. The backend's marketplace watch takes a listing down if
that ever changes, or if a Studio hook's code on chain stops matching its build. Studio deploys a hook
immutable (the default) or managed; it never hands the upgrade authority to the owner. Tested in
`programs/tests/tests/hook_authority.rs`.

### 7.1 Choices

| Rule | Choices | Default |
| --- | --- | --- |
| Holder rewards | off, 0.5%, 1%, 2%; buys and sells set together, or apart under Custom | 1% both sides |
| Creator fee | 0, 0.5%, 1%, 2% | 0.5% with holder rewards on, else 1% |
| Burn | off, 0.25%, 0.5%, 1%; both sides, or apart under Custom | off |
| Max wallet | off, 1%, 2%, 3%, 5%, until graduation | 2% |
| Creator wallet lock | off, 7, 30, 90 days | 30 days |
| Early-buyer lock | off; first 30 s, 60 s or 5 min; locked until 15 min, 1 h or 24 h after launch | off |

Presets on the launch form (2026-10-07: four remain; the default is "Plain"; the list is
`bordrless_core::policy::presets`, which the TypeScript `RULE_PRESETS` is pinned to through the
fee vectors):

| Preset | Rules |
| --- | --- |
| Plain | none; creator fee 1% |
| Diamond hands | early-buyer lock (first 5 min, until 24 h), max wallet 1%, creator wallet lock 90 days, holder rewards 1% both sides, creator 0.5% |
| Burn | burn 0.5% both sides, holder rewards 0.5%, creator 0.5% |
| Paid to hold | holder rewards 2% on sells only, creator 0.5%, creator wallet lock 30 days |

Beside them, two more options in the same row and style: **Custom** (mix the rules above by hand
on the launch page: the form's rule controls, launched as inline rules) and **Build your own**
(write a hook with the SDK, make a `LaunchConfig`, paste its key; the site reads it, shows every
rule and the hook, checks it and launches with it, §5.7). "Holders first", "Fair start",
"Scorched" and "Community" are gone everywhere (shared `RULE_PRESETS`, the home console, the
launch form, the docs, fixtures and tests).

Creator + holders + burn may not exceed 3% on either side; the form shows the total and blocks
above it.

### 7.2 One name per rule, on every surface

Holder rewards. Share with holders. Burn. Max wallet. Creator wallet lock. Early-buyer lock.
Sniper fee. Creator fee. Never "yield", "APR", "APY", "reflections" or "tax". The site ships
together with the kit, so v2 rules are shown as features; a figure the API cannot read yet is a
dash.

### 7.3 What the site says, and how

- Holder rewards are paid in SOL. Show realized figures only (paid all-time, paid in 24 h,
  claimable), never a projected rate. Rewards accrue "with every trade", not "by the second". They
  stay with the wallet that earned them and do not expire.
- With holder rewards on: "no program can hold this token (no other pool, vault or multisig), so
  every pool trade happens on its launch pool".
- Without holder rewards, anyone can open another pool for the token. Burn, the creator fee and the
  buy and sell rates apply only on the launch pool. The site trades only through the launch pool,
  and /pools and the token page label other pools "launch rules don't apply here".
- Max wallet: "No wallet can hold more than 2% (20,000,000 TICKER) until graduation. Never blocks a
  sell on the launch pool." Show the cap in tokens and in SOL at the current price.
- Creator wallet lock: "The creator's wallet can't sell, send or add liquidity until 6 Nov, 14:00
  UTC. Other wallets are not covered." Show the wallet's live share of supply; when it holds
  nothing, say that instead of showing a lock over an empty wallet.
- Early-buyer lock: "Tokens bought in the first 60 seconds can't be sold or sent until 15:42." A
  buy during the window says so next to the Buy button before it is made.
- Share with holders: "Shared SOL reaches holders over the next hour, in proportion to what they
  hold." Disabled, with the reason, while nobody is eligible yet.
- Burn: a fee paid in tokens and destroyed. It does not push the price up; fewer tokens can ever be
  sold back into the pool, and the market cap shown falls with the supply.
- Rules are "fixed at launch: the creator can't change these" (§4.14).
- Fees: one figure per side, in the order the DEX charges them, with `s = 0.25` Bordrless's share
  of the fees (§3.1; "Bordrless takes 25% of what a launch's rules collect"):
  buy `1 - (1 - (creator + holders)(1 + s))(1 - lp)(1 - burn_buy)`;
  sell `1 - (1 - burn_sell)(1 - lp)(1 - (creator + holders)(1 + s))`,
  where `lp` is the current LP fee (the sniper fee while elevated) and `holders` is 0 while nobody
  is eligible. Each side is rounded up to a basis point (`sideFeeBps` in `packages/shared`): the
  default "Plain" (creator 1%, LP 0.3%) gives buy 1 − 0.9875 · 0.997 = 1.5437% and sell the
  same, 155 bps; "Diamond hands" (holders 1%, creator 0.5%) gives 1 − 0.98125 · 0.997 =
  2.1694%, 217 bps a side. The site prints a side to a tenth of a point, rounded up so it never
  understates: "Buy 1.6% · Sell 1.6%" for Plain, "Buy 2.2% · Sell 2.2%" for Diamond hands, with
  the sniper fee apart and every breakdown naming who receives each part (Bordrless's line is its
  quarter of the creator and holder fees; it is 0 when they are).
- Price impact is the curve's movement only; fees are their own line; the effective price includes
  every fee and the burn.
- Graduation is stated in SOL raised ("graduates when the pool holds 75 SOL"); a dollar figure is
  approximate.
- Liquidity: "Tokens you add to the pool stop earning holder rewards; the pool's LP fee is what
  they earn instead." A creator under the wallet lock cannot add; the unlock time is shown.
- A claim smaller than 0.0005 SOL (about 100 times a claim's network fee) is hidden by default.

### 7.4 Surfaces

- Home: the introduction above the projects board. The claim (transfer hooks are played out:
  limited by design, they can only say no) and what a Bordrless hook does instead; a comparison
  built only from the Why statements and the table above; the rules a launch can have, with the
  safety rails marked honestly; live figures from `Overview` including its v2 totals (paid to
  holders, kept from snipers, burned, creator fees), each a dash when it cannot be read. Compact
  enough that the argument lands before the scroll; on phones the comparison may fold into a
  disclosure. The `app/layout.tsx` description matches.
- Launch form: a "Token rules (fixed at launch)" section after the creator fee, with the four
  presets, "Custom" (one row per rule with its choices and one line of consequence in real
  numbers, the total per side) and "Build your own" (a field for a `LaunchConfig` key; the site
  reads the config, shows its rules, its creator fee and its hook, checks it against the bounds
  and launches with it), and a preview of the card buyers will see. The first-buy field is clamped
  to max wallet with the reason shown. Before signing, one confirmation lists every permanent
  choice.
- Labelling (§5.8): a token launched from a config with a custom hook is "Custom hook, unverified"
  on every surface (board card, token page header and rules panel, trade panel line, portfolio,
  pools): the program id, its upgrade authority or "fixed", and one warning sentence: "Written by
  the creator, not Bordrless. It can refuse transfers or take part of them." Only kit rules get
  the normal rules treatment. The home console shows the four presets plus Custom and Build your
  own as read-only tiles in one row ("Custom: mix the rules above, on the launch page"; "Build
  your own: write a hook with the SDK, paste its config key").
- Board: one rules line per card in text ("Holders 1% · Max 2% · Creator 1.9% locked 27d", with
  "+1" when it doesn't fit), a total-fee chip, and "Paid to holders" as one of the card's figures
  when rewards are on. Filters: pays holders, burns, creator locked, early-buyer lock, lowest fees.
  Sort: paid to holders in 24 h.
- Token page: a Rules panel under the trade panel (before the chart on phones), one row per rule
  with its parameter, its consequence, its live state and what it doesn't cover; the fee table;
  "Your rewards" with "Claim as SOL"; "Share with holders"; and a verify line linking the launch
  transaction, the kit config and the kit program with its upgrade authority. The holders list marks
  the pool and the launch "earns no rewards". Trades list the holder fee and the burn. Ended
  states: "No rewards paid yet", "Unlocked on 6 Nov", "Lifted at graduation".
- Trade panel: a breakdown that adds up ("You pay 1 SOL: pool 0.003, Bordrless 0.01, creator 0.005,
  holders 0.01; 1,234,567 TICKER out, 6,172 burned; you receive 1,228,395"). With max wallet, the
  allowance is shown before typing, presets above it are disabled with the reason, and Max fills
  it. A locked creator or early buyer sees Sell disabled with the unlock time. In the early window
  a buy shows when its tokens unlock. "Sell 100%" claims rewards in the same transaction when there
  are any, and says so. Errors sit next to the button.
- Pools: the liquidity notes of §7.3, and "launch rules don't apply here" on a second pool of a
  launched token.
- Portfolio: claimable rewards per token, including tokens the wallet no longer holds; claim all
  (batched, unwrapped to SOL by default, the temporary bridged-SOL holding closed). Locked amounts
  show their unlock time.
- Docs: the protocol, the kit, the 64-byte hook-data layout and the upgrade policy. The kit's reward
  accounting replaces `tax_hook` as the worked example (a transfer tax is the one thing the
  transfer-fee extension already does). The current `#why` paragraph of
  `apps/web/src/app/docs/page.tsx` and `docs/architecture.md` (lines 5-7 and 114-116) are replaced
  by this document's Why statements.

## 8. App

### 8.1 Shared types (`packages/shared/src/api.ts`)

Names are fixed so the backend and the site can be built at the same time.

```ts
export interface LaunchRules {
  holderFeeBuyBps: number;
  holderFeeSellBps: number;
  burnBuyBps: number;
  burnSellBps: number;
  maxWalletBps: number;              // 0 = off; lifts at graduation
  creatorUnlockAt: number | null;    // unix seconds
  earlyWindowEndsAt: number | null;
  earlyUnlockAt: number | null;
  walletsOnly: boolean;              // holder rewards on: no program can hold the token
}
export interface LaunchRulesInput {
  holderFeeBuyBps: number;
  holderFeeSellBps: number;
  burnBuyBps: number;
  burnSellBps: number;
  maxWalletBps: number;
  creatorLockDays: number;           // 0 = off
  earlyWindowSecs: number;           // 0 = off
  earlyLockSecs: number;             // counted from the launch
}
export interface RuleStats {
  rewardsDistributed: Amount | null; // lamports
  rewardsClaimed: Amount | null;
  rewardsShared: Amount | null;
  rewardsStreaming: Amount | null;   // shared, not yet released
  rewardsUnclaimed: Amount | null;   // the reward vault's balance
  rewardsPaid24h: Amount | null;
  eligibleSupply: Amount | null;
  burned: Amount | null;             // base units burned on trades (not the graduation burn)
  liveSupply: Amount | null;
  creatorBalance: Amount | null;
  maxWalletAmount: Amount | null;
  maxWalletLifted: boolean;
}
export interface ProgramInfo { address: Address; upgradeAuthority: Address | null; upgradeable: boolean | null }

// LaunchSummary gains:
//   rules: LaunchRules; kitConfig: Address | null; preset: string | null;   // preset name, 'custom', or null
//   config: Address | null;                          // the LaunchConfig it was made from (§5.7)
//   customHook: { program: Address; flags: number; upgradeAuthority: Address | null } | null;   // §5.8
//   buyFeeBps: number; sellFeeBps: number;           // §7.3 formulas, outside the sniper window
//   rewardsDistributed: Amount | null; rewardsPaid24h: Amount | null; burned: Amount | null;
//   creatorShare: number | null;                     // the creator wallet's share of supply, 0 to 1
// LaunchDetail gains: ruleStats: RuleStats; holderVault: Address | null;
// Overview gains:
//   programs.kit: Address;
//   programInfo: Record<'token' | 'swap' | 'bridge' | 'launch' | 'kit', ProgramInfo | null>;
//   totals: { rewardsDistributed: Amount | null; sniperFeesKept: Amount | null;
//             burnedValue: Amount | null; creatorFees: Amount | null }   // lamports
// Trade gains: recipient: Address; creatorFee: Amount; holderFee: Amount; burn: Amount;
// Holder gains: earnsRewards: boolean.
// PortfolioEntry gains: claimableRewards: Amount | null;
//   locked: { amount: Amount; until: number; reason: 'creator_lock' | 'early_lock' } | null;
//   and Portfolio lists launches the wallet no longer holds when something is claimable.
// SwapQuote gains:
//   holderFee: Amount;                 // bridged SOL
//   burn: Amount;                      // token base units
//   totalFeeBps: number;               // §7.3 formulas at this moment (sniper fee included while elevated)
//   effectivePrice: number;            // whole quote per whole token, all fees and the burn included
//   maxWalletLeft: Amount | null;      // tokens the recipient can still receive
//   maxInForWallet: Amount | null;     // the input that fills that allowance
//   blocked: { reason: 'max_wallet' | 'creator_lock' | 'early_lock'; until: number | null } | null;
//   lockedUntil: number | null;        // a buy in the early window: when its tokens unlock
//   amountOut is what reaches the wallet (after burn); priceImpactBps is the curve's movement only.
// LaunchPrepareRequest gains: rules: LaunchRulesInput;
// LaunchPrepareResponse gains: devBuyMaxLamports: Amount | null; buyFeeBps: number; sellFeeBps: number;

export interface RewardPosition {
  mint: Address; symbol: string; name: string; image: string | null; decimals: number;
  balance: Amount; claimable: Amount; claimed: Amount | null;
}
export interface RewardsList { owner: Address; positions: RewardPosition[]; totalClaimable: Amount }
export interface RewardsPrepareRequest { owner: Address; mints: Address[]; unwrap: boolean }
export interface SharePrepareRequest { owner: Address; mint: Address; amount: Amount; solMode: SolMode }
export interface RewardEvent { kind: 'claim' | 'share'; owner: Address; amount: Amount; ts: number; signature: string }
// GET  /v1/rewards/:owner              -> RewardsList
// POST /v1/rewards/prepare             -> { transactions: PreparedTx[] }
// POST /v1/share/prepare               -> { transactions: PreparedTx[] }
// GET  /v1/launches/:mint/rewards      -> { events: RewardEvent[] }
// GET  /v1/launches?rules=rewards,burn,max_wallet,creator_lock,early_lock&sort=rewards24h|fees
```

Definitions:

- `rewardsPaid24h`: holder fees paid into the holder vault in the last 24 h (the `Swapped` deltas
  whose destination is the holder vault) plus `RewardsShared` amounts, in lamports.
- `sniperFeesKept`: over launch-pool swaps, the LP fee charged minus the fee the pool's base rate
  would have charged, in lamports (a sell's LP fee, in tokens, valued at that swap's price).
- `burnedValue`: over trade burns, the amount burned times that swap's pool price, in lamports.
- `preset`: the §7.1 preset whose rules and creator fee a launch matches exactly, `custom`
  (inline rules that match none, or a config without a custom hook), or `custom-hook` (a config
  with one).
- The protocol fee of a launch pool is `Swapped.protocol_fee` as before; a launch's "paid to
  Bordrless" total is the sum of them. Quotes compute it as §3.1's share, never as a rate of the
  trade.

### 8.2 Backend

- Quote math in `packages/shared` mirrors the pool hook and the DEX exactly (the order of §3.1, the
  rounding, the creator + holder guard, burn on the right side, the eligible threshold, the protocol
  fee in quote on both sides), pinned to the Rust code with shared fixture vectors. `maxAmountIn`
  divides by `(1 - lp - protocol) * (1 - creator - holder)`; `graduates` uses the input after the
  creator and holder fees; `maxWalletLeft` uses the fixed cap.
- `launch/prepare` validates the rules against the config and computes `devBuyMaxLamports` as the
  largest input whose delivered output (after the buy burn) stays within `max_wallet_amount` at the
  opening reserves, with the fees of the creator's own first buy: the normal LP fee, the protocol
  fee and the creator fee, and no holder fee (nobody is eligible). If a bot buys first the creator
  only receives less. A larger first buy is refused with the same sentence the form shows.
- Rewards: the mirror of §4.13 for `GET /v1/rewards/:owner` and portfolio claimables. Claim
  transactions create the bridged-SOL holding when missing, claim up to 4 mints, unwrap to SOL and
  close the holding when `unwrap` is set. Share transactions wrap SOL first when `solMode` is SOL.
- Indexer: the kit's events, `Swapped` v2 (creator and holder fees told apart by destination: the
  launch quote holding and the holder vault), burns, the platform totals of §8.1, and each launch's
  preset.
- Program info: each program's upgrade authority read from its ProgramData account, cached.

## 9. Tests that must pass before anything is called done

LiteSVM against the built programs, using `LiteSVM::new()` (it loads the mainnet feature set), plus
a test-only program `hook_tester` that answers configurable `HookReturn`s, tries forbidden
`write_hook_data` calls and routes swaps.

- Token: hook data by answer on transfer, mint and burn, and by `write_hook_data`;
  `write_hook_data` refused for another program's PDA, for the hook authority of a different mint's
  hook, for a mint without the flag, and for a holding of another mint; three deltas;
  `DeltaTooLarge` on a sum above the amount and on overflow; a duplicate delta account; a delta to
  the source or destination; more than three deltas; a burn answered by a token callback and hook
  data answered without the flag (`UnsupportedHookReturn`); `close_holding` refused with data under
  the flag and allowed without a hook, and refused with another mint's account; a registry created
  at an address that already holds lamports.
- DEX: a pool hook answering deltas and burn on both sides (balances, reserves, supply,
  `min_amount_out`, the `Swapped` fields, `recipient` in the args); the protocol fee taken in quote
  on buys and on sells, with reserves and `protocol_fees_quote` exact; `MintNotWritable`;
  `DeltaTooLarge`; forbidden delta accounts; protocol fees collected from a kit-token pool after
  sells, then again after the DEX admin changes the fee collector.
- Kit, binding: a substituted `kit_config`, a wrong reward vault, a missing reward vault with
  rewards on, a foreign mint whose hook is the kit (it cannot move), `init` without the kit-caller
  signature, `init` with a supply below 1,000 or a reward mint with a hook, `graduate` with another
  token's config, and claims by the pool or the launch, with a foreign holding, another owner's
  holding, a delegate signer or a destination owned by someone else: each refused.
- Kit, modules: each module alone, each pair and all four, each through `create_launch` and a
  transfer. A wallet-to-wallet transfer with every rule on moves the full amount, pays no fee and
  leaves every vault unchanged. With rewards on: transfers to a PDA owner, to the kit config, to the
  launch, to `Pubkey::default()` and to a program id refused; a second pool cannot be created.
  Without rewards: a burn-only launch's second pool burns nothing, and a max-wallet launch's second
  pool refuses sells at the cap until graduation. Max wallet: a buy at the cap lands, one unit over
  is refused, a sell into the launch pool is never blocked, the cap lifts at graduation. Creator
  wallet lock: no sell, send or liquidity add before the unlock; claims and shares still work;
  everything works after a clock warp. Early-buyer lock: tokens bought in the window cannot be sold,
  sent or burned; an early buyer who claims during the lock still cannot; tokens received later from
  a wallet can move; all can after the unlock, and the data clears so the holding closes.
- Kit, money: a seeded property test of transfers, burns, buys, sells, partial and full claims,
  shares and direct donations, including stretches with `eligible` below `min_eligible`; after every
  step the vault covers every claimable amount plus `held` and the stream, `eligible` equals the sum
  of holders' balances, and each claim pays exactly what the mirror computed. The economics review's
  hand example (one holder of 1.5e12, inflows of 2, 0, 1, 1, 1 lamports) pays at most 5. A share
  releases linearly over the hour, a buyer who arrives after a share only shares what is released
  after their buy, and a share is refused while nobody is eligible. A holder who sells out claims
  and closes the holding.
- Launch: no rules gives no hook and no hook authority; rules give the kit as hook from creation
  with the right flags and no hook authority; burn alone gives no hook. The fee math of §5.4 on buys
  and sells, both sides' rates, the creator + holder guard, the bounds and the hard ceilings, all
  checked against the TypeScript vectors. The holder fee is skipped while `eligible` is below the
  threshold. The creator's first buy into their own wallet pays the normal LP fee, even after a bot
  bought first, and only once; into another wallet it pays the sniper fee. A substituted holder
  vault at index 7 is refused. A first buy at the max-wallet cap lands and one above it is refused.
  Graduation with every rule on: supply, top-up, price continuity, max wallet lifted, `eligible`
  unchanged, `kit.graduate` bound; graduation of a kit launch with any kit account replaced by the
  program id is refused. `create_launch` still fits with 3 of its new addresses pre-funded.
- Runtime: for each path of §6, the compute used, the number of inner instructions, the CPI height
  and the v0 size with a lookup table loaded into LiteSVM, each asserted under a ceiling. A swap
  routed through `hook_tester` with every rule on. `create_launch` from a config and with a custom
  hook, likewise.
- Fee share and configs (`programs/tests/tests/launch_configs.rs`): Bordrless's quarter on buys and
  sells with each preset, in exact lamports; a Plain launch with creator fee 0 pays nothing; a 1%
  creator fee pays 0.25% of the trade; a token launched from a config (fields, reuse, the argument
  match, the bounds at creation and again at launch); a custom-hook launch with `hook_tester` (the
  slice on the supply mint, the deposit, buys, sells, a transfer and the graduation, the pool
  hook's rules still applied); `tax_hook` prepared for the mint (its cut on buys and sells, valued
  and shared, the pool's books balancing); every refusal of §5.8; the honeypot (sells fail, buys
  work, transfers work, the pool stays sound, fees collect).
- TypeScript: `packages/shared` vectors for the fee math, burn, max wallet, the early-window lock
  time, `devBuyMaxLamports` and the rewards mirror.
- The whole flow through the API on the local validator (`apps/server/test/localnet/e2e.ts`):
  launches with presets, buys, sells, a claim, a share, graduation, the rewards endpoints, portfolio
  claimables, and `launch/prepare` refusing a first buy above `devBuyMaxLamports`. Then the same
  through the site in a browser with the localnet dev wallet.

## 10. Not in v2

Recorded so nobody builds them by accident: fees that step down at graduation, perks for holders
of the platform token, a referral share, a sell fee that falls with holding time, rewards weighted
by holding time, a creator lock that unlocks linearly, and v1 (4,096-byte) transactions.

## Implementation notes

Decisions taken while building, where this specification is silent. None weakens a rule above.

### Protocol and token standard (§1, §2)

1. The protocol crate checks an answer (`HookReturn::check`, `read_answer`) and reports why it is
   refused; each program maps that to its own error codes, appended to its error enum so earlier
   codes keep their numbers. Token program and DEX alike: an answer that does not decode (or has
   bytes left over) is `InvalidHookReturn`; more than three deltas `TooManyDeltas`; a delta of
   zero `ZeroDelta`; a field the callback or flags do not allow `UnsupportedHookReturn`; a sum
   that overflows `DeltaTooLarge`; an account index named twice `InvalidDeltaAccount`, as §2.4
   says for an account named twice.
2. "Appears once" is checked twice: by index in the crate, and by key in the token program, since
   a client can pass the same holding at two indices of the extras.
3. §2.4's account rules all give `InvalidDeltaAccount`, a frozen delta holding included (v1 gave
   `Frozen`). All deltas are checked before any is credited.
4. The return data of a callback that may answer nothing (§1.1: the flags allow no field for it)
   is never read, so whatever it holds is ignored. `after_*` token callbacks never answer.
5. `TokenHookArgs.source_hook_data` and `destination_hook_data` are its last two fields (after
   `supply`); in the `After` phase they hold the data as written by the operation.
   `PoolHookArgs.recipient` follows `actor`.
6. `write_hook_data` makes exactly the checks of §2.2 and no others (a frozen holding's data can
   still be written by its hook). A mint without a hook or without `WRITES_HOOK_DATA` gives
   `HookDataNotWritable`; any other signer `NotHookAuthority`; a holding of another mint
   `MintMismatch` (`has_one`).
7. `close_holding` takes `owner`, `mint`, `holding`, `destination` (then the event authority and
   the program), and refuses data that is not all zero with `HookDataNotEmpty`.
8. `write_registry` treats a registry address still owned by the system program as not created:
   with no lamports it creates it; with lamports it tops it up to the rent-exempt minimum (at least
   one lamport), allocates and assigns it, as Anchor's `init` does. An address the hook already
   owns is rewritten in place when it has room.

### DEX (§3)

1. A swap answer's cut is checked whole before any token moves: `sum(deltas) + burn` (summed in
   checked arithmetic by the protocol crate) below the side's amount, else `DeltaTooLarge`; then
   every delta account; then the mint for a burn. The side's amount is the trader's input before,
   and after: the curve's output on a buy, the curve's output less the protocol fee on a sell.
2. Every delta-account failure is `InvalidDeltaAccount`: an index in the prefix or past the
   extras, a read-only account, an account that is not a token-program holding, a holding of the
   other side's mint, either vault, either of the trader's holdings (the input one and the one the
   output goes to), and an account named twice, by index (protocol crate) or by key (the same
   holding at two indices). A frozen target fails in the token program (`Frozen`).
3. `MintNotWritable` (6034) is checked only when an answer burns. A mint passed writable to a swap
   that burns nothing is accepted (the client only write-locks it).
4. A sell whose protocol fee (rounded up) takes the whole curve output is refused with the new
   `FeeExceedsOutput` (6035), as `FeeExceedsInput` refuses fees that take the whole input. The
   reference math is `bordrless_core::swap_amounts` (`Reserves`, `SwapAmounts`, `SwapFailure`):
   fees exceeding the input give `FeeExceedsInput`, a curve that gives nothing or more than the
   real reserve `InsufficientLiquidity` (as in v1), and this case `FeeExceedsOutput`.
5. `after_swap` is told `amount_in` = what reached the input vault, `amount_out` = the output it
   may cut (§3.1), `lp_fee_bps` = the fee applied, and the reserves after the swap; the pool
   account is written before the call, so a hook that reads it sees the same state. `swap_count`
   stays the count before this swap in both phases. (v1 told `after_swap` the reserves before.)
6. Each side's token-hook slice is used for every transfer and burn of that side: the hook's
   deltas, its burn and the transfer of the rest. A token hook whose extra accounts depend on the
   destination holding can therefore not be paid deltas to other holdings in a swap; `tax_hook` and
   the kit list the same accounts for every holding of a mint.
7. `DeltaPaid.amount` is the amount sent. A token hook on that mint may take its own cut, so the
   holding can gain less. `Swapped.amount_out` stays what the curve gave (before a sell's protocol
   fee); the fields are, in order: `pool`, `trader`, `recipient`, `direction`, `amount_in`,
   `deltas_in`, `burn_in`, `received_in`, `lp_fee`, `protocol_fee`, `lp_fee_bps`, `amount_out`,
   `deltas_out`, `burn_out`, `delivered_out`, the reserves, `swap_count`, `slot`, `ts`.
8. `collect_protocol_fees(quote_hook_accounts)` stays signed by the admin. Accounts: `admin`,
   `config`, `pool` (w), `quote_mint`, `quote_vault` (w), `collector_quote` (w, a holding of the
   quote mint owned by the config's fee collector, else `WrongHolding`), the token program, its
   hook authority and event authority, then the event authority and the program; the remaining
   accounts are exactly `quote_hook_accounts` (else `AccountCounts`). Nothing accrued moves
   nothing and still emits `ProtocolFeesCollected { pool, quote_amount, collector, ts }` (v1's
   `base_amount` is gone).
9. `Pool.protocol_fees_base` is removed (the account is 8 bytes shorter; the layout version stays
   1, since nothing is deployed). `finalize_curve` takes the whole base vault as the base reserve.
   `base_volume` and `quote_volume` add what reached the input vault and what the curve gave.
10. `Config.launch_protocol_share_bps` (§3.1; it was `launch_protocol_fee_bps` for a day) sits
    after `pools_created`, in two of the former reserved bytes (`reserved: [u8; 62]`);
    `ConfigArgs.launch_protocol_share_bps` is last in the args and `ConfigSet` carries it after
    `protocol_fee_bps`. `create_pool` picks the model by `curve` (a curve already requires the
    hook caller). The suites check a launch's pool carries the share model (`fee_model` 1, share
    2,500, flat rate 0) through graduation and a pool created directly the flat model (`fee_model`
    0, 100 bps, share 0).

### Kit (§4)

1. Errors (`KitError`, 6000 onward): `BadHookSigner`, `WrongMint`, `UnsupportedOperation`,
   `MissingRewardVault`, `WrongRewardVault`, `DestinationNotAllowed`, `CreatorLocked`,
   `EarlyLocked`, `MaxWalletExceeded`, `NotKitCaller`, `InvalidModules`, `InvalidMaxWallet`,
   `InvalidCreatorLock`, `InvalidEarlyLock`, `SupplyOutOfBounds`, `WrongMintSetup`,
   `WrongReserve`, `BadRewardMint`, `AlreadyGraduated`, `NotAHolder`, `RewardsOff`,
   `WrongHolding`, `WrongRewardMint`, `WrongDestination`, `NothingToClaim`, `ShareTooSmall`,
   `NoEligibleHolders`, `WrongProgram`, `MathOverflow`.
2. Callback binding (§4.6): a config of another mint, or a prefix mint that is not the
   arguments', is `WrongMint`; with holder rewards an absent vault is `MissingRewardVault`; a vault
   at another address, any vault passed without holder rewards, or a vault account that is not a
   token-program holding is `WrongRewardVault`; a `Mint` operation or the `After` phase is
   `UnsupportedOperation`.
3. The reward mint (safer reading of "no hook program"): `init` also refuses a reward mint with a
   hook authority (a hook set later would break every claim and share, which pass no hook
   accounts) or a freeze authority (a frozen vault stops every claim): `BadRewardMint`. Bridged
   SOL has none of the three.
4. `init` refuses a max wallet that rounds to nothing (`supply * bps / 10_000 == 0`,
   `InvalidMaxWallet`): such a cap would refuse every buy until graduation. The parameters of a
   module that is off must be zero (`KitConfig` stores 0 when off).
5. `init` checks its arguments and accounts before the caller, and writes nothing before every
   check passes, so each refusal has its own error. The caller is compared with
   `create_program_address(["kit-caller", mint, [bump]], LAUNCH_ID)` at the bump it was given, and
   that bump is stored for `graduate`, which checks the caller before `AlreadyGraduated`.
6. `eligible` changes in every callback for every module set, as §4.5 says. Without holder rewards
   and without the early-buyer lock the mint has no `BEFORE_BURN` (§4.2), so a holder's burn never
   reaches the kit and `eligible` keeps counting the burned tokens. It can only overcount, so no
   subtraction from it can fail, and nothing reads it then: the launch's pool hook reads it only
   with holder rewards.
7. "Once `now >= early_unlock_at`, `early_locked` is written as 0 on each side it touches" applies
   to `before_burn` as well: the source of a burn is cleared too.
8. Hook data: the callbacks write bytes 32..64 as zero and answer only the holder sides whose
   bytes changed; the pool's and the launch's data is never written. `claim` writes bytes 0..24
   (the snapshot as 0 when the holding is empty) and keeps bytes 24..64 exactly as read.
9. The stream: a sync at a time not after `stream_last` releases nothing, and `stream_last` never
   moves back. `share` counts what reached the vault (measured; with a hook-less reward mint it is
   the amount) and restarts the stream: `stream_last = now`, `stream_end = now + 3_600`, what was
   still streaming spread over the new hour.
10. `claim` and `share` check `RewardsOff` as a constraint on `kit_config`, and take the reward
    vault as an address-checked account whose balance is read at its fixed offset (owner and
    discriminator checked), so a token without holder rewards gets `RewardsOff` rather than an
    account error. `claim`'s `mint` is address-checked only (the token program checks it again in
    `write_hook_data`). The pool and the launch get `NotAHolder`; a holding of another mint or of
    another owner (a delegate signing included) `WrongHolding`; a destination of another mint or
    owner `WrongDestination`.
11. The registry's two extras are fixed keys (`AccountSource::Key`), written by `init`.
12. `LAUNCH_ID`, `SWAP_ID`, `BRIDGE_ID`, the token program's hook and event authorities and the
    kit's own hook authority are constants, tested against the crates and their derivations. The
    wallets-only rule is `Pubkey::is_on_curve` (solana-address with the `curve25519` and `syscalls`
    features Anchor already enables): on chain the curve25519 validate-point syscall, measured at
    169 CU per call, 159 of them the syscall.
13. A mint that names the kit with flags other than §4.2's (for example none) is not a kit token
    (§4.15): no callback runs, so it moves without the kit ever seeing it. With the kit's flags and
    no `KitConfig` it never moves.
14. The off-chain mirror of §4.13 is `bordrless_kit::mirror` (written from the formulas, apart from
    the program's sync), the reference the TypeScript mirror is pinned to.

### Launchpad (§5)

1. Errors (`LaunchError`, appended after 6017 so earlier codes keep their numbers):
   `HolderFeeTooHigh` (6018), `BurnTooHigh`, `RulesFeeTooHigh`, `MaxWalletOutOfBounds`,
   `CreatorLockTooLong`, `InvalidEarlyLock`, `KitAccountsMissing`, `UnexpectedKitAccounts`,
   `WrongKitAccount`, `WrongHolderVault` (6027). A config outside a ceiling stays `InvalidConfig`.
2. The bounds of §5.2 are one struct, `RuleBounds`, kept as `Config.rule_bounds`, taken by
   `ConfigArgs.rule_bounds` and carried by `ConfigSet`. The ceilings also require
   `1 <= min_max_wallet_bps <= max_max_wallet_bps` (a cap must be something) and a launch supply of
   at most 1e16 base units as well as at least 1,000: the kit installs on no larger supply (§4.11),
   so a config above it would make every launch with kit rules fail.
3. `create_launch` checks the rules in this order: holder fee, burn, creator + holders + burn on
   buys then on sells, max wallet (off, or within the bounds, and not rounding to nothing at the
   supply, which the kit would refuse), creator lock, early-buyer lock. The parameters of a module
   that is off must be zero: an early-buyer unlock without a window is `InvalidEarlyLock`.
4. `CreateLaunch`'s six kit accounts (kit, kit config, kit registry, reward vault, kit caller, kit
   event authority) are Anchor optional accounts: all absent (this program's id) without kit
   modules, else `UnexpectedKitAccounts`; with them each present (`KitAccountsMissing`) and at its
   address (`WrongKitAccount`), the reward vault exactly with holder rewards. The kit registry is
   the kit's own to bind (its `init` checks the seeds, `ConstraintSeeds`). `Graduate`'s five kit
   accounts follow §5.5: required exactly for a launch with a kit, the reward vault slot holding the
   vault with holder rewards and the kit's id without.
5. `Launch` also keeps `kit_caller_bump` (0 without a kit), so `graduate` signs the kit's
   `graduate` and checks the kit-caller account with `create_program_address` at the stored bump
   instead of a search (§4.12). The kit is called with the kit-caller PDA only; the launch PDA and
   the hook authority are never among the accounts of a kit instruction.
6. `LaunchCreated` gains, after `launch_fee_lamports` and in this order, `rules`, `modules`,
   `kit_config: Option<Pubkey>` (with a kit), `holder_vault: Option<Pubkey>` (with holder
   rewards) and the absolute `creator_unlock_at`, `early_window_end` and `early_unlock_at` (0 when
   off), so the indexer reads them without deriving anything.
7. The pool hook binds the `Launch` by owner, discriminator, mint and pool (only `create_launch`
   creates one, at `["launch", mint]` with that mint), without a derivation. `holder_vault` and
   `kit_config` are address-checked (§5.4); the hook writes neither, so neither is `mut` in
   `HookCallback` (the DEX checks a delta target is writable). The kit config is read through
   `bordrless_kit::client::read_kit_config` only for a side whose holder rate is above zero.
8. The holder-fee threshold is `eligible >= min_eligible` exactly as §5.4 writes it (any kit has
   `min_eligible >= 1`, so it equals the kit's own `eligible > 0 && eligible >= min_eligible`). On a
   sell the hook reads the count in `after_swap`, after the sell's burn and transfer: the seller's
   tokens are already out of it.
9. "None unless below the amount" applies to the sell's burn too (§5.4 states it for the buy's);
   with the ceilings it can never bind, and it keeps both sides one function (`trade_burn`).
10. The creator's exemption applies to buys only, and `creator_bought` is set only when the
    exemption is used (inside the window, into the creator's own wallet).
11. The pool, its LP mint and vaults and the launch's holdings are bound by the programs that
    create them (each checks its seeds), so `create_launch` derives only what nobody else checks:
    the pool registry's bump, the kit config, the holder vault and the kit caller (and Anchor the
    `Launch`). `graduate` checks the vaults and the LP mint against the pool account (address-bound
    to the launch, owned by the DEX). Fixed addresses (configs, hook and event authorities) are
    constants, tested against their derivations. Instructions the launch invokes are built from
    keys it holds (`cpi.rs`).
12. The fee math is `bordrless_core` (`creator_and_holder_fees`, `trade_burn`,
    `launch_before_swap`, `launch_after_swap`, `max_wallet_cap`); the policy bounds of §5.2 are
    `bordrless_core::policy`, the ceilings `bordrless_launch::constants::ceilings`. The TypeScript
    mirror is pinned to them through `programs/tests/vectors/launch-fees.json`, which a Rust test
    renders from the reference (rewriting the file and failing when it is stale).

### Review fixes (after the program review of 2026-10-07)

Where these notes and the sections above differ, these notes win. Each corrects a rule that was
provably insecure or unfair as written; none weakens a rule.

1. **One hook signer per hook program** (corrects §1, §4.6, §4.10, §4.12, §5.3, §5.4 and §6's
   table). §4.6 had the kit trust a single token-program signer and §5.4 the launch a single DEX
   signer. A callback receives its caller's signer as a signer and may pass it on in a CPI of its
   own, and anyone can make a mint or a pool whose hook is their own program: that program could
   call the kit or the launch with the shared signer and arguments of its choosing (`eligible`
   zeroed or drained, a pool's counters run to `u64::MAX`, the creator's first-buy exemption
   spent). Now the token program signs every callback with `["hook-authority", hook_program]`
   under itself, and the DEX every pool callback with `["hook-authority", hook_program]` under
   itself. A signer a hook passes on is that hook's own, which no other hook accepts.
   - The kit accepts `C2Y3B3hZTesJQqLYrZ7qoaZUoRmwYWh5Qh3MuFxruouE` (the token program's for the
     kit), the launch's pool hook `6Ztfr97cUewdViXDXdZUsQq4pz7MYdygvK1WijALjZ5q` (the DEX's for the
     launch), `tax_hook` `8v2CVajpJMVKXLpZePXQpqvq2nyn7r1so7DxgXu4CkAw`: each a constant, so §4.12's
     "compare with a constant" holds and no callback derives anything. A signer per mint or per
     pool, as the review suggested, binds no more: only the hook a signer was made for accepts it,
     and only the caller can sign it. It would have needed a stored bump and a derivation in
     every callback.
   - The bump is kept by the caller: `Mint.hook_signer_bump` (set by `create_mint` and
     `set_hook`) and `Pool.hook_signer_bump` (set by `create_pool`), each taken from `reserved`,
     so neither account changes size.
   - Token `transfer`, `mint_to` and `burn`: `hook_signer` is an optional account, required
     exactly when the mint has a hook and checked against `["hook-authority", hook_program]` at
     the mint's bump, else `BadHookSigner` (6019, now used). The token program's id stands for
     none; a mint without a hook ignores the slot.
   - DEX: a mint's token-hook slice is the hook program, the token program's signer for it, then
     the hook's extras (a kit mint's: `[KIT_ID, 6EsV…, kit_config, vault or KIT_ID]`, so
     `base_hook_accounts` is 4). The fixed `token_hook_signer` account is gone from `swap`,
     `create_pool`, `add_liquidity`, `remove_liquidity`, `finalize_curve` and
     `collect_protocol_fees`; a slice too short leaves the token program to refuse
     (`HookProgramMissing`, `BadHookSigner`), which lets each side of a pool have its own hook. The
     pool hook's `hook_signer` is optional, required and checked when the pool has a hook, else
     `BadHookSigner` (6036, new); `create_pool` checks it whenever a hook is named, even one that
     creates its own pool.
   - Mints that never have a hook take no hook signer: the bridge's `wrap`, `unwrap`, `wrap_sol`,
     `unwrap_sol`, the kit's `claim` and `share`, the launch's `claim_creator_fees` lose their
     `token_hook_signer` account.
   - The launch's `create_launch` and `graduate` keep `token_hook_signer`, now the token program's
     signer for the kit (used for the kit mint's `mint_to`, top-up, burn and pool deposit; unused
     without a kit). `dex_hook_signer` is the DEX's signer for the launch.
   - §6's protocol lookup table holds the two signers above in place of the two old global ones.
   - Defence in depth: `tax_hook` also refuses arguments of another mint (`WrongMint`, 6005), and
     its `collected` and the launch's `creator_fees_accrued`, `holder_fees_accrued` and
     `burned_on_trades` saturate. They are running totals for display (`claim_creator_fees` pays
     the quote holding's balance), so a counter can never block a trade.
   - Tested in `programs/tests/tests/hook_signers.rs`: `hook_tester`, as a token hook and as a
     pool hook, passes its signer on to the kit, `tax_hook` and the launch with the review's forged
     arguments; each refuses with `BadHookSigner` and nothing changes. With the kit and the launch
     made to accept that signer (as they did the old shared one) the forged calls go through and
     the tests fail.
2. **The share stream pauses while nobody is eligible** (corrects §4.7 and §4.13). §4.7 released
   the stream while `eligible < min_eligible` into `held`, which the next eligible sync paid out at
   once, so a newcomer could buy past the threshold, claim the lump and sell in one transaction.
   This broke §4.10's "pro rata to what they hold as it is released". Now `release` returns 0 then
   and moves `stream_end` on by the time since `stream_last` (and `stream_last` to now): the
   stream resumes at the same rate with the same time left. §4.13's `released` is 0 while
   `eligible == 0` or `eligible < min_eligible`. Direct donations below the threshold still wait
   in `held` and divide at the next eligible sync, as §4.7 says (the site always uses `share`).
3. **A share joins the stream at its own rate** (corrects §4.10's `stream_end = now + 3_600` and
   the Kit note 9 above). A later share used to restart the stream, spreading what was still
   streaming over a new hour: anyone could delay any share indefinitely for 0.001 SOL a call.
   Now the stream releases at its rate plus the new share's (`amount / 3_600` per second) until it
   is empty. With `remaining` left over `left` seconds, the new end is `now + floor((remaining +
   amount) * left * 3_600 / (remaining * 3_600 + amount * left))`, between `left` and 3,600
   seconds; with nothing streaming, an hour. Taking what streamed first as released first, every
   share is out by the end of its own hour of eligible holding. The stream never releases faster
   than its shares together (the rate is the sum of theirs; the floor can end it up to a second
   early). The state is unchanged: one `stream_remaining`, `stream_last`, `stream_end`.
   `bordrless_kit::math::merged_duration` and `KitConfig::add_share` are the reference.
4. The TypeScript mirror and the rewards vectors follow notes 2 and 3: the `rewards` cases of
   `launch-fees.json` include a paused stream and a merged second share.
5. Tests only:
   - The money walk accepts a refusal only when the kit itself refused: the transaction's code,
     the kit program's own failure with it in the logs, and Anchor's log naming the error. It
     counts refusals by name.
   - `programs/tests/tests/fee_oracle.rs` holds the core's §5.4 fee functions, the reference the
     vectors are rendered from, and real launch-pool swaps against an oracle written from §3.1
     and §5.4 with none of the core's code.

### Review fixes, second round (after the review of fix1, 2026-10-07)

Where these notes and anything above differ, these notes win. Note 1 replaces Review fix 3 above
(which had replaced Kit note 9) and the merged-share part of Review fix 4. It strengthens the
rules; it weakens none.

1. **A share made while another streams waits for it, then streams its own hour** (corrects
   §4.3, §4.7, §4.10, §4.11, §4.13 and §7.3, and Review fix 3).
   - **What was wrong.** Review fix 3 merged a new share into the running stream and released the
     two at the sum of their rates until the stream was empty. The earlier share's rate should
     have stopped at its own end. Instead it carried on over the new share, so a share made as
     another ended was released in seconds:
     - 36 SOL shared, then 1 SOL 3,599 s later: the 1 SOL was out within 98 s, 37 times its own
       rate.
     - On a "Holders first" launch with three holders of 0.5 SOL, three wallets read the new
       `stream_end`, bought 0.5 SOL each a second after the share and sold at it. On chain they
       claimed 480,354,377 lamports and kept 398,413,162 after every fee.
     - Review fix 3's claim that the stream "never releases faster than its shares together" was
       false.
   - **Why two streams.** One linear stream cannot take a second share without either stretching
     the first (§4.10's restart, which let anyone delay any share for 0.001 SOL a call) or
     speeding up the second (fix 3). So the kit keeps two, in bounded state:
     - `stream_remaining`, `stream_last`, `stream_end`: the running stream, as before. A later
       share never changes them.
     - `stream_next: u64` (new): what was shared while the stream runs. It streams linearly over
       the hour after the running stream, `[stream_end, stream_end + 3,600]`. It sits after
       `created_at` and is taken from `reserved`, now `[u8; 56]`, so `KitConfig` stays 431 bytes
       and every other field keeps its offset.
   - **`share`, after its sync:**
     - Nothing running or waiting: the share streams over `[now, now + 3,600]`.
     - The running stream's whole hour still ahead (`stream_end - now == 3,600`, a share in the
       second that hour began): the share joins that stream. It is the same hour, so each share
       keeps its own rate.
     - Otherwise: the share is added to `stream_next`.
     - A stream not synced at the share's time is refused (`MathOverflow`, a bug). It cannot
       happen, since `share` syncs first, but a share queued behind a stream already over would
       otherwise be released at once.
   - **The sync (§4.7).** The running stream releases as before. Once it is out
     (`now >= stream_end`) and something waits, the waiting shares become the running stream over
     `[old stream_end, old stream_end + 3,600]`, and the same sync releases the part of that hour
     already past (holders held through it). While nobody is eligible (Review fix 2) nothing is
     released and `stream_end` moves on with the clock, taking the waiting shares' hour with it.
   - **What this guarantees.**
     - Every share streams linearly, at its own rate, over exactly one hour of eligible holding.
     - That hour starts when the share is made or, while an earlier share streams, when that one
       ends: within the hour.
     - Between any two moments the stream releases at most what the shares' own hours release
       between them; the floors only delay it.
     - A share can be delayed by at most an hour (by a share made just before it). No share can
       stretch or speed up another.
     - The same back-run now loses 75,877,760 lamports, and a sandwich held 98 s loses
       58,349,985. That is about what the same trade loses around a share streaming its hour
       alone (62,962,555).
   - **What the other sections now read:**
     - §4.11: the vault also covers `stream_next`.
     - §4.13: `released` is the running stream's release; once `now >= stream_end` and the
       running stream is out, it adds `stream_next` released linearly over
       `[stream_end, stream_end + 3,600]`. It is 0 while nobody is eligible. Still streaming =
       `stream_remaining + stream_next - released`.
     - The reference is `KitConfig::release`, `KitConfig::add_share` and
       `bordrless_kit::mirror::stream_at`. `math::merged_duration` is gone.
     - §4.10's "reaches holders over the next hour" and §0.17's "streamed over an hour" hold in
       this sense.
   - **§7.3's share copy** becomes: "Shared SOL reaches holders over one hour, in proportion to
     what they hold. If an earlier share is still streaming, yours starts when it ends (at
     14:32)." The site reads the start from `KitConfig`. It is now when nothing runs or waits,
     or when `stream_end - now` is 3,600; otherwise it is `stream_end`.
2. **The TypeScript mirror and the rewards vectors** (Review fix 4) follow note 1.
   - Each case's `kit` in `launch-fees.json` gains `streamNext`, and `expected.streaming` counts
     it.
   - The cases include:
     - a share waiting for a running one;
     - the waiting share streaming the hour after it;
     - a running and a waiting share through a pause;
     - two shares in one second.
   - The TypeScript mirror adds `streamNext` to the kit state and ports `stream_at`.
3. **Tests.**
   - Kit unit tests:
     - `a_share_made_as_another_ends_keeps_its_own_rate`: the review's sequence, and the review's
       bound (at most the old tail plus 1 SOL·L/3,600 in the next L seconds), checked at chosen
       L and every second. fix1's merge, kept in the test, is shown to break the bound.
     - `every_share_streams_its_own_hour`: random shares checked against a schedule of hours
       written apart from the program, after every sync. It replaces the old bound, which counted
       every share since the stream was last empty and so let an inherited rate through.
     - `a_share_waits_for_the_running_stream_then_streams_its_own_hour` and
       `a_waiting_share_waits_through_a_pause`: exact states and releases.
     - `the_mirror_matches_the_sync`: the mirror against the program's sync on 20,000 random
       states.
   - On chain:
     - `a_share_made_as_another_ends_streams_its_own_hour`: the review's sequence.
     - `trading_around_a_share_loses_money`: a real launch, the back-run and the sandwich.
     - `a_share_streams_over_the_hour`: a second share waits, then both are claimed.
   - A probe build of the kit with fix1's `math.rs` reproduced the review's numbers above on
     chain, and all three of these failed against it.
