# Quote tokens: coins launched against SPL tokens

A coin may launch against one to six real SPL Token or Token-2022 mints instead of native SOL. Each is
its own DEX pool (a "leg") holding an equal share of the supply, coins only at the start, priced from
the same opening market cap in its token, never graduating. A coin is quoted in SOL or in its tokens,
never both. Native SOL launches are unchanged.

Deployed on mainnet on 20 Oct 2026: `bordrless_swap` in slot 459122832 (program data 662,200 bytes,
sha256 `567318c21044d62bb6eaade4993755b0d79f51b68a2457b075b2adea18a77104`) and `bordrless_launch` in
slot 459123096 (941,472 bytes, sha256 `5c5fd98ce1ca296c683f5b1e660df5720b4b1705e8ed478eb9c9a4dd025acc79`),
each extended first (+93,280 and +149,376 bytes).

Rules on token-quoted coins, phase 1 (§2.5), changed the launchpad and the kit (the DEX did not) and
were deployed on 21 Oct 2026: the kit in slot 459491916 (sha256
`7302050b4989070a395bab787cbdedf77c414d0d996a0cb353b39a7cf0a66277`, program data 373,888 bytes), then
the launchpad in slot 459492381 (sha256
`8fe9c810f4911d3ba179fa3976da0559a46ff92aa959ef1dbb930e8c3f797cf7`, 969,584 bytes).

Workshop items and the presets on token-quoted coins (§2.6, "builder decides"): a script companion,
a Workshop hook from a launch config, and the launch form's presets (a buyback, the launcher's part,
a split of the two, a Snowball) on a token-quoted coin, every payout in the legs' own tokens. Built on
top of the programs above and the deployed companion (its first 1,317,152 bytes of program data,
sha256 `e66bca965cbb377cdb99ae6d2a824514cda2949889702da26d36e98f6a730829`, are the build of 7fde540,
the rest zero); they change the launchpad and the companion (the DEX and the kit do not) and are not
deployed yet.

## 1. What a token-quoted coin is

- The whole supply is split evenly into `n` legs (1 ≤ n ≤ 6, `legShares`; the last leg takes the
  remainder). Leg `i` is a DEX curve pool `["pool", mint, quote_i, lp_fee_bps, LAUNCHPAD]` created by
  the launchpad as its own hook, holding `share_i` coins and **no quote**, with `virtual_base = 0` and
  `virtual_quote = legVirtualQuote(share_i, quoteUsd_i, decimals_i)`: the coin opens at the same
  $5,000 fully diluted (`OPENING_MCAP_USD`) in every token. On our constant-product curve a pool with
  a virtual quote and no virtual base is exactly a concentrated-liquidity position from the opening
  price to infinity (a single-sided range, as a Meteora DAMM v2 position from that price to the maximum): its price only rises from the opening as coins are bought
  (×4 at half the share sold, ×100 at 90%), and falls back toward it as they are sold.
- Legs never graduate, never finalize and never take liquidity; their LP is never minted. The LP fee
  goes to Bordrless (the DEX's share model for launchpad curves) in the leg's token.
- The **creator fee** (the launch's one fee rule here) is paid on the quote side of every trade, in
  the leg's token, into a fee account the launch owns per leg; the creator claims it per leg (or
  anyone pushes it to the creator's token account).
- **Rules (phase 1, §2.5; deployed 21 Oct 2026):** the burn on buys and sells, max wallet, the
  creator wallet lock and the early-buyer lock, with `create_launch`'s `LaunchRules` and bounds. The
  burn is every leg's pool hook's; the other three are the kit's, which treats every leg's pool as the
  launch's pool, and max wallet lifts once the legs together have sold what would have graduated a
  SOL coin.
- **Workshop items and the presets (§2.6; built, not deployed yet):** a script companion as the
  coin's creator, its whole creator fee its pot, every leg's fee in that leg's token in the
  companion's token accounts, spent as its script decides; a launch config whose custom hook (a
  Workshop or Studio hook, held to `create_launch`'s rule on who may upgrade it) becomes the coin's
  token hook, learning the legs from its registry; and the presets (a buyback, the launcher's part, a
  split of the two, a Snowball), their parts kept per leg in that leg's token: a buyback buys on the
  leg and burns, the launcher is paid in the leg's token, a Snowball trades every leg with its own
  pot.
- **Not available on a token-quoted coin:** a strategy game, a config's SOL cut, author's share or
  partner, the first-buy vest. (Holder rewards and payees came with §2.7, the games and Lossback
  with §2.8.)
  Without kit rules or a custom hook the coin's mint has no hook: it moves freely.
- Trading opens when **every** leg is open (the hook refuses swaps until then), so no leg trades
  before the others exist.
- Arbitrage keeps the legs' prices in line.

## 2. On chain

### 2.1 DEX: a third quote kind

- `Pool.reserved[0]` is the quote kind: 0 a Bordrless-standard token (today), 1 native SOL (today),
  **2 an SPL Token or Token-2022 mint**. `reserved[17]` is that mint's program (0 SPL Token, 1
  Token-2022). Every existing path keeps refusing a kind-2 pool (it checks `== 1` for native or reads
  the quote as a Bordrless holding).
- The quote vault is the pool PDA's associated token account of the quote mint under its program.
- **`create_spl_pool`**: a curve only (`virtual_quote > 0`, `quote_amount = 0`), created by its hook
  (`by_hook`), the quote mint checked against the extension policy (§2.3). Same args as
  `create_pool`; the quote side's hook slice is empty.
- **`swap_spl`**: `swap`'s core (`run_swap`) with an SPL leg beside the native one: the quote moves
  by `transfer_checked` through its program (the trader signs on a buy, the pool PDA on a sell); the
  trader's quote account is any token account of that mint (the trader's own on a buy). Cuts on the
  quote side are token accounts of the quote mint (owner = its program, mint checked, writable, not
  forbidden), paid the same way. Invariant after every instruction: the vault holds at least
  `quote_reserve + protocol_fees_quote` (no transfer fee is allowed, so it is exact unless someone
  donates).
- **`collect_protocol_fees_spl`**: permissionless; moves `protocol_fees_quote` to the treasury's
  associated token account of the quote mint (created idempotently, the caller paying its rent).
- **Refused on kind 2**: `finalize_curve`, `add_liquidity`, `remove_liquidity`, the native migration
  family, `collect_protocol_fees(_sol)`, the classic `swap` and `swap_v2`.
- **New rule for every pool**: a pool naming the launchpad as its hook is created only by the
  launchpad (`by_hook`). Without it anyone could create a leg's address before its `open_leg` runs
  (docs/launch-reserve.md §2.7 raised the same squat for the reserve pool).

### 2.2 Launchpad: legs

- `Launch.reserved[18] = 2` marks a token-quoted launch (the native flag `[17]` stays 0).
  `Launch.pool` and `Launch.quote_mint` name leg 0.
- A new account **`Legs`** at `["legs", mint]`: count, opened, and per leg its quote mint, program,
  decimals, virtual quote, share, pool, fee account, creator fees accrued and claimed, opened.
- **`create_token_launch`**: makes the mint and the launch as `create_launch` does for a launch with
  no custom hook and no companion (the creator a wallet: a companion creator is refused), with the
  kit only when its rules install it (phase 1, §2.5),
  the whole supply into the launch's holding, the `Legs` account from the quote mints (remaining
  accounts, 1 to 6, each SPL Token or Token-2022, none wrapped SOL, bridged SOL, a Bordrless mint or
  repeated) and their virtual quotes (each at least `MIN_LEG_VIRTUAL_QUOTE`), the creator fee within
  the config's bounds, the launch fee to the treasury as today.
- **`open_leg(index)`**: permissionless (the payer pays rent): the leg's fee account (a token
  account at a launchpad PDA `["leg-fees", mint, quote_mint]` owned by the launch PDA) and the DEX
  `create_spl_pool` call as the hook. The last leg opened lets trading start.
- **Hook callbacks**: on a kind-2 launch, `before_swap`/`after_swap` accept a pool whose address is
  `["pool", mint, quote_mint, lp_fee_bps, LAUNCHPAD]` for a quote in `Legs`, refuse until every leg
  is open, answer the creator fee as a quote-side delta to that leg's fee account, and count it in
  `Legs`. No holder fee, no sniper window, no creator first-buy rule.
- **`claim_creator_fees_token(index)`**: anyone; the leg's fee account to the creator's associated
  token account of that quote (created idempotently by the caller).
- **Refused on kind 2**: `graduate`, the bridged and SOL creator claims, every companion path, the
  kit's holder-reward paths (no holder rewards yet; the kit's other rules are phase 1, §2.5).
- Events: `TokenLaunchCreated`, `LegOpened`, `LegCreatorFeesClaimed` (the DEX's `PoolCreated` and
  `Swapped` describe the pools and trades as for any pool).

The exact account orders, args and events are written below by the programs build (§2.4) and are
what the SDK mirrors.

### 2.3 The quote mint policy (DEX, at `create_spl_pool`; mirrored in `TOKEN_2022_MINT_EXTENSIONS`)

An SPL Token mint is always fine. A Token-2022 mint's extensions: allowed MintCloseAuthority,
ConfidentialTransferMint, InterestBearingConfig, PermanentDelegate, MetadataPointer, TokenMetadata,
GroupPointer, TokenGroup, GroupMemberPointer, TokenGroupMember, ScaledUiAmount; DefaultAccountState
only when `Initialized`; TransferHook only with no program set; Pausable only when not paused;
refused TransferFeeConfig, NonTransferable, ConfidentialTransferFeeConfig, ConfidentialMintBurn and
any other number. The issuer's powers (freeze, permanent delegate, pause, a hook added later, closing
the mint, scaled amounts) are risks for clients to label, not refusals: xStocks carry all of them.

### 2.4 Interface

Built on `7fde540`. Rust builders: `bordrless_swap::client::{create_spl_pool, swap_spl, collect_protocol_fees_spl,
spl_vault_address}`, `bordrless_launch::client::{create_token_launch, open_leg,
claim_creator_fees_token, leg_swap, leg_swap_with, leg_hook_extras, legs_address,
leg_fees_address, LegKeys}`; the SPL helpers in `bordrless_swap::spl` (policy, reads, ATA,
`TransferChecked`). IDLs: `idl/bordrless_swap.json`, `idl/bordrless_launch.json`. Discriminators
are Anchor's: `sha256("global:<name>")[..8]`, `sha256("account:<Name>")[..8]`,
`sha256("event:<Name>")[..8]`; events are emitted by self-CPI (`EVENT_IX_TAG_LE` ‖ discriminator ‖
Borsh). Every account list below ends with the program's event authority (`["__event_authority"]`)
and the program itself, as `#[event_cpi]` does. s = signer, w = writable, r = read-only.

#### DEX (`GyzKSnnEu2uN5bBRecE4XYY2enbfR2D2MtxnbJPGy7hk`)

**`create_spl_pool(args: CreatePoolArgs)`** — `68 1e c7 39 5a 32 80 d8`. A hook creates its own
curve quoted in an SPL Token or Token-2022 mint (the launchpad calls it from `open_leg`).

| # | account | | |
| --- | --- | --- | --- |
| 0 | payer | s w | rent of the pool (411 B), its two vaults; the DEX's `pool_creation_fee_lamports` |
| 1 | authority | s | owns `authority_base` (the launch PDA for a leg) |
| 2 | config | w | `2XLvgczuVACmvvjfRLLAcLwMzFHutbAAro61zZKP8fsx` |
| 3 | treasury | w | `config.treasury` |
| 4 | base_mint | r | a Bordrless mint |
| 5 | quote_mint | r | SPL Token or Token-2022 mint (§2.3) |
| 6 | pool | w | `["pool", base, quote, lp_fee_bps u16 LE, args.hook_program]` (created) |
| 7 | base_vault | w | `holding(base_mint, pool)` (created) |
| 8 | quote_vault | w | `ATA(pool, quote_mint, quote program)` (created idempotently) |
| 9 | authority_base | w | `holding(base_mint, authority)` |
| 10 | hook_caller | s | `["hook-authority"]` under `args.hook_program` |
| 11 | token_program | r | Bordrless token `2XoEWp8cF3kRXg74eVwPAyTFhVCAztn3V88komxAvr22` |
| 12 | token_event_authority | r | its event authority |
| 13 | quote_token_program | r | the quote mint's owner |
| 14 | associated_token_program | r | `ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL` |
| 15 | system_program | r | |
| 16, 17 | event_authority, program | r | |
| 18.. | the base mint's token-hook slice | | exactly `args.base_hook_accounts` (none for a leg) |

`CreatePoolArgs` (Borsh, unchanged): `lp_fee_bps u16, hook_program Pubkey, hook_flags u16,
virtual_base u64, virtual_quote u64, base_amount u64, quote_amount u64, base_hook_accounts u8,
quote_hook_accounts u8, hook_data Vec<u8>` (u32 length, at most 256). Rules: not paused;
`virtual_quote > 0`, `quote_amount == 0`, `quote_hook_accounts == 0` (else `SplPoolCurveOnly`);
`base_amount > 0`; `hook_caller` is `args.hook_program`'s hook authority (else `BadHookCaller`);
`quote_token_program` is the mint's owner (`WrongTokenProgram`); the mint passes §2.3 (the
`QuoteMint*` codes). Fee model: the share model when the hook is the launchpad, the flat rate
otherwise. No initialize callback, no LP mint. Emits `PoolCreated` (`lp_mint` the default key,
`quote_reserve 0`, `lp_supply 0`, `lp_minted 0`, `curve true`), then `SplPoolCreated`.

**`swap_spl(args: SwapArgs)`** — `17 5e f7 10 b2 9f 9a 1c`.

| # | account | | |
| --- | --- | --- | --- |
| 0 | trader | s | owner or delegate of the input account |
| 1 | config | r | |
| 2 | pool | w | a kind-2 pool (`NotSplPool` otherwise) |
| 3 | base_mint | r | (w only for a hook's base burn; never for a leg) |
| 4 | quote_mint | r | |
| 5 | base_vault | w | `pool.base_vault` |
| 6 | quote_vault | w | `pool.quote_vault` |
| 7 | trader_base | w | the coin holding: the trader's on a sell, any to deliver to on a buy |
| 8 | trader_quote | w | a token account of the quote: the trader's on a buy; on a sell any to deliver to but the pool's vault or another account the pool owns (`WrongTokenAccount`) |
| 9 | hook_program | r | the pool's hook (the DEX's id for none) |
| 10 | hook_signer | r | the DEX's `["hook-authority", hook_program]` (the DEX's id for none) |
| 11 | token_program | r | Bordrless token |
| 12 | token_event_authority | r | |
| 13 | quote_token_program | r | `pool.reserved[17]`'s program (`WrongTokenProgram`) |
| 14, 15 | event_authority, program | r | |
| 16.. | in-side token-hook slice, out-side slice, the pool hook's extras | | the quote side's slice is empty (`AccountCounts`) |

