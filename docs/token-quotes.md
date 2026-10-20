# Quote tokens: coins launched against SPL tokens

A coin may launch against one to six real SPL Token or Token-2022 mints instead of native SOL. Each is
its own DEX pool (a "leg") holding an equal share of the supply, coins only at the start, priced from
the same opening market cap in its token, never graduating. A coin is quoted in SOL or in its tokens,
never both. Native SOL launches are unchanged.

Deployed on mainnet on 20 Oct 2026: `bordrless_swap` in slot 459122832 (program data 662,200 bytes,
sha256 `567318c21044d62bb6eaade4993755b0d79f51b68a2457b075b2adea18a77104`) and `bordrless_launch` in
slot 459123096 (941,472 bytes, sha256 `5c5fd98ce1ca296c683f5b1e660df5720b4b1705e8ed478eb9c9a4dd025acc79`),
each extended first (+93,280 and +149,376 bytes).

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
- **Not available on a token-quoted coin (v1):** kit rules (holder rewards, burn, max wallet, the
  early-buyer and creator locks), companions of every kind (their pots are SOL), custom hooks and
  launch configs (Workshop or not), partner shares, the first-buy vest. The coin's mint has no hook: it moves freely.
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
  no kit, no custom hook and no companion (the creator a wallet: a companion creator is refused),
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
- **Refused on kind 2**: `graduate`, the bridged and SOL creator claims, every kit and companion path.
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

## 3. Tests

`programs/tests/tests/token_quotes_dex.rs` (a kind-2 pool's life, the mint policy, quote-side cuts,
the protocol fee sweep, the launchpad-pool rule, a sell refused into the pool's own token accounts),
`token_quotes_launch.rs` (1-leg and 6-leg launches with SPL Token and Token-2022 quotes, trading
refused until every leg is open, the creator fee per leg and its claim, every refusal) and
`token_quotes_sol_differential.rs` (35 SOL-path transactions run on this tree and on a 7fde540 build,
compared account by account; `#[ignore]`d unless `QUOTES_BASELINE_SBF_DIR` names that build):

```
scripts/solana/programs.sh build
scripts/solana/programs.sh test -p bordrless-program-tests --test token_quotes_dex
scripts/solana/programs.sh test -p bordrless-program-tests --test token_quotes_launch
QUOTES_BASELINE_SBF_DIR=<7fde540 build>/target/deploy scripts/solana/programs.sh test -p bordrless-program-tests --test token_quotes_sol_differential -- --ignored
```