`SwapArgs` (unchanged): `direction u8 (0 sell, 1 buy), amount_in u64, min_amount_out u64,
in_hook_accounts u8, out_hook_accounts u8, hook_data Vec<u8>`. A leg swap: both hook counts 0,
the four leg extras (below) as the pool hook's: 20 accounts. The quote moves by `TransferChecked`
(the trader signs a buy's input and its quote-side cuts; the pool signs a sell's output and its
cuts); quote-side cuts go to writable token accounts of the quote mint under its program
(`InvalidDeltaAccount` otherwise; never the vaults or the trader's accounts); a quote-side burn
is `SplBurn`. A sell may not deliver into the pool's own quote vault or any token account the pool
owns (`WrongTokenAccount`: the token programs take a transfer to the same account as a no-op, which
would strand the seller's quote outside the books). Ends with the invariant `quote vault amount >=
quote_reserve + protocol_fees_quote` (`SplInvariant`). Emits `Swapped` (unchanged; amounts in raw quote units).

**`collect_protocol_fees_spl()`** — `26 af 34 be 4c 28 73 61`. Anyone; no args.

| # | account | | |
| --- | --- | --- | --- |
| 0 | payer | s w | pays the collector's token account if created |
| 1 | config | r | |
| 2 | pool | w | kind 2 |
| 3 | quote_mint | r | `pool.quote_mint` |
| 4 | quote_vault | w | `pool.quote_vault` |
| 5 | fee_collector | r | `config.fee_collector` (as `collect_protocol_fees_sol` pays) |
| 6 | collector_quote | w | `ATA(fee_collector, quote_mint, quote program)` (created idempotently) |
| 7 | quote_token_program | r | the pool's |
| 8 | associated_token_program | r | |
| 9 | system_program | r | |
| 10, 11 | event_authority, program | r | |

Emits `ProtocolFeesCollected { pool, quote_amount, collector: fee_collector, ts }`.

**Refused on kind 2**: `swap` and `finalize_curve`, `add_liquidity`, `remove_liquidity`,
`collect_protocol_fees` (a constraint on `pool`: `SplPool`); `swap_v2` (`NotNativePool`);
`collect_protocol_fees_sol`, `migrate_pool_to_native`, `revert_pool_to_bridged`, `rebalance_pool`,
`buffer_all`, `set_buffer_target` (`NotBridgedSol`). **Every pool**: `create_pool` /
`create_native_pool` naming the launchpad as hook without its hook authority signing is refused
(`LaunchpadPoolNeedsLaunchpad`; checked after the `before_initialize` callback, so a pool that
asks the launchpad keeps getting `OnlyLaunchpadCreatesPools`).

**Kind-2 `Pool`** (411 bytes, layout unchanged; a kind-2 pool always has a hook, so `reserved`
starts at byte 351): `reserved[0]` (byte 351) `= 2`; `reserved[17]` (368) the program, 0 SPL
Token, 1 Token-2022; `reserved[18]` (369) the quote's decimals; `reserved[1..17]` and `[19..60]`
zero. `lp_mint` (75..107) the default key, `lp_mint_bump 0`, `lp_supply 0`; `quote_vault`
(139..171) the pool's ATA of the quote; `curve` (258) true for ever; `fee_model` (348) 1 and
`protocol_share_bps` the config's launch share for a leg (the launchpad's curve), else 0 and the
flat rate; `creator` the depositing authority (a leg: the launch PDA).

#### Launchpad (`1jcBymHxBjniZDhNPy51Vgm5Nz7pLUdxa9UBHc4TavC`)

**`create_token_launch(args: CreateTokenLaunchArgs)`** — `5d 57 3a 7e 58 4b ac e9`.

| # | account | | |
| --- | --- | --- | --- |
| 0 | creator | s w | a wallet (a companion's creator PDA is `CompanionTokenLaunch`); pays the launch fee and rent |
| 1 | config | w | `5n25iAaXFsjs4UgGQM6fCiRhaQcCVQ1L5BgyRZ7UrmaE` |
| 2 | treasury | w | `config.treasury` (receives `launch_fee_lamports`) |
| 3 | mint | s w | the new coin (a fresh keypair) |
| 4 | launch | w | `["launch", mint]` (created) |
| 5 | legs | w | `["legs", mint]` (created) |
| 6 | launch_reserve | w | `holding(mint, launch)` (created; receives the whole supply) |
| 7 | token_program | r | Bordrless token |
| 8 | token_event_authority | r | |
| 9 | system_program | r | |
| 10, 11 | event_authority, program | r | |
| 12.. | the quote mints, 1 to 6, in leg order | r | |

`CreateTokenLaunchArgs` (Borsh): `name String, symbol String, uri String, creator_fee_bps u16,
virtual_quotes Vec<u64>` (one per quote mint, same order). Checks: not paused; name 1–32 bytes,
symbol 1–10, uri ≤ 200 (`InvalidMetadata`); `creator_fee_bps ≤ max_creator_fee_bps`
(`CreatorFeeTooHigh`) and within `max_rules_fee_bps`; 1–6 mints and as many virtual quotes
(`InvalidLegCount`); none wrapped SOL (`So111…112`, Token-2022's `9pan9b…NdXejP`:
`SolQuoteToken`); each passes §2.3 (`QuoteMintInvalid` for a non-SPL/T22 account such as a
Bordrless mint, then the `QuoteMint*` codes); none twice (`DuplicateQuoteToken`); each
`virtual_quote ≥ 1,000` (`LegVirtualQuoteTooSmall`). Shares: `supply / n` each, the last leg
`supply − (n−1)·⌊supply/n⌋` (`legShares`). The mint: the config's decimals and supply, no hook, the
launch its mint authority then none. Emits `TokenLaunchCreated` (no `LaunchCreated`).

**`open_leg(index: u8)`** — `e7 30 37 b6 35 0f 51 f7`. Anyone.

| # | account | | |
| --- | --- | --- | --- |
| 0 | payer | s w | rent of the fee account, the pool, its vaults, its registry (and any DEX creation fee) |
| 1 | launch | r | `["launch", mint]`, quoted in tokens (`NotTokenLaunch`) |
| 2 | legs | w | `["legs", mint]` |
| 3 | mint | r | `launch.mint` |
| 4 | quote_mint | r | `legs[index].quote_mint` |
| 5 | leg_fees | w | `["leg-fees", mint, quote_mint]` (created: a token account of the quote, owner the launch) |
| 6 | launch_reserve | w | `launch.reserve_holding` |
| 7 | registry | w | `["bordrless-hook-accounts", pool]` under the launchpad (written) |
| 8 | dex_config | w | the DEX config |
| 9 | dex_treasury | w | the DEX config's treasury |
| 10 | pool | w | `legs[index].pool` (created by the DEX) |
| 11 | base_vault | w | `holding(mint, pool)` |
| 12 | quote_vault | w | `ATA(pool, quote_mint, quote program)` |
| 13 | hook_signer | r | the launchpad's `["hook-authority"]` `3dfEZLdjRcpTJ4RxPzgkqnqaG2FipRdaHgRW72AL6kqL` |
| 14 | dex_event_authority | r | |
| 15 | swap_program | r | the DEX |
| 16 | token_program | r | Bordrless token |
| 17 | token_event_authority | r | |
| 18 | quote_token_program | r | the leg's program |
| 19 | associated_token_program | r | |
| 20 | system_program | r | |
| 21, 22 | event_authority, program | r | |

Refusals: `NoSuchLeg` (index ≥ count), `LegAlreadyOpen`, `WrongLegAccount` (quote mint, pool,
fee account or program not the leg's); the DEX's own checks again (a quote mint paused or given a
hook program since the launch: `QuoteMintPaused`, `QuoteMintTransferHook`, …). An address funded
or created by someone first (the fee account, the pool, its vaults, its registry) does not block
the leg. The DEX call: `create_spl_pool` with `lp_fee_bps = launch.lp_fee_bps`, `hook_program =
launchpad`, `hook_flags = LEG_HOOK_FLAGS = 0x3c1` (the launch flags without
`BEFORE_SWAP_OVERRIDES_FEE`), `virtual_base 0`, the leg's `virtual_quote`, `base_amount` the
leg's share. Emits `LegOpened`; the last leg opened starts trading.

**`claim_creator_fees_token(index: u8)`** — `69 82 f9 8d 0d 9a 75 04`. Anyone.

| # | account | | |
| --- | --- | --- | --- |
| 0 | payer | s w | pays the creator's token account if created |
| 1 | launch | r | quoted in tokens |
| 2 | legs | w | |
| 3 | quote_mint | r | the leg's |
| 4 | leg_fees | w | the leg's fee account |
| 5 | creator | r | `launch.creator` (`NotCreator`) |
| 6 | creator_quote | w | `ATA(creator, quote_mint, quote program)` (created idempotently) |
| 7 | quote_token_program | r | the leg's |
| 8 | associated_token_program | r | |
| 9 | system_program | r | |
| 10, 11 | event_authority, program | r | |

Pays the fee account's whole balance (`NothingToClaim` at 0 or before the leg opens) and adds it
to the leg's `creator_fees_claimed` (a display counter, saturating: it never blocks a claim). Emits
`LegCreatorFeesClaimed`.

**Leg pool registry** (`["bordrless-hook-accounts", pool]` under the launchpad), what a client
appends to a leg's swaps (callback indices 5 to 8; the prefix is the DEX's hook signer, the pool,
the coin, the quote mint, the trader):

| index | account | | source |
| --- | --- | --- | --- |
| 5 | the launch | w | PDA(launchpad, `["launch", Account(2)]`) |
| 6 | the leg's fee account | w | PDA(launchpad, `["leg-fees", Account(2), Account(3)]`) |
| 7 | the `Legs` | w | PDA(launchpad, `["legs", Account(2)]`) |
| 8 | the kit config | r | Key(`PDA(["kit", mint], KIT)`, `launch.kit_config`) |

The callbacks on a leg: the pool must be one of `Legs` (by its quote mint and pool) with index 6
its fee account (`WrongPool`, `WrongLegAccount`); index 7 must be the coin's writable `Legs`
(`LegsAccountInvalid`); every leg open (`LegsNotOpen`); the answer is the creator fee alone,
`ceil(amount × creator_fee_bps / 10,000)` of a buy's quote input (`before_swap`) or a sell's quote
output (`after_swap`), as a delta to index 6, added to the leg's `creator_fees_accrued`; no LP fee
override, no holder fee, no burn, no sniper window. The DEX's share model then takes the LP fee
and `launch_protocol_share_bps` of the creator fee for Bordrless (`protocol_fees_quote`).

**`Launch` quoted in tokens** (layout unchanged): `reserved[18]` (byte 529: `custom_hook` is
always none) `= 2`, `reserved[17]` 0; `pool`/`quote_mint` leg 0's; `quote_holding` leg 0's fee
account; `status 0` for ever; `graduation_quote u64::MAX`; `curve_tokens` the supply;
`virtual_quote`, `virtual_base`, `reserve_tokens`, `sniper_*` 0; `lp_holding`, `holder_vault`,
`config` the default key; `rules` none, `modules 0`; `kit_config` derived as for any launch;
`creator_fees_accrued`/`claimed` stay 0 (each leg counts its own, in its own token).

**`Legs`** — `["legs", mint]`, account discriminator `16 be 5c dd c3 3b ab bd`, 974 bytes:

| bytes | field | |
| --- | --- | --- |
| 0..8 | discriminator | |
| 8 | version | u8 (1) |
| 9 | bump | u8 |
| 10..42 | mint | Pubkey |
| 42..74 | launch | Pubkey |
| 74 | count | u8, 1–6 |
| 75 | opened | u8 |
| 76 + 139·i | `legs[i]`, i < 6 (unused legs zero) | `Leg`, 139 bytes |
| 910..974 | reserved | [u8; 64] |

`Leg` (offsets within the leg): `quote_mint` 0..32, `quote_program` 32 (u8, 0 SPL Token, 1
Token-2022), `decimals` 33, `fee_bump` 34, `virtual_quote` 35..43 (u64), `share` 43..51,
`pool` 51..83, `fee_account` 83..115, `creator_fees_accrued` 115..123, `creator_fees_claimed`
123..131, `opened_at` 131..139 (i64, 0 until open).

**Events** (Borsh after the discriminator):

- `SplPoolCreated` (DEX) — `f5 d8 90 b3 83 42 6d 8b`: `pool, quote_mint, quote_program: Pubkey,
  quote_decimals: u8, quote_vault: Pubkey`. Right after the pool's `PoolCreated`
  (`ca 2c 29 58 68 dc 9d 52`).
- `TokenLaunchCreated` — `c8 b1 d9 52 7f 5d 62 d8`: `launch, mint, creator, legs_account: Pubkey,
  name, symbol, uri: String, supply: u64, decimals: u8, creator_fee_bps, lp_fee_bps: u16,
  launch_fee_lamports: u64, legs: Vec<LegTerms>, slot: u64, ts: i64`; `LegTerms`: `quote_mint,
  quote_program: Pubkey, decimals: u8, virtual_quote, share: u64, pool, fee_account: Pubkey`.
- `LegOpened` — `fa a1 6c 14 5c 8d 1d dc`: `launch, mint: Pubkey, index: u8, quote_mint, pool,
  fee_account: Pubkey, virtual_quote, share: u64, opened, count: u8, payer: Pubkey, slot: u64,
  ts: i64`.
- `LegCreatorFeesClaimed` — `17 85 8f 43 aa 26 10 cb`: `launch, mint: Pubkey, index: u8,
  quote_mint, creator, destination: Pubkey, amount, claimed_total, slot: u64, ts: i64`.
- Trades: the DEX's `Swapped` (`d9 34 34 53 93 87 60 6d`, unchanged) per leg; fee sweeps
  `ProtocolFeesCollected` (`a5 22 7d 9b 0f 56 63 bf`).

**Error codes** (appended; every earlier code keeps its number):

DEX: 6052 `SplPool`, 6053 `NotSplPool`, 6054 `SplPoolCurveOnly`, 6055 `QuoteMintInvalid`,
6056 `QuoteMintTransferFee`, 6057 `QuoteMintNonTransferable`, 6058 `QuoteMintFrozenByDefault`,
6059 `QuoteMintTransferHook`, 6060 `QuoteMintPaused`, 6061 `QuoteMintConfidential`, 6062
`QuoteMintUnknownExtension`, 6063 `WrongTokenProgram`, 6064 `WrongTokenAccount`, 6065 `SplBurn`,
6066 `SplInvariant`, 6067 `LaunchpadPoolNeedsLaunchpad`. Launchpad: 6062 `TokenQuotedLaunch`, 6063
`NotTokenLaunch`, 6064 `InvalidLegCount`, 6065 `QuoteMintInvalid`, 6066 `QuoteMintTransferFee`, 6067
`QuoteMintNonTransferable`, 6068 `QuoteMintFrozenByDefault`, 6069 `QuoteMintTransferHook`, 6070
`QuoteMintPaused`, 6071 `QuoteMintConfidential`, 6072 `QuoteMintUnknownExtension`, 6073
`SolQuoteToken`, 6074 `DuplicateQuoteToken`, 6075 `LegVirtualQuoteTooSmall`, 6076
`CompanionTokenLaunch`, 6077 `NoSuchLeg`, 6078 `LegAlreadyOpen`, 6079 `LegsNotOpen`, 6080
`WrongLegAccount`, 6081 `LegsAccountInvalid`. The two programs number independently (a refusal
inside `open_leg`'s DEX call carries the DEX's code). The policy codes map one to one onto
`quotes.ts` `QuoteTokenRefusal`: `transfer_fee` (also for type 16), `non_transferable`,
`frozen_by_default`, `transfer_hook`, `paused`, `confidential` (type 24), `unknown_extension`,
`not_a_mint`/`bordrless_token` (`QuoteMintInvalid`), `sol` (`SolQuoteToken`), `duplicate`.

**Measured** (LiteSVM 0.17 with the SPL Token, Token-2022 11.0 and ATA 1.1.1 programs it bundles; mainnet's
token programs may cost slightly differently; compute units as consumed):

| instruction | compute units | accounts | transaction |
| --- | --- | --- | --- |
| `swap_spl` buy on a leg, SPL Token | 65,549 | 20 | 838 B |
| `swap_spl` sell on a leg, SPL Token | 65,643 | 20 | 838 B |
| `swap_spl` buy on a leg, Token-2022 (xStocks-like) | 72,532 | 20 | 838 B |
| `swap_spl` sell on a leg, Token-2022 (xStocks-like) | 72,626 | 20 | 838 B |
| the same buy with the buyer's `create_holding` in front | 78.6k–81.6k (SPL), 85.6k–88.6k (T22) | | 888 B |
| `create_token_launch`, 1 leg | 93.7k–104.2k | 13 | 750 B |
| `create_token_launch`, 6 legs | 134.2k–155.2k | 18 | 956 B (a 64-byte URI; a 200-byte one about 1,092 B) |
| `open_leg`, SPL Token | 118.0k–130.0k | 23 | 915 B |
| `open_leg`, Token-2022 (xStocks-like) | 129.6k–141.6k | 23 | 915 B |
| `claim_creator_fees_token`, creating the creator's ATA | 34.7k–37.7k (SPL), 43.6k–46.6k (T22) | 12 | 552 B |
| `collect_protocol_fees_spl`, creating the collector's ATA | 29.2k–35.2k (SPL), 38.2k–45.7k (T22) | 12 | 551 B |

The swaps are exact (no address search runs in them); the others vary by a few thousand units
from run to run with the bump searches of fresh addresses (`create_token_launch` derives each
leg's pool and fee account; the ATA program searches its address). Transaction sizes include the
fee payer's signature and a compute-budget instruction; all fit one legacy-size packet without
a lookup table. Rent at opening, per leg: the fee account (165 B under SPL Token; Token-2022 sizes it for the
mint), the pool (411 B), its base holding, its quote ATA, its registry; the `Legs` (974 B) and the
`Launch` at creation.

**Sizes** (`cargo build-sbf`, platform-tools v1.57, SBF v3): `bordrless_swap.so` 568,920 → 662,200 B (+93,280);
`bordrless_launch.so` 792,096 → 941,472 B (+149,376). The baseline equals today's mainnet
ProgramData exactly, so `solana program extend` must add at least 93,280 B to the DEX (about
0.649 SOL of rent) and 149,376 B to the launchpad (about 1.040 SOL) before the upgrades. Rebuilt
from the same tree, `hook_vault.so` (+3,352 B: the launchpad's new error strings),
`bordrless_router.so` and `loss_cover_script.so` (same size, shifted panic locations) differ
from their deployed bytes too; their behaviour does not change and they need no upgrade.

### 2.5 Rules (phase 1)

The burn on buys and sells, max wallet, the creator wallet lock and the early-buyer lock, on a
token-quoted coin as on a SOL coin, with the same `LaunchRules` and bounds. No holder fee yet
(holder rewards are phase 2). Changes the launchpad and the kit; the DEX is unchanged.

- **The burn** is every leg's pool hook's, on the coin's side of each trade: a sell's input in
  `before_swap`, a buy's output in `after_swap`, `⌊amount × burn_bps / 10,000⌋` (dropped when not
  below the amount), counted in `Launch.burned_on_trades`. The coin's mint must be passed writable
  to a burning leg's swaps (the DEX's `MintNotWritable` otherwise).
- **The kit** (max wallet, the creator wallet lock, the early-buyer lock) is installed by
  `create_token_launch` through the kit's new `init_legs`, as `create_launch` installs it through
  `init`: the mint names the kit as its hook from its first instruction, with the flags of its
  modules; no reward mint, no reward vault. Its config is marked quoted in tokens
  (`KitConfig.reserved[9] = 2`), and its callbacks take the coin's `Legs` after their two extras:
  every leg's pool is the launch's pool (excluded from holders and from `eligible`, never capped,
  a transfer out of any of them a buy for the early-buyer lock).
- **Max wallet lifts** where a SOL coin's does, at the point that would have graduated it: once the
  legs together have sold `curve_bps` of the supply. Each leg's `after_swap` records its coins sold
  (its share less its pool's base reserve) on the `Legs`; the first trade, on any leg, after the
  legs together had sold `lift_at` sets `Legs.reserved[8]` and logs `LegsMaxWalletLifted`; the kit
  reads that byte and lifts the cap (and sets `KitConfig.graduated`, as `graduate` does for a SOL
  coin) before that trade's output reaches the trader. The trade that crosses is still capped, like
  the buy that sells a SOL coin's curve out (its graduation follows it).
- **The locks run from when trading opens**, the last `open_leg`, as a SOL coin's run from its
  launch, which is when it opens: the creator wallet lock, the early-buyer window and its unlock. The
  launch stores them counted from `create_token_launch`; the kit moves each by `max(0, T −
  created_at)`, `T` the latest `opened_at` of the legs once every leg is open (read from the same
  `Legs`), so withholding the last leg never spends a lock before anyone can trade. The last
  `open_leg` says the times in force (`TokenLaunchLocks`).
- **Holder fees are refused** (`HolderFeeOnTokenLaunch`); every other bound is `create_launch`'s
  (`HolderFeeTooHigh`, `BurnTooHigh`, `RulesFeeTooHigh`, `MaxWalletOutOfBounds`, `CreatorLockTooLong`,
  `InvalidEarlyLock`).
- **Compatibility**: data that ends after `virtual_quotes` (every call made before rules) reads as
  no rules, with the accounts as before; a launch without rules creates, opens, trades and claims
  exactly as before; every `Launch`, `KitConfig` and `Legs` made before reads the new fields as zero
  (no rules, a SOL coin's kit, nothing counted, not lifted). SOL launches are unchanged byte for byte.

#### Interface (phase 1)

Rust builders: `bordrless_launch::client::{create_token_launch, token_launch_kit_accounts,
open_leg_with, leg_base_slice, leg_swap_with_rules, leg_swap_with_slice}` (`leg_swap` and `open_leg`
build a launch without rules, as before); `bordrless_kit::client::{init_legs, legs_hook_extras,
legs_hook_slice, legs_address}`; the times in force: `Legs::{trading_opened_at, lock_delay}` and
`bordrless_kit::rules::lock_times`. IDLs: `idl/bordrless_launch.json`, `idl/bordrless_kit.json`.

**`create_token_launch(args: CreateTokenLaunchArgs)`** — discriminator unchanged
(`5d 57 3a 7e 58 4b ac e9`). `CreateTokenLaunchArgs` gains a last field, `rules: LaunchRules`
(Borsh, 22 bytes: `holder_fee_buy_bps, holder_fee_sell_bps, burn_buy_bps, burn_sell_bps,
max_wallet_bps: u16, creator_lock_secs, early_window_secs, early_lock_secs: u32`). Data that ends
before it reads as `LaunchRules::NONE`; a part of it is `InstructionDidNotDeserialize`. Accounts
0–11 and the quote mints unchanged; when the rules install the kit (`max_wallet_bps`,
`creator_lock_secs` or `early_window_secs` above 0), six accounts follow the quote mints:

| # | account | | |
| --- | --- | --- | --- |
| 12+n | kit program | r | `14RJQXPdJfkehit6ezktjd3xujamf8nVSKw2shKamaEH` |
| 13+n | token hook signer | r | `["hook-authority", KIT]` under the token program, `C2Y3B3hZTesJQqLYrZ7qoaZUoRmwYWh5Qh3MuFxruouE` |
| 14+n | kit config | w | `["kit", mint]` under the kit (created) |
| 15+n | kit registry | w | `["bordrless-hook-accounts", mint]` under the kit (written) |
| 16+n | kit caller | r | `["kit-caller", mint]` under the launchpad (signs `init_legs`) |
| 17+n | kit event authority | r | |

(`n` quote mints.) `KitAccountsMissing` when they are missing, `WrongKitAccount` when one is not at its
address (the kit checks the registry's seeds). A burn alone needs no kit and takes none. The mint:
hook the kit, flags `0x01` (before transfer), `0x91` with the early-buyer lock (before transfer,
before burn, writes hook data). The `Launch`: `rules`, `modules`, `kit_caller_bump`,
`creator_unlock_at`, `early_window_end`, `early_unlock_at` (the launch time plus each duration, 0 when
off; they run from when trading opens, moved as the kit callbacks say); `holder_vault` the default
key. The `Legs`: `reserved[0..8]` = `lift_at` with max wallet.
Emits `TokenLaunchCreated` (unchanged), then, with any rule, `TokenLaunchRules`; the kit emits
`KitInstalled`.

**`open_leg(index)`** — with kit rules, three remaining accounts: the kit, the token hook signer, the
kit config (w) (`KitAccountsMissing`, `WrongKitAccount`). The deposit runs the kit: the DEX's
`create_spl_pool` gets `base_hook_accounts = 5`, the coin's slice `[kit, token hook signer, kit config
(w), kit id, Legs]`. The leg that opens last (trading opens), on a launch with the creator wallet lock
or the early-buyer lock, also emits `TokenLaunchLocks` with the times in force; no other launch does.

**Leg swaps** (`swap_spl`) with kit rules: the coin's slice on its side (`out_hook_accounts = 5` on a
buy, `in_hook_accounts = 5` on a sell): the kit, the token hook signer, the kit config (w), the kit's
id, the `Legs` (r), as the kit's registry for the mint resolves them; then the leg's four extras as
before. With a burn, the coin's mint writable. 25 account entries (22 keys).

**Kit `init_legs(args: KitInitArgs)`** — `78 68 5e 1c e3 48 ba d3`. Signed by the launchpad's
kit-caller PDA, inside `create_token_launch`.

| # | account | | |
| --- | --- | --- | --- |
| 0 | kit_caller | s | `["kit-caller", mint]` under the launchpad, at `args.kit_caller_bump` (`NotKitCaller`) |
| 1 | payer | s w | the creator |
| 2 | mint | r | hook the kit, no hook or mint authority, the whole supply minted (`WrongMintSetup`) |
| 3 | launch_reserve | r | the launch's holding of the whole supply (`WrongReserve`) |
| 4 | kit_config | w | `["kit", mint]` (created) |
| 5 | registry | w | `["bordrless-hook-accounts", mint]` (written) |
| 6 | system_program | r | |
| 7, 8 | event_authority, program | r | |

`KitInitArgs` unchanged; `pool` leg 0's; `reward_mint` the default key (`WrongRewardMint`); no holder
rewards (`RewardsOnTokenQuote`); every other bound as `init`'s. Emits `KitInstalled` (reward mint and
vault the default key). The registry for the mint lists three extras: the kit config (w,
`Key`), the kit's id (r, `Key`: no reward vault), the `Legs` (r, `PDA(launchpad, ["legs",
Account(1)])`, the mint being the callback's account 1).

**Kit callbacks** (`before_transfer`, `before_burn`) of a config quoted in tokens: the `Legs` is the
first account after the two extras (`LegsMissing` without it; `WrongLegs` unless it is owned by the
launchpad, has `Legs`' discriminator and size and names this mint). The lock times in force are the
config's moved by `max(0, T − created_at)`: `T` the latest `opened_at` of the legs once `opened ==
count`, the clock before that (nothing trades yet); a time of 0 stays 0; nothing is written back.
Every other kit reads its accounts exactly as before.

**Layouts** (all in reserved bytes; sizes unchanged):

| account | bytes | field |
| --- | --- | --- |
| `KitConfig` (431 B) | 385 (`reserved[9]`) | 2 for a coin quoted in tokens, else 0 |
| `Legs` (974 B) | 910..918 (`reserved[0..8]`) | `lift_at` u64: `⌊supply × curve_bps / 10,000⌋` with max wallet, else 0 |
| `Legs` | 918 (`reserved[8]`) | `lifted` u8, 1 once lifted (the kit reads it) |
| `Legs` | 926 + 8·i (`reserved[16 + 8i]`), i < 6 | leg `i`'s coins sold, i64 (share less the pool's base reserve after its last trade; counted only while max wallet waits) |
| `Legs` | 75; 76 + 139·i + 131 | `opened` and each leg's `opened_at` (existing fields, now read by the kit for the locks' delay) |
| `Launch` | unchanged | `rules`, `modules`, the lock times (counted from the launch) and `kit_caller_bump` now set on a token-quoted coin |

**Events**:

- `TokenLaunchRules` — `74 f0 75 0b 68 43 d7 75` (self-CPI, after `TokenLaunchCreated`, only with a
  rule): `launch, mint: Pubkey, rules: LaunchRules, modules: u8, kit_config: Option<Pubkey>,
  creator_unlock_at, early_window_end, early_unlock_at: i64, lift_at: u64, ts: i64` (the lock times
  counted from the launch).
- `TokenLaunchLocks` — `cf c4 41 47 1c 56 4b 61` (self-CPI, after the last leg's `LegOpened`, only
  with a lock): `launch, mint: Pubkey, trading_opened_at, delay, creator_unlock_at, early_window_end,
  early_unlock_at, ts: i64`: the times in force (`delay = trading_opened_at − created_at` added to
  each, 0 staying 0). These are the times to show.
- `LegsMaxWalletLifted` — `b2 72 ea fd fd 95 68 9f`, logged by the pool hook (`Program data:`; the
  DEX calls the hook without the launchpad's event authority): `launch, mint, pool: Pubkey, sold,
  lift_at: u64, ts: i64`.

**Error codes** (appended): launchpad 6082 `HolderFeeOnTokenLaunch`; kit 6033 `LegsMissing`, 6034
`WrongLegs`, 6035 `RewardsOnTokenQuote`.

**Measured** (LiteSVM, every rule but holder fees; compute units as consumed by the transaction,
with a compute limit and a price; sizes with the protocol lookup table / without):

| instruction | compute units | accounts | transaction |
| --- | --- | --- | --- |
| `create_token_launch`, 1 leg | 131.5k–157.0k | 19 | 800 / 983 B |
| `create_token_launch`, 3 legs | 156.1k–169.6k | 21 | 882 / 1,065 B |
| `create_token_launch`, 6 legs | 180.4k–208.9k | 24 | 1,005 / 1,188 B |
| the same, 32 B name, 10 B symbol, 200 B URI | 178.8k–187.8k | 24 | 1,171 / 1,354 B |
| `open_leg` with the kit, SPL Token | 134.9k–143.9k | 26 | 781 / 1,026 B |
| `open_leg` with the kit, Token-2022 (xStocks-like) | 151.1k–155.6k | 26 | 781 / 1,026 B |
| the last `open_leg` (trading opens, `TokenLaunchLocks`), Token-2022 | 145.8k–150.3k | 26 | 781 / 1,026 B |
| `swap_spl` buy with the kit, SPL Token / Token-2022 | 107.3k / 114.3k | 25 | 767 / 919 B |
| `swap_spl` sell with the kit, SPL Token / Token-2022 | 107.6k / 114.6k | 25 | 767 / 919 B |
| a buy with the buyer's `create_holding` in front | 120.3k–124.8k / 127.4k–128.9k | | 786 / 969 B |

Ranges are three runs: the creates and `open_leg` vary with the bump searches of fresh addresses (the
legs' pools and fee accounts, the kit caller, the ATA program's); the swaps are exact. A leg trade
with the kit costs about 41.7k compute units more than without it (65.5k under SPL Token, §2.4).

**Sizes**: `bordrless_launch.so` 941,472 → 969,584 B (+28,112; sha256
`8fe9c810f4911d3ba179fa3976da0559a46ff92aa959ef1dbb930e8c3f797cf7`) and `bordrless_kit.so` 345,320 →
373,888 B (+28,568; sha256 `7302050b4989070a395bab787cbdedf77c414d0d996a0cb353b39a7cf0a66277`). The
launchpad's program data was 941,472 B and the kit's 352,888 B: the launchpad was extended by 28,112 B
and the kit by 21,000 B before the upgrades of 21 Oct, the kit first (the new launchpad's
`create_token_launch` calls `init_legs`; the new kit runs every existing coin as before). The DEX is
unchanged (it rebuilds to its deployed bytes).

### 2.6 Workshop items on token-quoted coins ("builder decides")

A token-quoted coin pays everything in its own tokens; a Workshop builder's script decides what to do
with them (keep them, trade the legs, pay holders in a token, or swap a token for SOL elsewhere and pay
that), and the presets keep and spend their parts in them (the owner, 21 Oct: "if the quote token is
SPY, it should pay in SPY"). There is no converter. Changes the launchpad and the companion; the DEX
and the kit are unchanged, and SOL coins, their companions and the coins made under §2.5 are
unchanged byte for byte.

- **A companion launches a token-quoted coin** with its new `launch_token`, which wraps
  `create_token_launch` as its `launch` wraps `create_launch`: the companion's creator address
  (`PDA(["creator", mint])` under the companion) signs as the creator, which `create_token_launch`
  now accepts (only the companion can sign for it). A script companion or a preset: not a game, nor
  a companion with a holders' part (`TokenLaunchKind`: a game trades and pays SOL; the holders' part
  pays through the kit's holder rewards, which a token-quoted coin has none of yet), nor one with
  payees (`TokenCoinPayees`: their parts are paid in SOL); a script's pot is the whole creator fee
  (`pot_bps` 10,000; `TokenCompanionSplit` otherwise); no creator wallet lock
  (`CreatorLockUnsupported`, as `launch`). The companion is native from its launch: SOL that reaches
  its creator address is its lamports, and a script's `claim_fees` books it to the pot.
- **Each leg's creator fee** is claimed by anyone with `claim_creator_fees_token` into the creator
  address's associated token account of the leg's token (created by the claim; the ATA program takes
  a PDA owner as it takes a wallet). For a script the companion splits nothing in tokens: those
  accounts hold the script's pot, every leg's in its token.
- **The script's step** (`script_step`, unchanged accounts; the launch's `pool` is leg 0's) runs with
  no SOL fees to claim and a SOL pot of zero unless SOL reached the creator address: its bounty is
  then nothing. The script spends the creator address's tokens by signing as it (a `swap_spl` on any
  leg with the creator address as trader, a `TransferChecked` to a holder's token account, a swap
  elsewhere). The companion checks, besides its checks on a SOL coin (the SOL pot, the creator
  address's rent, its holdings), that every token account of the creator address the step was handed
  is still the creator's: the same program and size, its owner the creator address, no delegate, no
  close authority (`ScriptBrokeCustody`). Its tokens may go; the account, its rent and the fee claims
  into it may not. Payout terms and payout answers stay SOL. `dev_buy` is refused (`NotOnTokenCoin`):
  the launcher buys on a leg with their own tokens.
- **A preset's parts are per leg, in the leg's token.** `launch_token` makes the preset's
  `CompanionLegs` (`PDA(["companion-legs", mint])`, the creator address paying its rent), a `LegPot`
  per leg: the leg's token, the buckets (buyback, holders, launcher, pot), the coins a Snowball holds
  from that leg, its reference price (the leg's opening price) and its last trade. Anyone's
  `claim_fees_leg(i)` claims leg `i` (`claim_creator_fees_token` into the creator address's account
  of the leg's token) and splits what that account holds above the leg's set-aside as `claim_fees`
  splits SOL (the bounty first, in the token, to the sender's account of it when passed; the
  rounding to the pot, else the buyback, else the launcher). `withdraw_leg(i)` pays the leg's
  launcher's part to the launcher's associated account of the token (made when missing).
  `buyback_leg(i)` is `buyback` on the leg: a minute after the launch and an interval after the leg's
  last, waiting while the leg's price is more than 3% above its reference (which then catches up),
  at most the pool share of the leg's quote side, a minimum out from the leg's own curve less its
  fees and 2%; it buys with `swap_spl` (the creator address the trader) and burns what it bought.
  `snowball_step_leg(i)` is `snowball_step` on the leg with its pot and its coins (a buy on a drop, a
  sell on a rise, the flat market's rebalancing trade, a follow), after the early-buyer window. The
  lamport bounds (`max_buyback`, a Snowball's `max_trade`) are a SOL coin's and are not read; the
  pool share bounds a leg's trade. The SOL steps (`claim_fees`, `buyback`, `share`,
  `snowball_step`, `dev_buy`) refuse a preset's token-quoted coin (`NotOnTokenCoin`; `share` is
  allowed with the SOL switch, §2.7). A leg's currency is decided in one place,
  `CompanionLegs::pays_in_token` (the leg's token, or SOL for the launcher's and holders' parts with
  the SOL switch, §2.7). The buyback and the Snowball keep apart (review3 F1): `buyback_leg` moves
  only the leg's `reference_price`, `reference_at` and `last_at`, `snowball_step_leg` only its
  `snowball_reference` (from the opening price) and `snowball_last_at`.
- **A launch config** may follow the quote mints of `create_token_launch` (a wallet's or a
  companion's), its rules and creator fee repeated in the arguments (`ConfigMismatch`). Refused on a
  token-quoted coin: a SOL cut (`QuoteCutOnTokenLaunch`: nothing to pay it in), and an author's share
  someone else's launch would pay or a partner (`ConfigSharesOnTokenLaunch`: both are paid by the SOL
  claims; a config's author launching from it pays no share). The config's custom hook is held to
  `create_launch`'s rule (who may upgrade it was checked when the config was made: no one, Studio's or
  the protocol's key, or its own `hook_timelock`, which must hold no proposal) and becomes the coin's
  token hook; a custom hook and kit rules exclude each other, as on a SOL coin. A config without a
  hook may carry kit rules, whose six accounts then follow it.
- **The hook learns the legs** from its registry, which must list the coin's `Legs`
  (`HookMissingLegs`): the entry `Pda(launchpad, ["legs", Account(1)])` (the mint), as the kit's
  registry for a token-quoted coin lists it (`bordrless_launch::instructions::legs_registry_entry`).
  Every transfer of the coin and every leg trade then hands the hook the `Legs`: the legs' pools are
  `Legs.legs[i].pool` (offset 76 + 139·i + 51, `count` at 74). `open_leg` passes the hook's slice on
  each leg's deposit; leg trades carry it on the coin's side.

#### Interface (Workshop items)

Rust builders: `bordrless_launch::client::{create_token_launch_with, open_leg_custom}` and
`bordrless_launch::instructions::{legs_registry_entry, lists_legs}`; `bordrless_companion::client::
{launch_token, companion_legs_address, claim_fees_leg, withdraw_leg, buyback_leg,
snowball_step_leg}`. IDLs: `idl/bordrless_launch.json`, `idl/bordrless_companion.json`.

**`create_token_launch(args)`** — unchanged data. The creator may be a companion's creator address.
The remaining accounts, `n` quote mints first:

| launch | after the quote mints |
| --- | --- |
| without a config (as before) | the kit's six when the rules install the kit (§2.5) |
| from a config with a custom hook | the config (r), the hook program (r), the token program's signer for it (r), the hook's registry for the mint (r), its extras as the registry lists them, and a timelocked hook's `Timelock` (r) last |
| from a config without a hook | the config (r), then the kit's six when its rules install the kit, else nothing |

The config is the first remaining account the launchpad owns with a `LaunchConfig`'s discriminator (a
quote mint never is one). Checks after the rules' (and `HolderFeeOnTokenLaunch`): `ConfigMismatch`,
`QuoteCutOnTokenLaunch`, `ConfigSharesOnTokenLaunch`, `CustomHookWithKitRules`; the hook's accounts as
`create_launch` checks them (`CustomHookAccountsMissing`, `InvalidCustomHook`, `WrongHookSigner`,
`HookRegistryMissing`, `HookExtrasMismatch`, `HookTimelockInvalid`, `HookTimelockPending`), then
`HookMissingLegs`; without a hook, `KitAccountsMissing`/`WrongKitAccount` for the kit's accounts or
`UnexpectedCustomHookAccounts` for any account after the config; then the quote mints as before.
Effects: the mint's hook is the config's custom hook with the config's flags (it is called on the
mint, its extras passed); `Launch.config`, `custom_hook` and `custom_hook_flags` are set
(`author_share_bps` stays 0). Emits, after `TokenLaunchCreated`, **`TokenLaunchConfigured`** —
`be 3d 60 4a 3a 1b 9c 6c` (self-CPI, only from a config): `launch, mint, config: Pubkey, custom_hook:
Option<Pubkey>, custom_hook_flags: u16, ts: i64`; then `TokenLaunchRules` with any rule.

**`open_leg(index)`** of a launch with a custom hook: the remaining accounts are the hook
(`InvalidCustomHook`), the token program's signer for it (`WrongHookSigner`) and the hook's extras for
the deposit (`CustomHookAccountsMissing` without the first two); `create_spl_pool` gets them as the
coin's slice. **Leg swaps** carry `[hook, its signer, extras]` on the coin's side (`in_hook_accounts`
on a sell, `out_hook_accounts` on a buy), as a SOL coin's custom hook; transfers carry the hook and
its extras as on a SOL coin.

**Companion `launch_token(args: CreateTokenLaunchArgs)`** — `0a 80 56 ab 03 89 a1 f4`. Accounts: 0
launcher (s), 1 companion (w), 2 creator address (w), 3 `companion_legs` (w,
`PDA(["companion-legs", mint])`), 4 the launchpad (r), 5–6 event authority and program, then
`create_token_launch`'s accounts in its order (the creator address first, not marked a signer; the
mint signs; its sixth, the `Legs`, is read for a preset; a preset with payees whose launcher's part
stays in the legs' tokens appends its `PayeesLegs` last, §2.7). Refusals: `AlreadyLaunched`,
`TokenLaunchKind` (a game), `TokenCompanionSplit`, `HolderRewardsOff` (a holders' part without the
coin's holder fee), `PayeesLegsMissing`, `CreatorLockUnsupported`, `MissingAccount`, `WrongCreator`,
`WrongMint`, the launchpad's own, and `AuthorShareUnsupported` after it. A preset's `CompanionLegs` is
made (1,293 B, 7,218,680 lamports at mainnet's 5,080 lamports a byte, from the creator address's
lamports: the launcher's `fund` covers it); a script's address is left alone. Grows the companion by
its native byte (the creator address paying) and books its spare lamports to the launcher's part, as
a companion launched native. Emits `CompanionNativeSet` and `CompanionLaunched`.

**`CompanionLegs`** — discriminator `8e 9d 68 fd a3 af f4 ea`, 1,293 B: `version` u8 (1) at 8, `bump`
at 9, `mint` at 10, `count` at 42, `legs: [LegPot; 6]` at 43 + 203·i, `reserved` [u8; 32]. A
`LegPot` (203 B): `quote_mint` 0, `quote_program` 32 (0 SPL Token, 1 Token-2022), `decimals` 33,
`currency` 34 (0: the leg's token; 1: the launcher's and holders' parts in SOL, the SOL switch),
`pending_buyback` 35, `pending_holders` 43, `pending_beneficiary` 51, `pending_pot` 59, `tokens_held`
67, `reference_price` u128 75 (the buyback's), `reference_at` 91, `last_at` 99, `claimed_total` 107,
`bounties_total` 115, `spent_total` 123, `burned_total` 131, `paid_beneficiary_total` 139,
`snowball_reference` u128 147 and `snowball_last_at` i64 163 (the Snowball's own, review3 F1),
`shared_total` 171 (the holders' part shared through the kit), `swapped_total` 179 (tokens sold to
SOL), `reserved` [u8; 16] 187. The creator address's account of a leg's token holds exactly the leg's
four buckets after every step.

**The leg steps** (anyone; `index` u8 the leg). Named accounts of `claim_fees_leg` and `buyback_leg`:
0 cranker (s w), 1 companion, 2 creator address (w), 3 launch, 4 `companion_legs` (w), 5 system
program, 6–7 event authority and program; `snowball_step_leg` has the Snowball's terms
(`["snowball", mint]`, r) at 5 and the system program at 6; `withdraw_leg` has the sender (s w), the
companion, the creator address (w), the launcher (`beneficiary`, address-checked), the launch,
`companion_legs` (w), the system program. The remaining accounts are found by key:

| instruction | discriminator | remaining accounts |
| --- | --- | --- |
| `claim_fees_leg` | `8a 12 16 79 2a 54 8a a4` | `claim_creator_fees_token(index)`'s accounts and the launchpad (the creator the creator address); the sender's account of the leg's token (w) for the bounty |
| `withdraw_leg` | `75 30 3d e8 f2 6e bf 7f` | the leg's quote mint, the creator address's account of it (w), the launcher's (w), its token program, the ATA program |
| `buyback_leg` | `07 59 af c6 8f dc f4 dd` | the leg's `swap_spl` for the creator address (the coin's hook slice on the coin's side, the mint writable) and the DEX; a custom hook's registry; `create_holding` of the creator's coin holding and the token program; the token `burn`'s accounts (the hook and its extras); the bounty account |
| `snowball_step_leg` | `a2 45 3a d8 e1 ce c9 38` | `buyback_leg`'s without the burn, the coin's `Legs` |

Refusals: `NotLaunched`; `NotALegStep` (the launch is not token-quoted), `NoSuchLeg`; `MissingAccount`;
`NothingToDo` (nothing new to claim, nothing in the bucket, nothing a Snowball can trade);
`BuybackNotDue`, `SnowballNotDue`, `NoQuote`; Anchor's `AccountNotInitialized` for a companion with no
pots on the legs (a SOL coin's, a script's) or a leg Snowball step of a companion that is not one;
`ConstraintAddress` for another launcher. Events (self-CPI): `LegFeesClaimed` `7b 28 11 02 ff 58 f3
a2` (`companion, index, quote_mint, claimed, bounty, to_buyback, to_holders, to_beneficiary, to_pot,
cranker`), `LegBeneficiaryPaid` `83 8b 15 dc 91 29 11 7b` (`companion, index, quote_mint, amount,
destination`), `LegBoughtBack` `63 c0 b9 c2 e3 9b 76 59` (`companion, index, spent, burned, bounty,
price_after: u128, cranker`), `LegBuybackWaited` `54 25 80 a2 6b 30 a6 d3` (`companion, index, price,
reference`: u128s), `LegSnowballTraded` `5a a9 ff 60 98 a9 11 22` (`companion, index, side` (1 buy, 0
sell)`, on_step: bool, quote, tokens, bounty, price_before, price_after` (u128s)`, pot_after,
tokens_after, cranker`), `LegSnowballFollowed` `45 44 43 05 2d f8 98 5c` (`companion, index,
reference_before, price`).

**Error codes** (appended): launchpad 6083 `QuoteCutOnTokenLaunch`, 6084 `ConfigSharesOnTokenLaunch`,
6085 `HookMissingLegs` (6076 `CompanionTokenLaunch` is no longer raised); companion, one block 6095 to
6100: 6095 `TokenLaunchKind`, 6096 `TokenCompanionSplit`, 6097 `NotOnTokenCoin`, 6098
`TokenCoinPayees` (no longer raised since §2.7), 6099 `NotALegStep`, 6100 `NoSuchLeg`
(`ScriptBrokeCustody` now also covers the creator address's token accounts); §2.7 appends 6101 to
6104.

**Measured** (LiteSVM; the Workshop hook in the tests is `hook_tester`, which records every call, so a
real hook's costs differ; three runs; sizes with the tests' table, the protocol table with the
companion's event authority, the launchpad, DEX, token and SPL programs and the treasury (the leg
steps' also the ATA program), / without; every leg step fits with it, 994 B at most):

| instruction | compute units | accounts | transaction |
| --- | --- | --- | --- |
| `launch_token` (a script), 1 / 3 / 6 legs | 122.8k–136.3k / 137.7k–148.2k / 163.3k–179.8k | 20 / 22 / 25 | 742 / 956, 824 / 1,038, 947 / 1,161 B |
| `launch_token` (a preset, its `CompanionLegs` made), 1 / 3 / 6 legs | 129.8k–138.8k / 151.7k–163.7k / 175.2k–194.7k | 20 / 22 / 25 | 742 / 956, 824 / 1,038, 947 / 1,161 B |
| `launch_token` from a Workshop hook's config, 1 / 3 / 6 legs | 154.7k–172.7k / 169.7k–180.2k / 197.0k–222.5k | 27 / 29 / 32 | 941 / 1,155, 1,023 / 1,237, 1,146 / 1,360 B |
| the same, 6 legs, 32 B name, 10 B symbol, 200 B URI | 206.6k–218.6k | 32 | 1,308 B with the tests' table (1,029 B with a table that also holds the quote mints and the hook's accounts) / 1,522 B |
| `create_token_launch` (a wallet) from a Workshop hook's config, 1 / 3 / 6 legs | 116.7k–127.2k / 128.7k–143.7k / 164.1k–170.1k | 20 / 22 / 25 | 836 / 988, 918 / 1,070, 1,041 / 1,193 B |
| the same, 6 legs, the longest metadata | 174.9k–186.9k | 25 | 1,203 / 1,355 B |
| `open_leg` with the hook, SPL Token / Token-2022 (xStocks-like) | 148.3k–158.8k / 164.7k–172.2k | 28 | 753 / 1,060 B |
| `swap_spl` buy with the hook's slice and `create_holding`, SPL Token / Token-2022 | 107.6k–109.1k / 108.8k–110.3k | 25 | 850 / 1,033 B |
| `swap_spl` sell with the hook's slice, SPL Token / Token-2022 | 91.7k / 98.4k | 25 | 800 / 983 B |
| `claim_creator_fees_token` into the companion's ATA, creating it, SPL Token / Token-2022 | 34.9k–36.4k / 43.7k–54.2k | 12 | 474 / 564 B |
| `script_step`: a buy on a leg (SPL Token / Token-2022), a payment in a leg's token, a sell | 246.0k–259.5k / 230.6k–236.6k, 112.7k–127.7k, 225.5k–230.0k | 56, 39, 56 | 357, 331, 357 B with the coin's step table (1,008, 858, 1,008 B with the protocol table; 1,439, 1,134, 1,439 B without) |
| `claim_fees_leg`, the first (making the creator address's ATA) / later, SPL Token; Token-2022 | 82.7k–102.1k / 73.6k–93.0k; 93.2k–115.7k / 78.5k–101.0k | 22 | 582 / 734 B |
| `withdraw_leg` (making the launcher's ATA), SPL Token / Token-2022 | 46.4k–59.9k / 53.9k–67.4k | 14 | 540 / 630 B |
| `buyback_leg`, no rules / the kit's burns and max wallet / a Workshop hook (SPL Token; Token-2022) | 214.2k–247.2k / 259.4k–308.9k / 256.3k–275.8k; 203.3k–234.8k / 239.6k–289.1k / 248.4k–272.4k | 46 / 54 / 55 | 802 / 1,078, 812 / 1,150, 971 / 1,247 B |
| `snowball_step_leg` buy, no rules / the kit's / a Workshop hook (SPL Token; Token-2022) | 180.6k–189.6k / 234.6k–251.1k / 227.4k–269.4k; 168.2k–195.2k / 228.2k–247.7k / 225.4k–237.4k | 40 / 45 / 46 | 828 / 1,104, 835 / 1,173, 994 / 1,270 B |
| `snowball_step_leg` sell, the same | 157.2k–164.7k / 205.2k–211.2k / 204.0k–231.0k; 162.4k–184.9k / 222.4k–234.4k / 215.1k–222.6k | 40 / 45 / 46 | as the buy |

**Sizes**: `bordrless_launch.so` 969,584 → 985,592 B (+16,008; sha256
`a663ee1ea4754c4d166b347cdfa14d4fa05372ccc310d235e6436db109fcef1d`): the launchpad's program data
(969,584 B) must be extended by 16,008 B (about 0.111 SOL). `bordrless_companion.so` 1,317,152 →
1,471,576 B (+154,424, the presets about 138 KB of it; sha256
`347a6a2ae25de4bc85eab9b6a086bb00a4b7b41b255136f0b86c3c1d562b4c5d`): its program data (1,327,056 B) by
144,520 B (about 1.006 SOL). The DEX and the kit are unchanged (they rebuild to their deployed bytes).
These sizes are §2.6's alone; §2.7 grows the launchpad, the kit and the companion again (its sizes). Upgrade the launchpad first: a new companion on the old launchpad
fails only `launch_token` (`CompanionTokenLaunch`); the new launchpad takes a companion creator only
when the companion signs, which only the new companion's `launch_token` does before a launch.

### 2.7 Holder rewards in the quote token, and the SOL switch

A token-quoted coin may now take holder fees, and its launcher may choose SOL for what is paid out.
The leg hook takes the holder fee like a SOL coin's, on the quote side of every leg trade, into the
kit's associated token account of that leg's token. On a one-leg coin the kit pays it with its usual
reward accounting in that token (continuous, claimable any time). On a coin of several legs it pays
every leg's token in rounds (an hour by default, 10 minutes to a day), each holding its share once a
round.

**The SOL switch** is one byte, set at launch. It pays the launcher's part, a companion's holders'
and launcher's parts, and the holder rewards in SOL. Their tokens are sold on the token's registered
Raydium AMM v4, CPMM, CLMM or Pump AMM pool by one launchpad instruction, `swap_to_sol`, with a
minimum from the pool's price (its time-weighted price where it keeps one). Every 8 slots, all the
token's sellers together may sell at most half the pool's fee times its depth, so a push of the price
costs more than it gains. Buyback and Snowball keep their pots in the legs' tokens.

The Holders part, a Split with holders and payees now launch on token coins too. SOL coins don't
change.

#### Interface (holder rewards and the SOL switch)

**Notes on the build** (where it departs from the first design)

1. **Rounds pay on the least a holding held, not on `received_round`.** A holding's bytes 32..56 are
   `RoundData` (below): the least it held this round and the round before. `pay_round` pays
   `⌊amount × least / supply⌋`, where `least` is what it held from the start of the round before until
   now. A buy, or someone's dust, never stops a holder being paid (a stamp would have, for the whole
   round); what a holding bought in the last two rounds just isn't counted yet. The sum still can't pass
   `supply`, because every holding paid held at least `least` when `supply` was taken.
2. **No `payout_sol` account.** The crank makes the seller's wrapped-SOL account (it pays the rent),
   the program closes it into the crank, and the crank's own lamports pass `out` on to the seller in
   the same instruction. The crank ends up paying only the transaction fee.
3. **The minimum also allows for the pool's impact**: `min_out = value × (1 − f) × D / (D + value) ×
   0.99` (value at `P_ref`, `D` the depth on the SOL side). The design's `value × (1 − f) × 0.99`
   would refuse most sales near the cap. The venue enforces it as its own minimum out, and the
   program checks it again.
4. **The time-weighted price** covers 1,800 s, or the whole ring when it is younger but at least 300 s
   old (`ReferenceStale` otherwise). There is no age limit on the newest observation: a quiet CLMM
   writes one only when a swap moves its tick, so the price hasn't changed since then.
5. **`payouts` and `holder_round_secs` are one optional 5-byte `TokenPayouts`** after the 22-byte
   rules. To set either one without rules, write `LaunchRules::NONE` (22 zero bytes) first.
6. **Payees on token coins** are paid in each leg's token by `withdraw_leg`. A party whose token
   account is missing or can't receive is owed its share in `PayeesLegs` and paid at a later
   withdrawal, so nobody blocks the others. With the switch they are paid in SOL by `withdraw`.
7. **Kind 1 survives an issuer's drain.** A Token-2022 permanent delegate (xStocks) can take the
   vault's tokens. `sync` then lowers its record to the balance instead of failing, so the coin keeps
   trading and only rewards are lost.
8. **Registering the same pool again keeps its registrant and time** (a depth refresh), so nobody can
   hold off a deeper pool by re-registering the current one every hour. When a reference is replaced,
   the old pool's accounts must name the stored venue and pool before anything is read from them
   (`WrongVenueAccounts`). Found while testing: without this, another pool passed as "the old one"
   read as failing and let any pool replace a healthy reference.
9. **Errors** as built (below): the launchpad adds `SwapWindowFull`, `SwapBelowMinimum` and
   `WrongVenueAccounts`. The kit adds `InvalidRoundSecs` and `WrongSwap`, and drops `RoundsOnOneLeg`
   (the launchpad picks the kind from the leg count). The companion adds `PayeesLegsMissing`.
10. **`swap_to_sol` returns `(sold u64, lamports u64)`**, not `out` alone.

**Launchpad** (`1jcBymHxBjniZDhNPy51Vgm5Nz7pLUdxa9UBHc4TavC`)

- **`create_token_launch(args)`**: `CreateTokenLaunchArgs` = name, symbol, uri, `creator_fee_bps`,
  `virtual_quotes`, optional `rules` (22 B), optional **`payouts: TokenPayouts { flags: u8,
  holder_round_secs: u32 }`** (5 B). Nothing is written when both are 0, and data that ends before it
  reads as 0, so every earlier launch's bytes are unchanged. In `flags`, bit 0 is the SOL switch;
  any other bit, or a round other than 0 (an hour) outside 600..86,400 s, gives `PayoutsInvalid`. Holder fees are checked as
  on a SOL launch (6082 `HolderFeeOnTokenLaunch` is no longer raised). A holder fee installs the
  kit with `HOLDER_REWARDS` and a reward kind: **3 (SOL)** with the switch, **1 (the leg's token)** on
  one leg, **2 (rounds, `holder_round_secs`, an hour when 0)** on two to six. The accounts don't
  change: leg 0's quote mint, already among them, is the reward mint for kind 1.
  `Launch.reserved[19]` (byte 530) is `flags`. A launch with the switch or holder rewards emits
  **`TokenLaunchPayouts`** `2e 48 06 b4 ef a6 54 d4` (`launch, mint, flags: u8, reward_kind: u8,
  holder_round_secs: u32, ts`).
- **`open_leg(index)`** on a coin with holder rewards takes a fourth remaining account after phase
  1's three kit accounts: the kit's vault of the leg's token, `ATA(kit_config, quote_mint, quote
  program)`, writable (`KitAccountsMissing` when absent, `WrongKitAccount` at another address). It is
  made idempotently before the deposit runs the kit, the opener paying (165 B under SPL Token, more
  under Token-2022), and listed in the leg's registry as callback index **9**, `Key(vault)`, writable.
- **Leg trades with holder rewards** carry that vault after the leg's four extras (launch,
  `leg-fees`, `Legs`, kit config). The leg hook adds a second delta, the holder fee on the quote
  side, to index 9, which must be the derived vault (`WrongKitVault`); eligibility is the kit's
  `eligible`, as on a SOL coin. The coin's hook slice is the kit's with its `Legs`. Kind 1 passes
  `[kit config (w), the vault (r), Legs]` as the kit's extras (`kit::client::token_rewards_hook_slice`),
  where other kinds pass the kit's id. Builder:
  `launch::client::leg_swap_for(launch, legs, keys, trader, recipient, dir, amount, min)`, which is
  exactly `leg_swap_with_rules` without holder rewards.
- **`claim_creator_fees_sol`** takes a token coin whose switch is on (`is_native() || sol_switch()`).
  **`claim_creator_fees_token`** with the switch needs the creator's signature
  (`CreatorSignsTokenClaim`), except for a companion's creator address: the companion's own
  `claim_fees_leg` still claims, and its paid parts are sold by `swap_bucket_to_sol`.
- **`QuoteReference`**: `["quote-ref", quote_mint]`, discriminator `37 30 86 62 83 ec d4 c6`, 189 B:
  `version` 8, `bump` 9, `quote_mint` 10, `venue` 42 (1 AMM v4, 2 CPMM, 3 CLMM, 4 Pump AMM), `via` 43
  (0 SOL, 1 USDC), `pool` 44, `registrant` 76, `registered_at` i64 108, `depth_at` u64 116 (lamports),
  `window` u64 124 (`slot / 8`), `window_value` u64 132 (lamports sold in it), `paused` 140,
  `sold_total` 141, `lamports_total` 149, `reserved [u8; 32]` 157. Rent at mainnet's 5,080 lamports a
  byte: 1,610,360 lamports.
- **`register_quote_pool()`** `e7 48 74 3e 1e fc da e7`. Anyone may call it. Accounts: payer (s w),
  quote_reference (w), quote_mint, system program, event authority, program. Remaining: the pool's
  venue accounts (below, without a CLMM's tick arrays). For a pool against USDC, or when replacing a
  reference that is via USDC, USDC's reference and its pool's venue accounts follow. When replacing,
  the registered pool's venue accounts come last. Checks: a pool of the four programs with
  its discriminator and size; mints `{token, WSOL}` or `{token, USDC}` (Pump AMM: base the token, quote
  WSOL or USDC); its own vaults; swaps on; fee 100..30,000 ppm; at least 100 SOL on the SOL side (USDC
  valued through USDC's reference). It replaces a reference only with a pool at least 25% deeper,
  after an hour, or at once when the registered pool fails a check now. A Pump AMM pool with
  cashback is refused (`ReferencePoolInvalid`: its sale would credit the seller in lamports). Emits
  **`QuotePoolRegistered`** `a6 08 41 0c a7 00 6d 78` (`quote_mint, reference, venue, via, pool,
  replaced: Option<Pubkey>, depth_lamports, fee_ppm: u32, registrant, ts`).
  **`pause_quote_reference(paused: bool)`** `1e 7a ac 6a 89 2c cf a3`: admin (s), config,
  quote_reference (w), event authority, program. Emits **`QuoteReferencePaused`** `c0 cc 82 d8 b3 ed b3
  35`.
- **The venue accounts** (`launch::client::{amm_v4_accounts, cpmm_accounts, clmm_accounts,
  pump_accounts, via_usdc_accounts}`):

| venue | accounts, in order |
| --- | --- |
| Raydium AMM v4 `675kPX…1Mp8` | program, pool (w), `["amm authority", [nonce]]`, coin vault (w), pc vault (w); the swap is `SwapBaseInV2` (tag 16, 8 accounts) |
| Raydium CPMM `CPMMoo…KP1C` | program, authority `GpMZbSM2…vTKHJirzeGfMFoaZ8UR2X7F4v8vHTvxFbL`, amm config, pool (w), vault 0 (w), vault 1 (w), observation (w); `swap_base_input` (13 accounts) |
| Raydium CLMM `CAMMCz…grWqK` | program, amm config, pool (w), vault 0 (w), vault 1 (w), observation (w), Token-2022, Memo, then the bitmap extension and the tick arrays the swap crosses (w); `swap_v2` |
| Pump AMM `pAMMBay6…MfXEA` | program, pool (w), global config `ADyA8hde…`, base vault (w), quote vault (w), a protocol fee recipient of the global config and its quote ATA (w), event authority `GS4CU59F…`, the coin creator's vault ATA (w) and its authority, fee config `5PHirr8j…`, fee program `pfeeUxB6…`, `["pool-v2", base]` only when the sale pays a creator fee, a buyback recipient of the global config and its quote ATA (w); `sell` (21 accounts plus those) |
| through USDC | USDC's mint, the seller's USDC ATA (w), USDC's reference (w), the token's pool's accounts, USDC's pool's accounts |

- **`swap_to_sol(amount_in: u64)`** `12 3c ff 27 66 80 3e 8c`. Accounts: 0 crank (s w), 1 authority (s
  w: the kit config or the companion's creator address, signing through its program), 2 launch, 3
  `Legs`, 4 quote_mint, 5 source (w: the authority's token account of it), 6 quote_reference (w), 7 the
  authority's WSOL ATA (w), 8 WSOL mint, 9 the quote's token program, 10 SPL Token, 11 ATA program, 12
  system program, 13–14 event authority and program, then the venue accounts. The order of the sale:
  - the switch is on (`SolSwitchOff`); the authority is a custodian (`NotACustodian`); the quote is a
    leg; the reference isn't paused; the pool still passes its fee and depth checks;
  - `P_ref = max(spot, 97% × TWAP)` on CPMM and CLMM, the spot on AMM v4 and Pump AMM; through USDC,
    the token's price in USDC times USDC's own;
  - the room is `fee × depth / 2` per 8-slot window, less what the window has sold, across every
    seller of the token (and, through USDC, also USDC's window). `SwapWindowFull` under 0.01 SOL of
    room. The amount is `min(amount_in, balance, room at P_ref)`; `SwapTooSmall` under 0.01 SOL;
  - the swap with `min_out` (above); `SwapBelowMinimum` if what landed is less. Then the close, and
    the SOL passed to the authority.

  It returns `(sold, lamports)` and emits **`SwappedToSol`** `f4 2e ef 4b 77 da b6 89` (`launch, mint,
  quote_mint, authority, venue, via, amount_in, lamports_out, min_out, price: u128 (lamports per raw
  unit, Q64.64), window_value, ts`). Builder: `launch::client::swap_to_sol_rest(seller, mint,
  quote_mint, quote_program, venue)` gives the accounts from the launch on.
- **`swap_creator_fees(index: u8, amount_in: u64)`** `be cb 60 88 99 97 d5 c0`. Anyone may call it, for
  a wallet creator (`CompanionCreator` for a companion's coin). Accounts: crank (s w), launch (w), `Legs`,
  quote_mint, `leg-fees` (w), quote_reference (w), the launch's WSOL ATA (w), WSOL mint, the quote's
  token program, SPL Token, ATA program, system program, event authority, program, then the venue
  accounts. The launch signs. `creator_fees_accrued += lamports`, and the creator takes them with
  `claim_creator_fees_sol`.
- **Errors**, appended: 6086 `PayoutsInvalid`, 6087 `WrongKitVault`, 6088 `CreatorSignsTokenClaim`,
  6089 `QuoteReferenceMissing`, 6090 `ReferencePoolInvalid`, 6091 `ReferencePoolShallow`, 6092
  `ReferenceNotDeeper`, 6093 `ReferenceReplacedRecently`, 6094 `ReferencePaused`, 6095
  `ReferenceStale`, 6096 `SwapTooSmall`, 6097 `NotACustodian`, 6098 `SolSwitchOff`, 6099
  `CompanionCreator`, 6100 `SwapWindowFull`, 6101 `SwapBelowMinimum`, 6102 `WrongVenueAccounts`.

**Kit** (`14RJQXPdJfkehit6ezktjd3xujamf8nVSKw2shKamaEH`)

- **`init_legs(args: KitInitArgs, rewards: LegsRewards { kind: u8, round_secs: u32 })`**: the
  rewards are 5 optional bytes at the end, none for kind 0. With `HOLDER_REWARDS`, a kind is
  required (6035 `RewardsOnTokenQuote` is raised only when a launchpad sends none). Kind 1 reads the
  reward mint from the first remaining account and sets `reward_vault = ATA(config, mint)`. Kind 2
  takes `round_secs` (600..86,400, `InvalidRoundSecs`). Kind 3 is native from birth. Emits
  **`LegsRewardsInstalled`** `a3 c9 ff 73 ef 82 e0 5b` (`mint, kind, round_secs, reward_mint,
  reward_vault`) when the kind isn't 0.
- **`KitConfig.reserved`**: [10] (byte 386) the kind, [11..15] `round_secs`, [15] the reward
  program (0 SPL Token, 1 Token-2022), [16] the reward decimals. All are 0 on every kit made before.
- **A holding's bytes 32..56 on a kind-2 kit (`RoundData`)**: `mark` u32 (the round of the last
  update), `paid` u32 (the last round paid), `min_cur` u64 (the least held in round `mark`),
  `min_prev` u64 (in `mark − 1`). The callbacks keep it on every transfer and clear it when a holding
  is emptied. It is 0 on every other kit (the SOL coins' bytes don't change).
- **`claim_token`** `74 ce 1b bf a6 13 00 49` (kind 1): owner (s w), kit_config (w), mint, holding (w),
  reward_mint, reward_vault (w), destination (w: the owner's token account of the reward mint, its ATA
  made when it is that and missing, the owner paying), `Legs`, kit hook authority, token program, token
  event authority, the reward's token program, ATA program, system program, event authority, program.
  It syncs at the vault's balance, settles, and pays `min(owed, vault)` by `TransferChecked`. A
  destination that can't receive gives `NoTokenAccount`. **`claim_token_for`** `c9 8f 5f f5 d5 52 40
  46`: the same with payer (s) and owner (r), no ATA program or system program, and into an existing
  account. Both emit `RewardsClaimed` (amounts in the token).
- **`share_token(index: u8, amount: u64)`** `50 27 39 fa b6 2b 81 dd`: sharer (s), kit_config (w),
  `Legs`, the leg's quote mint, source (w), the kit's vault of the leg (w), its token program, event
  authority, program. Kind 1 streams the share over the hour, as `share`. Kind 2 adds it to the leg's
  vault, which the next round counts. Kind 3 gives `WrongRewardKind`: share SOL there.
- **`start_round()`** `90 90 2b 07 c1 2a d9 d7`: payer (s w), kit_config, kit_rounds (w), `Legs`,
  system program, event authority, program; remaining: every leg's vault in leg order. Once a round
  (`RoundAlreadyStarted`). It records the eligible supply and each vault's balance (0 for a vault not
  made yet), and makes `KitRounds` the first time (the payer pays 0.0016 SOL). Emits **`RoundStarted`**
  `b4 d1 02 f4 ee 30 aa 78` (`mint, round: u32, supply, amounts: Vec<u64>, ts`).
- **`pay_round(create_missing: bool)`** `58 6d c4 2e c7 de 5a 62`: payer (s w), kit_config, kit_rounds
  (w), mint, holding (w), owner, `Legs`, kit hook authority, token program, token event authority,
  system program, event authority, program. Remaining: per leg, the vault (w), the quote mint and the
  owner's ATA of it (w); then the token programs the legs use and, with `create_missing`, the ATA
  program. It is refused unless the round has started (`RoundNotStarted`), the owner isn't excluded
  (`NotAHolder`), the holding hasn't been paid this round (`AlreadyPaidThisRound`) and held something
  through the round before (`NotHeldThroughRound`); `NothingToClaim` when no leg pays. Each leg pays
  `⌊amount × least / supply⌋`, capped at what is left of it. A leg whose ATA is missing (without
  `create_missing`), frozen or asks for a memo is skipped for this holding this round. Emits
  **`RoundPaid`** `e1 dc d2 13 94 15 7f 9a` (`mint, round, owner, balance, amounts`).
- **`KitRounds`**: `["kit-rounds", mint]`, discriminator `7e 9e ac 24 81 1a 1f ba`, 191 B: `version`
  8, `bump` 9, `mint` 10, `round` u32 42, `started_at` i64 46, `supply` u64 54, `count` 62, `legs:
  [{amount u64, paid u64}; 6]` at 63 + 16·i, `reserved [u8; 32]` 159. It isn't in the IDL's
  accounts (it is read by hand), so the SDK decodes it from these offsets.
- **`swap_rewards_to_sol(index: u8, amount_in: u64)`** `f9 66 8a 11 6a 1e 5f fd` (kind 3, anyone):
  crank (s w), kit_config (w), `Legs`, the leg's quote mint, the kit's vault of the leg (w), the
  launchpad, event authority, program. The remaining accounts are `swap_to_sol`'s from the launch on,
  without its `Legs`, quote mint and source (the kit names those): `swap_to_sol_rest(kit_config, …)`.
  The kit syncs, then CPIs `swap_to_sol` with the config signing; `amount_in` 0 means the whole vault.
  What lands is streamed over the hour when it divides and is at least `MIN_SHARE_LAMPORTS`, else it
  waits for the next sync. Emits **`RewardsSwappedToSol`** `69 0f 3c 3f b1 48 87 19` (`mint, index,
  quote_mint, amount_in, lamports, shared: bool`).
- **`claim_sol` / `claim_sol_for` on a token coin** take the coin's `Legs` as the first remaining
  account (`LegsMissing`), so every leg's pool is excluded; other kits ignore it. This closes phase
  1's TODO in `claim.rs` and `native.rs` (§2.5). The bridged-SOL `claim` and `share` give
  `WrongRewardKind` on a kit whose kind isn't 0.
- **Errors**, appended: 6036 `WrongRewardKind`, 6037 `NoTokenAccount`, 6038 `RoundNotStarted`, 6039
  `RoundAlreadyStarted`, 6040 `NotHeldThroughRound`, 6041 `AlreadyPaidThisRound`, 6042
  `InvalidRoundSecs`, 6043 `WrongSwap`. 6035's message now reads as above.

**Companion** (§2.6's presets, extended)

- **`LegPot`** grows to **203 B** and **`CompanionLegs`** to **1,293 B** (`legs` at 43 + 203·i;
  rent at 5,080 lamports a byte: 7,218,680 lamports). The fields from §2.6 keep their offsets, then:
  `snowball_reference` u128 147, `snowball_last_at` i64 163 (review3 F1), `shared_total` 171 (the
  holders' part shared through the kit), `swapped_total` 179 (tokens sold to SOL), `reserved [u8; 16]`
  187. `currency` (34) is 1 when the launch's switch is on: the launcher's and the holders' parts are
  then paid in SOL.
- **F1** (review3, Medium): the Snowball starts from the leg's opening reference
  (`snowball_reference`) and touches only `snowball_reference` and `snowball_last_at`. `buyback_leg`
  touches only `reference_price`, `reference_at` and `last_at`. On a SOL coin the two had their own
  fields, and on legs they had shared one, so a Snowball's steps could hold a buyback off
  indefinitely, or a Snowball follow could lift the buyback's reference to a pumped price. The
  reviewer's two tests are regressions in `token_quotes_presets.rs`.
- **`launch_token`**: holders and Split-with-holders need the coin's holder fee
  (`HolderRewardsOff`). Payees without the switch need their **`PayeesLegs`** appended after
  `create_token_launch`'s accounts (`PayeesLegsMissing`). It is made there, from the creator
  address's lamports (602 B, 3,708,400 lamports), and never forwarded. A script is unchanged.
  `TokenLaunchKind` is now raised only for a game; `TokenCoinPayees` is no longer raised.
- **`PayeesLegs`**: `["payees-legs", mint]`, discriminator `1c 5e a4 0c f8 f7 32 59`, 602 B: `version`
  8, `bump` 9, `mint` 10, `owed [[u64; 10]; 6]` at 42 + 80·leg + 8·payee, `beneficiary_owed [u64; 6]`
  at 522 + 8·leg, `reserved [u8; 32]` 570. Like `KitRounds`, it isn't in the IDL's accounts.
- **`withdraw_leg(index)`** with payees: after §2.6's remaining accounts come the `Payees`,
  `PayeesLegs` (w), each payee's wallet and ATA of the leg's token (w; made when missing, the sender
  paying), and the system program. What the leg's launcher's part gained since its last withdrawal is
  split as `withdraw` splits SOL; each party is paid its share plus what it was owed. Events
  **`LegPayeePaid`** `07 4d 36 83 67 cb 83 66` (`companion, index, payee, amount, owed`) and
  `LegBeneficiaryPaid`. With the switch, `SolSwitchOn`: those parts are sold to SOL, then `withdraw`
  pays them.
- **`share_leg(index: u8)`** `8e 9a e7 e6 0c 57 ac 92` (anyone): `claim_fees_leg`'s named accounts.
  Remaining: the kit's `share_token` accounts for the creator address (the kit, its config (w), `Legs`,
  the quote mint, the creator address's ATA (w), the kit's vault (w), the token program, the kit's
  event authority), then optionally the sender's account of the token for the bounty. It shares the
  leg's `pending_holders`, less the bounty. With the switch, `SolSwitchOn`. Emits
  **`LegSharedWithHolders`** `a2 ce 6a 75 fd 63 0b 4f` (`companion, index, quote_mint, amount, bounty,
  cranker`).
- **`swap_bucket_to_sol(index: u8, part: u8)`** `a7 b4 ba dd 8c 2a b2 df` (anyone; `part` 0 the
  launcher's, 1 the holders'; anything else gives `NotSwitchedPart`): cranker (s w), companion (w),
  creator address (w), launch, `companion_legs` (w), system program, the launchpad, event authority,
  program. The remaining accounts are `swap_to_sol_rest(creator address, …)`. It is refused without
  the switch (`SolSwitchOff`). It sells up to the bucket and credits what came back to
  `pending_beneficiary` or `pending_holders` (SOL), which `withdraw` (with payees) and `share` (→ the
  native kit) pay. Emits **`LegSwappedToSol`** `f0 23 92 1a d0 4b 7c 58` (`companion, index,
  quote_mint, part, amount_in, lamports, cranker`). `share` (SOL) is allowed on a token coin with the
  switch.
- **Errors**, appended after `NoSuchLeg` (6100): 6101 `SolSwitchOff`, 6102 `SolSwitchOn`, 6103
  `NotSwitchedPart`, 6104 `PayeesLegsMissing`.

**Measured** (LiteSVM, the venues dumped from mainnet, pools read on 21 Oct. "With the table" means
the protocol table plus the venue's fixed accounts; the second figure is without a table.)

| instruction | compute units | accounts | transaction |
| --- | --- | --- | --- |
| leg buy with the holder delta, SPL Token / Token-2022 (1 leg) | 101,005 / 116,625 | 26 | 757 / 1,002 B |
| leg sell with the holder delta, SPL Token / Token-2022 | 87,252 / 100,102 | 26 | 738 / 952 B |
| leg buy / sell, 3 legs (rounds) and 6 legs | 102,944 / 86,765 and 104,616 / 89,936 | 26 | as above |
| `claim_token`, SPL Token / Token-2022 | 38,042 / 41,489 | 16 | 543 / 695 B |
| `claim_token_for`, SPL Token / Token-2022 | 26,633 / 30,061 | 15 | 572 / 662 B |
| `start_round`, 3 / 6 legs | 24,749 / 44,517 | 10 / 13 | 469 / 497, 568 / 596 B |
| `pay_round`, 3 / 6 legs | 56,018 / 101,915 | 25 / 34 | 810 / 993, 1,107 / 1,290 B (six legs need a table) |
| `share_leg` | 60.8k–81.0k | 19 | 610 / 731 B |
| `register_quote_pool`, AMM v4 / CPMM / CLMM / Pump AMM | 14.9k / 15.9k / 34.9k / 21.7k | | 347 / 530, 351 / 596, 363 / 794, 367 / 860 B |
| `swap_creator_fees`, AMM v4 / CPMM / CLMM / Pump AMM | 74.8k–80.8k / 86.0k–87.5k / 137.7k–139.2k / 146.5k–149.5k | 19 / 21 / 27 / 29 | 495 / 771, 500 / 869, 511 / 1,035, 516 / 1,133 B |
| `swap_creator_fees` via USDC: CPMM NEAR / CLMM TSLAx / Pump CBUCKS, then AMM v4 SOL/USDC | 136k–146k / 191k–197k / 204.2k | | |
| `swap_rewards_to_sol`: CLMM TSLAx / Pump AMM ANSEM / via USDC (CLMM TSLAx/USDC, then AMM v4) | 147.4k / 179.8k / 220.0k | | |
| `swap_bucket_to_sol` (CLMM TSLAx), either part | 178.4k–190.4k | | |

Pushes measured on the live pools: on AMM v4 (RAY) and Pump AMM (ANSEM), a push of the spot before a
sale costs more than the sale loses at 1/100, 1/20 and 1/5 of the pool's token side. For example, on
RAY at 1/20 the pusher's round trip pays 612,598,021 lamports, against 608,814,678 lamports less for
the sale; on ANSEM at 1/5 it pays 5,420,928,655 against 4,964,667,160. On CPMM and CLMM a push of the
pool's whole token side fails the minimum (`SwapBelowMinimum`, the venue's own minimum out).

**Sizes** (`target/deploy`, platform-tools v1.57, SBF v3). The extends are against mainnet's program
data on 21 Oct, at mainnet's rent of 5,080 lamports a byte, measured:

| program | BASE | now | sha256 | program data on mainnet | extend |
| --- | --- | --- | --- | --- | --- |
| `bordrless_kit.so` | 373,888 | 508,464 | `5a432ba1dd85e3f45da3444fcff64cd24ef01adf7675b4a3870dcb660fd5a35f` | 373,888 | 134,576 B, 0.684 SOL |
| `bordrless_launch.so` | 985,592 | 1,178,104 | `312b06b43179a1afea933298c6b97fce9d91f4ce77bf03e067162a5a42b6ca11` | 969,584 | 208,520 B, 1.059 SOL |
| `bordrless_companion.so` | 1,471,576 | 1,535,136 | `6287fb3969d695b31ea8e7468e55a3dd8f51901dbe461bf8c496ac326a8aa5d6` | 1,327,056 | 208,080 B, 1.057 SOL |

The extends total 2.80 SOL (3.84 SOL at the older 6,960 lamports a byte). The kit grew more than the design
estimated, because each of its seven new Anchor instructions costs about 15 to 20 kB. The upgrade order:
extend, then the kit, the launchpad, the companion.

**Tests** (`programs/tests/tests/`):

- `token_quotes_holder_rewards.rs` (8): one leg, SPL Token and Token-2022, fees into the vault and
  `claim_token`/`claim_token_for`/`share_token`; an issuer draining the vault; rounds paying every
  leg's token pro rata once a round; dust never making a holder miss a round; `create_missing`; a
  random transfer and pay walk never paying more than a round's amounts; the bounds; a launch without
  the new options being the launch it was.
- `token_quotes_sol_switch.rs` (11, `#[ignore]` without the dumps): each venue selling a creator fee
  (8 pools, SPL Token and Token-2022); USDC chains on CLMM, CPMM and Pump AMM; registration and every
  refusal, including the same pool again; the window across coins and in one transaction; pushes
  failing the minimum (CPMM, CLMM) or costing more than they gain (AMM v4, Pump AMM); custodians;
  the kit's sale and its stream; the kit's sale through Pump AMM and through USDC, within the call
  depth; a companion's two parts sold, then `withdraw` and `share`; the figures.
- `token_quotes_companion_payouts.rs` (3): the Holders part shared in each leg's token; payees paid
  in each leg's token and owed what they can't take; the figures.
- `token_quotes_presets.rs` (9, review3's two F1 tests added), `token_quotes_rules.rs` and
  `token_quotes_builders.rs` (holder fees and holders and payees now launch).
- `token_quotes_sol_differential.rs`: the earlier runs against the deployed programs pass unchanged
  (35 / 156 / 168 transactions identical). They also pass against BASE. Two new tests:
  - `token_coins_without_the_new_options_match_the_programs_before_them_byte_for_byte`
    (`QUOTES_75_BASELINE_SBF_DIR`, BASE's binaries, 46 transactions). It covers a one-leg coin, a
    three-leg coin with the kit's burn, max wallet and both locks, and a script companion's three-leg
    coin with its leg steps. The holder fee refused by BASE (6082) and launched now is checked on its
    own. The presets are left out, since F1 and §7.5 change `CompanionLegs` on purpose.
  - `a_token_coin_with_rules_made_before_the_payouts_upgrade_trades_after_it` (the deployed
    programs): the upgrade writes nothing; reward kind 0 and payouts 0 are read; trades and claims
    follow; the switch's sale is refused (`SolSwitchOff`).
- Unit tests: the kit's reward kinds and `RoundData` (`unit_tests/rounds.rs`); the launchpad's
  `Legs` offsets for the kit, the kit's `swap_to_sol` discriminator, the vault address both ways and
  `TokenPayouts`' bytes; the venue math (Raydium's tick price, the minimum, the cap).
- `scripts/solana/venue-fixtures.sh` dumps the five venue programs (Raydium AMM v4, CPMM, CLMM,
  Pump AMM and Pump's fee program) with `SOLANA_RPC_URL` from the environment, and checks their
  sha256 (`--check` checks only). The pools' accounts are in `programs/tests/fixtures/venues/*.accounts`.

**Sources for the venue layouts** (each checked against a live transaction or account):

- raydium-amm `91739d8d9f5cf347e0ce14ebe4d2bb7069fb31b6` (`AmmInfo` 752 B; `SwapBaseInV2` tag 16;
  tx `4FjtpEs9…qptyS45`, slot 459,537,366);
- raydium-cp-swap `f4b9b26342afe9122d5ff67cd7a4f19ded4ac0a5` (`PoolState` 637 B, `AmmConfig` 236 B,
  `ObservationState` 4,075 B, `swap_base_input`; tx `2B3jxbn7…de2A`);
- raydium-clmm `edfc5c0a0cd8084ed694668391174a35b5246cf5` (code at `ed1eb415…`; `PoolState` 1,544 B,
  `AmmConfig` 117 B, `ObservationState` 4,483 B, tick arrays 10,240 B, bitmap extension 1,832 B,
  `swap_v2`);
- pump-public-docs `639b87fb45c3adeffa6f7c4a1bf0cb85e392bf22` (the pool, `GlobalConfig`, the fee
  program's `FeeConfig` tiers, `sell`'s accounts; tx `3RJiS6yH…`, slot 459,539,525). A PDA as
  `user` was checked on `CPbNwMAz…`, slot 459,534,449.

**Off chain, needed before it opens** (nothing may open before the three upgrades):

- **SDK**: the new IDLs (`idl/bordrless_launch.json`, `bordrless_kit.json`,
  `bordrless_companion.json`), `TokenPayouts` written only when set, `leg_swap_for`'s vault on leg
  trades, the kind-1 hook slice, `KitRounds`/`PayeesLegs` decoders from the offsets above, the
  venue account builders, and the events and errors above.
- **launch/prepare**: holder fees on token coins; `payouts` (the DIY config's "Pay in SOL" switch, off
  by default); `holder_round_secs`; with the switch, the reference registered in the setup
  transaction when the token has none (the deepest qualifying pool on the four venues, else the
  switch greyed: `sol_switch_unavailable`); the open_leg's fourth kit account; Holders, a Split with
  holders and payees on token coins (with `PayeesLegs` appended); the fund covers `CompanionLegs`
  at 1,293 B.
- **Keeper**:
  - `crankSolSwitch` every 30 s for balances worth 0.01 SOL: `swap_creator_fees`,
    `swap_rewards_to_sol`, `swap_bucket_to_sol` (part 0 and 1). It pays the fees; the WSOL rent comes
    back.
  - the holder push: `claim_token_for` for kind 1, `claim_sol_for` with the `Legs` for kind 3.
  - `crankHolderRounds`: `start_round` once a round, then `pay_round` for holdings worth the push,
    `create_missing` where it pays for the ATA.
  - `share_leg` for presets' holders' parts (switch off), `withdraw_leg` with payees.
  - the registrar, which replaces references with deeper pools.
  - the `Legs` on every `claim_sol(_for)` of a token coin.
- **Indexer**: `TokenLaunchPayouts`, `QuotePoolRegistered`, `QuoteReferencePaused`, `SwappedToSol`,
  `LegsRewardsInstalled`, `RoundStarted`, `RoundPaid`, `RewardsSwappedToSol`, `LegSharedWithHolders`,
  `LegSwappedToSol`, `LegPayeePaid`. A holder delta on a leg trade is the holder fee in the leg's
  token.
- **Workshop**: the hook template's helper that reads leg pools from `Legs` lives in the monorepo's
  `tools/studio-builder`, not in this tree.
- One flag, `QUOTE_PAYOUTS_OPEN`, opened after the three upgrades.

**Risks**

- **Venue upgrades.** Raydium and Pump can upgrade their programs. A changed layout reads as
  `ReferencePoolInvalid` or `WrongVenueAccounts` and the sale waits, with the tokens still in their
  vault or bucket. `pause_quote_reference` stops a token at once; `venue-fixtures.sh --check` shows a
  re-dump that differs.
- **Spot-only venues** (AMM v4, Pump AMM) rely on the cap. The live-pool tests show a push costs more
  than it gains; the 1% slack and the per-window cap bound any one sale.
- **One window per token.** Every coin quoted in a token shares its 8-slot room, so a busy token's
  sales are delayed (not lost) while the keeper spreads them.
- **Rounds need pushes.** A holding the keeper doesn't push, because its share is below the
  threshold, is paid only when someone sends `pay_round` for it. What isn't paid rolls into the next
  round.
- **Kind 1 and an issuer.** A permanent delegate can take the vault; holders' rewards are lost, but
  the coin keeps trading.
- **Rent.** LiteSVM uses the older rent (6,960 lamports a byte); mainnet's is 5,080, so the tests top
  up the venue fixtures. The rents quoted here are mainnet's.
- **Cost.** About 2.80 SOL in extends. The `.so` sizes could come down only with a different build
  profile, which this change doesn't touch.

### 2.8 Games, Lossback and scripts with a launcher's part

The four "Hook + Companion" presets now launch on a token-quoted coin: the lottery, the jackpot, the
streak and Lossback. Each hook learns the legs from the coin's `Legs` in its registry
(`prepare_legs`). The companion keeps a game's pot, and the pot of a script with a launcher's part,
per leg in that leg's token (`CompanionLegs`). It pays prizes, shares and cover in each leg's token.

A game's minimum pot and an unaudited hook's cap are shares of the coin's opening value, so no price
is read. With the SOL switch a game's pot is sold to SOL and the game plays as on a SOL coin.
Lossback keeps its pot in the tokens.

The quote token is never burned: a blocked game's pot buys the coin back on its leg and burns the
coin. Strategy games stay on SOL coins. SOL coins don't change. Below, "the companion address" is the
companion's `["creator", mint]` PDA (the code's `creator_address`). Built on top of §2.7 (its
companion `6287fb39…`); not deployed.

#### Interface (games, Lossback and scripts with a launcher's part)

**Notes on the build** (where it departs from the first design)

1. **The quote token is never burned** (the owner's decision). A blocked game's
   pot on a token coin moves to each leg's `pending_buyback` (`LegPotToBuyback`, reason 1).
   `buyback_leg` then buys the coin back on that leg and burns the coin, as any leg buyback does.
   `burn_stranded` doesn't change: it burns only SOL. On a token coin that SOL is only the oracle
   float a block moved into the SOL buyback.
2. **Strategy games stay SOL-coin only.** `launch_token` refuses one (`StrategyOnTokenCoin`, 6108,
   "its plans and budgets are lamports"), and so does `set_game_token_terms`.
3. **The Studio starters keep `prepare` and gain `prepare_legs`.** The design had `prepare` write the
   `Legs` entry for every mint. That added an account to every SOL game coin's launch and trades, and
   a jackpot launch with a timelocked hook's `Timelock` grew to 1,243 B, over a packet
   (`companion_kinds.rs` caught it). `prepare_legs` has the same accounts and state, and its
   registry adds the coin's `Legs`. A SOL coin's registry, launch and trades are as before. The
   jackpot reads the `Legs` as the first remaining account after its two extras.
4. **The token mode is in `Game.reserved`**: the terms, `min_pot_bps`, a u16 at byte 452 of the
   486-byte `Game` (`set_game_token_terms` takes 0 to 10,000; 0 clears it), and byte 454
   (`reserved[2]`), which only `launch_token` sets to 1, when it launches a game with its terms.
   Terms set before a SOL coin's `launch` don't count: that game plays as a SOL coin's.
5. **`LegPot.prize` and `prize_paid` are at 187 and 195**, in the 16 reserved bytes §2.7 left.
   `LegPot` stays 203 B and `CompanionLegs` 1,293 B: no realloc, no new rent.
6. **The lottery's float is topped up with SOL**, not by a sale. `launch_token` books the launcher's
   funded spare into `Companion.pending_pot` (lamports). Any SOL later sent to the companion address
   is booked into the float at the next `draw` (what it holds above everything set aside). A float
   that can't pay the oracle rolls the round over (`OracleUnpaid`) and leaves the token pots alone.
   `swap_bucket_to_sol`'s `part` 2 sells a game's whole leg pot, and only with the switch
   (`SolSwitchOff` otherwise), as the other parts.
7. **Lossback's registry on a token coin** is the state (w), the launch, leg 0's pool (`Key`), the
   coin's `Legs`, then legs 1 to 5's pools (`Key`). Leg 0's pool keeps the SOL coin's third place.
   The leg count is `LossCoverState.reserved[0]` (byte 210). A callback finds each leg's pool by the
   `Legs`. A transfer must carry the `Legs`, the record leg's pool and, on a buy, the pool it buys
   out of (`WrongAccount`, as the design had it). A holding with no record takes the leg it buys on,
   or leg 0 for a transfer.
8. **Version 3 answers** come back from `loss_cover_script` as `CoverAnswer`: the Borsh of
   `PayoutAnswer` (v1) or `PayoutAnswerLegs` (v3), with no enum tag. The companion reads the version
   byte.
9. **Any script with a launcher's part** on a token coin (any Workshop script, not only Lossback)
   keeps its parts per leg, as the design had it for Lossback. Each leg's pot moves by what the companion
   address's account of that leg's token gained or lost during the step, and never below zero
   (`ScriptOverspent`). A leg whose account the step wasn't given is left alone. As on a SOL coin,
   whatever the step itself brings into that account counts as pot (a sale, or a fee claim made
   inside the step).
10. **Thresholds**: `TOKEN_POT_SCALE` = 50 SOL stands for the opening value, so `lamports_as_bps(x) =
    x × 10,000 / 50 SOL`. The 10 SOL cap is 2,000 bps, `MIN_POT_CAP` (0.1 SOL) is 20, and a dormant
    game's floor (`TOKEN_MIN_MIN_POT_BPS`) is 20. Each leg keeps `cap_bps × virtual_quote_i / 10,000`,
    rounded down.
11. **Agents**: nothing built (they need no program change). `bill_leg` in the script template and
    the keeper are left for the off-chain work below.

**Companion** (`6ZUM1gWBH9hBBNoJoaVAGwSftyZ6CUda6vUZTW9MsJuo`)

- **`set_game_token_terms(min_pot_bps: u16)`** `43 fa 69 29 84 fe 65 23`: mint (s), companion, game
  (w), event authority, program. Only before the launch (`AlreadyLaunched`). Above 10,000 gives
  `BadTokenTerms`, and a strategy gives `StrategyOnTokenCoin`. Emits **`GameTokenTermsSet`** `19 b6 62
  00 bd da 2a eb` (`game, mint, min_pot_bps: u16`). Client: `set_game_token_terms(mint, bps)`.
- **`launch_token`** takes lotteries, jackpots, streaks and scripts with a launcher's part.
  - A game's `Game` is appended after `create_token_launch`'s accounts (`launch_token_game`), before
    or after `PayeesLegs`, writable (a game with its terms is marked launched in tokens there); it is
    never forwarded (`MissingAccount` without it).
  - The game is checked as `launch` checks it (`check_game_launch`: the config's hook is the game's,
    with its flags and header, and a jackpot or streak registry has at most two extras besides the
    launch). Then `in_tokens()` must differ from the launch's SOL switch (`TokenGameTerms`).
  - `CompanionLegs` is made for every companion except a script that takes the whole fee.
  - A lottery in tokens books the funded spare as the float (6 above), not as the launcher's part.
- **The game steps on a game in tokens** (switch off) take, among their remaining accounts,
  `token_pot_accounts(mint)` (`CompanionLegs` (w), `Legs`).
  - The paying steps add `leg_prize_accounts(mint, quotes, winner, sender, bounty)`. Per leg, in leg
    order: the quote mint, the companion address's ATA (w), the winner's ATA (w; made when missing,
    the sender paying) and, with the bounty, the sender's ATA (w). Then SPL Token, Token-2022, the ATA
    program and the winner. The winner's ATA of every leg with something to pay must be there
    (`MissingAccount`) and writable (`PayoutTarget`), so no sender can leave a leg out of a prize or
    pass it read-only (or forfeit a jackpot round either way).
  - A jackpot's `settle`, and a `retire` of a jackpot or a streak whose pot is short, also take per
    leg its `leg-fees` account and the companion address's ATA of its token
    (`leg_fee_accounts(mint, quotes)`).
  - `with_accounts(ix, extra)` appends them.

  | step | on a game in tokens |
  | --- | --- |
  | every step (`apply_status`) | a cap: each leg's pot above `cap_bps × vq_i / 10,000` goes to its buyback (`LegPotToBuyback` reason 0; a streak's locked prize stays); a block: every leg's whole pot (reason 1), prizes cleared |
  | `draw` | `pot_bps ≥ draw_threshold_bps(min_pot_bps)` (`PotTooSmall`; once dormant the floor is 20); books the float; the oracle paid from it; at the request each leg's `prize = prize_bps × pending_pot_i`, **`LegPrizesFixed`** `46 c0 1e 52 47 8d 10 4a` (`game, mint, round, kind: u8, prizes: Vec<u64>, pot_bps: u64`); `Game.prize` 0 |
  | `claim_prize` | each leg's `prize − prize_paid` to the winner's ATA, less `bounty_bps` to the sender's ATA when passed; **`LegPrizePaid`** `a1 b2 1f 15 10 33 0b a8` (`companion, index, quote_mint, kind (0 draw, 1 settle, 2 share), owner, amount, bounty`) per leg; a leg whose ATA can't take its token now (frozen, memo) keeps its part |
  | jackpot `settle` | unfunded: `FeesUnclaimed` while a `claim_fees_leg` of every leg would fund it (the pot's part of each leg's unclaimed fees and of what the companion address's account holds above the leg's parts, as `fundable` on a SOL coin); funded: `prize_bps` of each leg's pot (`LegPrizePaid` kind 1) |
  | streak `close_epoch` | the threshold as `draw`; each leg's prize fixed and locked (`LegPrizesFixed` kind 2) |
  | `claim_share` | `share_of(prize_i, weight, total)` per leg, at most `prize_i − prize_paid_i` (kind 2) |
  | `retire` | each leg's `pending_pot` to its buyback (reason 2); `prize_due` counts what a claim would bring, as `settle` does |
  | `reveal`, `expire`, `close_receipt`, `burn_stranded` | unchanged (SOL only) |

  **`LegPotToBuyback`** `b5 86 c1 62 f9 a1 30 a1` (`companion, index, amount, reason: u8, pending_pot`).
- **With the SOL switch** a game takes no terms. Its pot's part of each claim waits on the leg until
  **`swap_bucket_to_sol(index, 2)`** sells it into the SOL `pending_pot`. Every step then runs its SOL
  path, as on a SOL coin. `part` 2 for anything but a game gives `NotSwitchedPart`. Scripts (Lossback)
  and Snowball keep their pots in the tokens.
- **Scripts with a launcher's part** on a token coin: the custody check is per leg (9 above). A
  version-3 answer is **`PayoutAnswerLegs { version: 3, entries: Vec<{ account: u8, amount: u64, tag:
  u32, leg: u8 }> }`** (14 B an entry, at most `MAX_PAYOUTS_PER_STEP`).
  - Each leg is bounded as a SOL pot is: `step_bps` of the leg's pot at the step's start; `window_bps`
    of its pot at the window's first step (kept in `LegPot.prize`, what the window paid in
    `prize_paid`; `window_start` stays in `PayoutTerms`); and `holding_bps` of the leg's step budget
    per holding.
  - The holding's owner's ATA of the leg's token must be among the accounts, writable and able to
    take it (`PayoutLegInvalid` when absent, `PayoutTarget` when it can't take it). Payment is by
    `TransferChecked`.
  - The hook records each payment (`record_payout(amount, tag)`, `paid` in the leg's token). Emits
    **`LegScriptPayout`** `b4 d4 87 09 50 31 73 29` (`companion, index, holding, owner, amount,
    tag`). The sender's bounty stays SOL.
  - Versions 1 and 2 stay SOL, and a v3 answer on a SOL coin is refused (`BadPayoutAnswer`).
- **Errors**, appended after 6104: 6105 `TokenGameTerms`, 6106 `LegAccountsMissing`, 6107
  `PayoutLegInvalid`, 6108 `StrategyOnTokenCoin`, 6109 `BadTokenTerms`. 6095 `TokenLaunchKind` and
  6096 `TokenCompanionSplit` are no longer raised (their messages say so). 6103 `NotSwitchedPart`
  names part 2.
- **Constants**: `PART_POT` 2, `TOKEN_POT_SCALE` 50,000,000,000, `TOKEN_MIN_MIN_POT_BPS` 20,
  `PAYOUT_ANSWER_LEGS_VERSION` 3.

**`lottery_hook`** (`HqFWsCBQ416DAfevJ9TspyT5yXGGoYTCpcreiGkCgWcr`)

- **`prepare_legs(round_secs: u32)`** `94 76 16 5d 1c 53 d4 19`: `prepare`'s accounts and state. The
  registry is `[state (w), launch, Legs]`. The callbacks ignore the third extra (tickets and `enter`
  are unchanged: every leg pool is off the curve). `prepare` is unchanged. Client:
  `prepare_legs(payer, mint, round_secs)`, `extras_legs(mint)`, `legs_address(mint)`.

**`loss_cover_hook`** (`6KMdXC3Fxovp3pnUsdQuUbderPzZtTA4t6R1jacYJWQr`)

- **`prepare_legs()`** `94 76 16 5d 1c 53 d4 19`: `prepare`'s accounts (the mint signs), then the
  legs' pools in leg order as remaining accounts (1 to 6; their keys are written, not read).
  `state.pool` is leg 0's pool and `reserved[0]` (byte 210) the count. The registry is as in 7 above.
  Client: `prepare_legs(payer, mint, pools)`, `extras_legs(mint, pools)`.
- **`Record.leg`** is byte 54 (it was reserved; 0 on every SOL coin's record), and the reserved bytes
  are now 55..64. The basis is per leg, and the callbacks price as the module doc
  says.

**`loss_cover_script`** (`9mRXU9gpWY3B13AyuCCtNjHnKt1pUFBQ6vJDLTwgakhm`)

- **Token mode** is taken when the launch is quoted in tokens. The state is `STATE_LEN_LEGS` 384 B: the
  SOL state's 128, then `LegRound` per leg at 128 + 36·i (`b`, `l_prev`, `l_cur`, `round_paid` u64,
  `measured` u32).
- Each leg rolls its round at its first step: `b_i` is the leg's `pending_pot` then, and
  `f_i = min(1, b_i / l_prev_i)`. It logs **`CoverRoundLeg`** `63 69 65 c9 5b 2b 47 c4` (`mint, round,
  leg, b, l_prev`). `min_pot_lamports` isn't applied there.
- A holding is measured against its record leg's spot, in that leg's token, and paid only from that
  leg's pot. `min_loss` is basis points of the holding's cost (`entry × balance`; the site sends 50).
- It answers version 3. The step's extra accounts add `CompanionLegs`, the `Legs`, every leg's pool,
  quote mint and the companion address's ATA of it, both token programs, and, for each paid holding,
  its owner's ATA of the record leg's token (w). Error `NoLegs` when the companion's legs don't
  match.

**The Studio starters** (`programs/tests/fixtures/starters/{jackpot,streak}`, and the same in the
monorepo's `apps/server/src/studio/starters/{jackpot,streak}.rs`)

- **`prepare_legs()`** `94 76 16 5d 1c 53 d4 19` (the Anchor name, so the same in every starter):
  `prepare`'s accounts. The registry is `extra_accounts_legs()`, which is `extra_accounts()` then the
  coin's `Legs`.
- The jackpot counts a buy out of any leg's pool when the `Legs` is passed (`read_legs`, then
  `qualifying_buy_legs`); otherwise the SOL rule applies.
- `bordrless-game` gains `legs.rs`: `LEGS_SEED`, `LEGS_LEN` 974, `MAX_LEGS` 6, `legs_address`,
  `parse_legs`/`read_legs` (owner the launchpad, discriminator, size, mint) → `LegsView { pools,
  count }`, and `qualifying_buy_legs`. It is copied into the monorepo's
  `tools/studio-builder/template/crates/bordrless-game` as well.

**Measured** (LiteSVM; legacy transaction sizes, and "through a table" with the coin's own lookup
table of every account but the sender's and the winner's or holders'):

| instruction | compute units | transaction |
| --- | --- | --- |
| `draw`, 1 / 3 / 6 legs | 84.5k–114.5k / 86.4k–108.9k / 89.3k–141.7k | 1,030 B (6 legs: 27 accounts) |
| `claim_prize`, 1 / 3 / 6 legs, with the bounty | 60.0k–82.5k / 99.5k–128.0k / 139.6k–163.1k | 1,117 / 1,381 / 1,579 B; 6 legs 519 B through a table |
| jackpot `settle`, 3 legs | 91.5k–124.8k | 1,449 B |
| streak `claim_share`, 3 legs | 97.3k–128.8k | 1,318 B |
| `swap_bucket_to_sol(index, 2)`, CLMM TSLAx | 175.7k–184.7k | |
| Lossback step in tokens, 3 legs, 2 payouts | 257.0k–287.0k | 1,653 B; 438 B through a table |
| six-leg Lossback buy on leg 5 (with `create_holding`) / sell | 103.0k–106.0k / 89.8k | 1,155 / 1,105 B (30 / 29 accounts); 405 / 355 B through a table |

`claim_prize` from 3 legs, `settle`, `claim_share` and a Lossback step need the coin's table (the
keeper's per-coin tables, `stepTables.ts`). A `draw` and a six-leg Lossback trade fit without one.

**Sizes** (`target/deploy`, platform-tools v1.57, SBF v3). The extends are against mainnet's program
data on 21 Oct, at 5,080 lamports a byte:

| program | §2.7 | now | sha256 | program data on mainnet | extend |
| --- | --- | --- | --- | --- | --- |
| `bordrless_companion.so` | 1,535,136 | 1,631,800 | `66599be3646e14e754bdebbcf747b484fe257cc8a2014c583c4ebbd71c8db2bf` | 1,327,056 | 304,744 B, 1.548 SOL (§2.7's 208,080 B and 1.057 SOL included: §2.8 adds 96,664 B, 0.491 SOL) |
| `lottery_hook.so` | 219,720 | 221,064 | `d8a28b5ee5904846e7f7e532ee0b59dc2e3cb0417cfbb1a17f01eaed8ebbaced` | 219,720 | 1,344 B, 0.0068 SOL |
| `loss_cover_hook.so` | 216,472 (the deployed source: 229,080) | 236,128 | `e39920fe5bfb02c19e0c34247611e2aee276045caf08fe94bee2d4855c00f97a` | 282,008 | none (45,880 B to spare) |
| `loss_cover_script.so` | 177,040 | 200,072 | `ebe6c64e44929bf4e3807283d3471b1ee4754fb1a517b87dcb77e0504efd7a73` | 241,736 | none (41,664 B to spare) |
| `studio_jackpot.so` / `studio_streak.so` (fixtures) | 170,616 / 186,056 | 174,176 / 187,584 | | Studio deploys a copy per coin | |

**Upgrade order**:

1. Extend the companion (if §2.7's extend has not already covered it) and `lottery_hook`.
2. Upgrade `lottery_hook`, `loss_cover_hook` and `loss_cover_script`, Bordrless's hook key signing.
3. Upgrade the companion, after §2.7's.

Two follow-ups after the upgrades:

- **Approve `loss_cover_script` again** with its new code's hash (`approve`, `APPROVAL_KIND_SCRIPT`).
  An approval holds only for the code it hashed, so every Lossback step is refused (`NotApproved`)
  until then.
- **Record a hashed audit again** if `lottery_hook` or `loss_cover_hook` has one
  (`set_hook_status_v2`). Such an audit also holds only for its code.

SOL coins run the same on the new builds (the differentials below), so the order between the hooks
and the companion doesn't matter to them.

**Off chain, needed before it opens** (behind its own flag, after the upgrades):

- **SDK**: the IDLs (`idl/bordrless_companion.json`, `lottery_hook.json`, `loss_cover_hook.json`,
  `loss_cover_script.json`).
  - Builders: `set_game_token_terms`, `launch_token_game`, `token_pot_accounts`, `leg_prize_accounts`,
    `leg_fee_accounts`, `swap_bucket_to_sol` part 2, both hooks' `prepare_legs` and `extras_legs`, and
    the starters' `prepare_legs`.
  - Decoders: `Game` byte 452, `LegPot` 187 and 195, `Record` byte 54, `LossCoverState` byte 210,
    `LegRound`, `PayoutAnswerLegs`.
  - The events and errors above.
- **launch/prepare**: the four presets on a token coin (Lottery, Jackpot, Streak, Lossback; Strategy
  hidden).
  - The minimum pot is a share of the opening value: 100 bps by default, "1% of the opening value,
    about $50".
  - The setup transaction: `create`, the hook's `prepare_legs` (Lossback with the legs' pools in leg
    order), `create_game`/`create_game_v2` or `create_script_v2`, and `set_game_token_terms` (switch
    off only). Then `launch_token_game` (a game) or `launch_token`.
  - Lossback: `state_len` 384 and `min_loss` 50 bps.
  - `fund` covers `CompanionLegs` (1,293 B), and for a lottery the float (0.02 SOL or more).
- **Studio**:
  - new game hooks are built from the new starters (`prepare_legs`), and the API sends `prepare_legs`
    for a token coin;
  - the hooks Studio has already deployed have no `prepare_legs`, so they stay on SOL coins;
  - the game simulation's (`sim_game`) scenarios on legs, in the monorepo, aren't done yet.
- **Keeper**:
  - `games.ts` and `jackpots.ts` per leg: `claim_fees_leg` on every leg before `settle` and
    `close_epoch`; `draw`, `reveal` and `claim_prize` with the accounts above; the winner's ATAs (the
    keeper pays their rent once) and the bounty ATAs; the coin's table for the steps that need it;
    `retire`.
  - The float: watch `OracleUnpaid` rollovers and send the companion address SOL.
  - Switched games: `swap_bucket_to_sol` part 2 in `crankSolSwitch`.
  - Lossback: the step declaration brings the `Legs`, the pools, the quote mints and the companion
    address's ATAs, `CompanionLegs`, and only holders who have an ATA of their record leg's token.
- **Indexer**: `GameTokenTermsSet`, `LegPrizesFixed`, `LegPrizePaid`, `LegPotToBuyback`,
  `LegScriptPayout`, the script's `CoverRoundLeg`, and `LegSwappedToSol` with part 2.
- **Agents**: `bill_leg` in the script template, `DecideArgs`, and the keeper's per-leg state
  and bills. None of it is built.

**Review (21 Oct): four fixes.** An independent review found four bugs, each reproduced. All
four are fixed, and the reviewer's tests, which assert the fixed behaviour, are regressions here
(`review5_games.rs`, 5; `review5_lossback.rs`, 3). The shared root of F1 and F2: the token program
hands a hook the remaining accounts the sender chooses, unchecked, and §7.6 read leg pools, the
`Legs` and payout accounts from them.

- **F1 (high), Lossback fake losses** (`loss_cover_hook`). A sender could leave the `Legs` out, so an
  arrival was priced at leg 0's spot whatever its record's leg. Or it could leave the record leg's
  pool out, so the new coins kept the record's old entry. Either one inflated a basis and drained a
  leg's cover. Now a transfer on a coin quoted in tokens must carry the `Legs`, the record leg's pool
  and, on a buy, the pool it buys out of. An arrival it can't price at its record's leg is refused
  (`WrongAccount`, as the kit refuses a leg trade without its `Legs`). A SOL coin's transfers are
  unchanged.
- **F2 (high), a prize forfeited through a read-only account** (companion, `LegPayout`). A winner's
  ATA passed read-only was skipped as "can't receive", and the step still closed the draw, round or
  share. It is refused now (`PayoutTarget`). Only the owner's own frozen or memo-requiring account
  still keeps its leg's part in the pot.
- **F3 (medium), fees claimed raw** (companion, `settle`, `prize_due`). Fees claimed with the
  launchpad's permissionless `claim_creator_fees_token` sat unsplit in the companion address's
  accounts, and `settle` closed a fundable round unfunded. It now mirrors the SOL path's
  `fundable` (`fundable_bps`): each leg's pot, plus the pot's part of its unclaimed fees and of what
  the companion address's account holds above the leg's parts. A round such a claim would fund gets
  `FeesUnclaimed`. This also changes a case: fees waiting that would *not* fund the round no longer
  block it, so it closes unfunded, as on a SOL coin.
- **F4 (medium), token terms on a SOL coin's game** (companion, `Game`). `set_game_token_terms`
  before a SOL `launch` left the game in token mode, so every step failed `LegAccountsMissing` and the
  pot was stranded. Now only `launch_token` marks a game in tokens (byte 454, the `Game` writable at
  that launch). Terms set before a SOL coin's `launch` are left unread, and the game plays as a SOL
  coin's.
  - The review suggested refusing them at the SOL `launch`. That needs the `Game` among a SOL game
    launch's accounts, which would change every SOL game launch the API sends today. This fix
    changes no SOL coin's accounts.

**Account lists that changed for the API and the SDK:**

- `launch_token_game`: the `Game` is writable.
- `leg_fee_accounts`: per leg, two accounts: the `leg-fees` account and the companion address's ATA
  of the leg's token. `settle` needs them. A `retire` of a jackpot or a streak whose pot is short
  needs them too: `MissingAccount` otherwise.
- The prize steps: unchanged lists, but the winner's ATAs must be writable, as `leg_prize_accounts`
  makes them.
- Lossback: unchanged lists. The registry's extras must now be passed whole on every transfer and
  trade, as the registry gives them (a client that trimmed them is refused).

**Tests and differentials after the fixes:**

- New tests: a read-only leg account refused (`PayoutTarget`); a raw claim counted by `settle`;
  token terms before a SOL launch leaving a SOL game (its `draw` takes the SOL path); a Lossback
  transfer without the `Legs` or the record leg's pool refused; the `Game` mark (unit).
- The jackpot test now waits on fees that would fund the round.
- Every suite passes; the whole workspace had 1,014 tests passing (with review 6, below). The two
  `audit_pw_integration` differentials need a binary that isn't here, as before.
- The differentials: 37 SOL game transactions and 15 SOL Lossback transactions identical against
  the deployed programs; 57 identical against BASE (§7.5's coins without games); 156, 168, 35 and 46
  in `token_quotes_sol_differential`; `token_quotes_sol_switch` (11).
- `cargo clippy -D warnings` is clean, and rustfmt is clean on every changed file.

**A second review (21 Oct): two more fixes.** A second review checked the four fixes above (no new
holes; every differential identical) and found two issues that were already there at review 5.
Both are fixed. Its tests are regressions here: `review6_games.rs` (5) and `review6_lossback.rs` (6,
with review 5's three again).

- **N1 (medium), a pot full at its cap was never funded.** Each leg is cut to its share of the cap,
  `⌊cap_bps × vq_i / 10,000⌋`, then measured as `⌊pot_i × 10,000 / (vq_i × n)⌋`. Two roundings down
  make a pot exactly at its cap measure just under `cap_bps`. So when the cap is the threshold (the
  cap below the game's least), no lottery drew, a streak's `close_epoch` gave `PotTooSmall`, and a
  jackpot's rounds closed unfunded.
  - Fix: `TokenPots::holds` and `reaches`. When the cap is the threshold, a pot also holds it when
    every leg's pot is at least its share of the cap (`leg_cap`), as a SOL coin's pot at its cap is
    drawn.
  - Used by `draw`, `close_epoch`, `settle` (funded, and the `FeesUnclaimed` check on what a claim
    would bring) and `prize_due`.
  - A new test, `a_streak_pot_full_at_its_cap_closes_its_epoch`, covers the streak.
- **N2 (low), frozen fee accounts.** `fundable_amounts` (formerly `fundable_bps`) counted a leg's
  unclaimed fees even when its fee account was frozen. An xStocks-like issuer holds freeze
  authority, so `settle` would answer `FeesUnclaimed` until the 30-day grace ran out.
  - Fix: fees in a frozen account (`AccountState` 2) count for nothing. A leg whose companion
    address's account is frozen adds nothing either, since no claim can move or split its fees.
  - The review's "bounded by the grace" test asserted the delay itself, so it is adapted to the fix
    (`review6_a_frozen_fee_account_no_longer_delays_a_round`: the first settle closes the round,
    unfunded).

No account list changed, and the IDLs are the same. Only the companion changed: 1,631,800 B,
`66599be3…`, the extend above. The hooks and the script are as after review 5 (`loss_cover_hook`
`e39920fe…`, `loss_cover_script` `ebe6c64e…`, `lottery_hook` `d8a28b5e…`).

**After both reviews:**
- The whole workspace has 1,026 tests passing, apart from the two `audit_pw_integration`
  differentials, as before.
- The differentials are unchanged: 37 and 15 against the deployed programs, 57 against BASE, and
  156 / 168 / 35 / 46 in `token_quotes_sol_differential`; `token_quotes_sol_switch` passes (11).
- `token_quotes_games` has 11 tests.
- clippy and rustfmt are clean.

## 3. Tests

`programs/tests/tests/token_quotes_dex.rs` (a kind-2 pool's life, the mint policy, quote-side cuts,
the protocol fee sweep, the launchpad-pool rule, a sell refused into the pool's own token accounts),
`token_quotes_launch.rs` (1-leg and 6-leg launches with SPL Token and Token-2022 quotes, trading
refused until every leg is open, the creator fee per leg and its claim, every refusal),
`token_quotes_rules.rs` (phase 1: each rule alone and all of them, over one leg and three, in SPL
Token and Token-2022 quotes; the burn on every leg, max wallet across legs and its lift at the
threshold, the creator lock, the early-buyer lock on buys from different legs, leg pools never capped
or counted, holder fees refused, the kit's accounts checked, today's data without rules, and the
locks running from when trading opens: a last leg opened 61 s and 5,000 s after the launch still locks
the first buys for an hour from the opening and the creator for the whole lock, with
`TokenLaunchLocks` on the last `open_leg`),
`token_quotes_builders.rs` (§2.6: a script companion's launch of a coin quoted in one token and in
three, SPL Token and xStocks-like Token-2022; each leg's fee claimed into the companion's token
accounts, ATAs of a PDA made by the claim; the script, `fixtures/scripts/example`'s leg modes, buying
on a leg with those tokens, selling, paying a holder in a leg's token, each step with a SOL pot of
zero; SOL reaching the creator address booked to the pot and spent within it; the creator's token
accounts kept its own against a hand-over, a delegate, a close authority and a close; a Workshop
hook, `hook_tester` with the `Legs` in its registry, from a config: its cut on every leg's buys and
sells, on a transfer and on each leg's deposit, the `Legs` handed to it every time, by a wallet and by
a companion; every refusal; the figures, `measures_builders_compute_and_sizes`),
`token_quotes_presets.rs` (§2.6's presets: Buyback, the launcher's part, a Split and a Snowball over
one SPL Token leg, one xStocks-like Token-2022 leg and three legs of both; their `CompanionLegs`, each
leg's claim split in its token with the bounty, the launcher paid in each token, a buyback on every
leg within its curve and burned, its premium wait, a Snowball's buys, sells and follow on every leg
with its own pot; with the kit's rules and with a Workshop hook; the SOL steps and wrong legs refused;
the figures, `measures_presets_compute_and_sizes`) and
`token_quotes_holder_rewards.rs`, `token_quotes_sol_switch.rs` (the venues dumped from mainnet:
`scripts/solana/venue-fixtures.sh`; `#[ignore]`d without them) and `token_quotes_companion_payouts.rs`
(§2.7: see its Tests), and
`token_quotes_sol_differential.rs` (35 SOL-path transactions run on this tree and on a 7fde540 build,
compared account by account, `QUOTES_BASELINE_SBF_DIR`; with `RULES_BASELINE_SBF_DIR` naming the
DEX, launchpad and kit deployed before §2.5, SOL kit launches with every rule, bridged and native, and
a token-quoted coin without rules, the same byte for byte, and accounts made by those programs reading
and trading after the upgrade; and, with `BUILDERS_BASELINE_SBF_DIR` naming the DEX, launchpad, kit
and companion deployed before §2.6, every kind of SOL companion, bridged and native (a script, a
buyback and launcher split, the presets Holders, Snowball, Buyback and a three-way Split),
custom-hook launches on SOL coins, and token-quoted coins with kit rules and without, the same byte
for byte (168 transactions); with `QUOTES_75_BASELINE_SBF_DIR` naming the programs before §2.7,
token-quoted coins without its options the same byte for byte (46 transactions);
`#[ignore]`d without them), and for §2.8 `token_quotes_games.rs` (a lottery on one and three legs,
the terms, a cap and a block to each leg's buyback and the coin burned, `retire`, a jackpot, a streak,
the float running out and topped up, a switched lottery on TSLAx (`#[ignore]`d without the venue
dumps), the six-leg figures), `token_quotes_lossback.rs` (a record's basis on the leg of its first
buy, each leg's pot paying only its holders in its token, a six-leg coin's trades) and
`review5_games.rs`, `review5_lossback.rs`, `review6_games.rs`, `review6_lossback.rs` (the reviews'
regressions), and
`token_quotes_games_differential.rs` (SOL games and a SOL Lossback coin against the deployed
companion, `lottery_hook`, `loss_cover_hook` and `loss_cover_script`, `DEPLOYED_76_SBF_DIR`, 37 and
15 transactions; token-quoted coins without games against the programs before §2.8,
`QUOTES_76_BASE_SBF_DIR`, 57 transactions; `#[ignore]`d without them):

```
scripts/solana/programs.sh build
scripts/solana/programs.sh test -p bordrless-program-tests --test token_quotes_dex
scripts/solana/programs.sh test -p bordrless-program-tests --test token_quotes_launch
scripts/solana/programs.sh test -p bordrless-program-tests --test token_quotes_rules
cargo build-sbf --tools-version v1.57 --arch v3 --manifest-path programs/tests/fixtures/scripts/example/Cargo.toml --sbf-out-dir target/deploy
scripts/solana/programs.sh test -p bordrless-program-tests --test token_quotes_builders
scripts/solana/programs.sh test -p bordrless-program-tests --test token_quotes_presets
scripts/solana/programs.sh test -p bordrless-program-tests --test token_quotes_holder_rewards
scripts/solana/programs.sh test -p bordrless-program-tests --test token_quotes_companion_payouts
SOLANA_RPC_URL=<a mainnet RPC> scripts/solana/venue-fixtures.sh
scripts/solana/programs.sh test -p bordrless-program-tests --test token_quotes_sol_switch -- --ignored
QUOTES_BASELINE_SBF_DIR=<7fde540 build>/target/deploy RULES_BASELINE_SBF_DIR=<pre-§2.5 build>/target/deploy BUILDERS_BASELINE_SBF_DIR=<pre-§2.6 build>/target/deploy QUOTES_75_BASELINE_SBF_DIR=<pre-§2.7 build>/target/deploy scripts/solana/programs.sh test -p bordrless-program-tests --test token_quotes_sol_differential -- --ignored
for f in starters/jackpot starters/streak; do cargo build-sbf --tools-version v1.57 --arch v3 --manifest-path programs/tests/fixtures/$f/Cargo.toml --sbf-out-dir target/deploy; done
scripts/solana/programs.sh test -p bordrless-program-tests --test token_quotes_games
scripts/solana/programs.sh test -p bordrless-program-tests --test token_quotes_lossback
DEPLOYED_76_SBF_DIR=<the deployed programs' ELF images> QUOTES_76_BASE_SBF_DIR=<pre-§2.8 build>/target/deploy scripts/solana/programs.sh test -p bordrless-program-tests --test token_quotes_games_differential -- --ignored
```
