# Companions: reward tokens without a keeper

Status: v1 live on mainnet (2026-10-08); v2 (the lottery, below and `docs/games.md`) live since 2026-10-09; phase 2 (jackpot, streak) live on mainnet since 2026-10-09 (companion `fff582d8…`). Program `bordrless_companion`, one program deployed by Bordrless; each launch
that uses it gets its own instance. v2 is an additive upgrade of the same program: every v1
instruction, account and error code is unchanged.

## Why

Reward and buyback tokens on Solana run on their team's backend: a bot harvests fees, swaps them
and airdrops the proceeds from a list it computes off chain. Holders trust that it runs, computes
fairly and keeps nothing. A companion does the same with no one to trust:

- **The companion is the launch's creator.** It creates the launch through the launchpad (a CPI),
  signing as `creator = PDA(["creator", mint], companion)`, a system-owned address with no data
  that only the companion can sign for. Every creator fee therefore lands with the companion, and
  only its code decides what happens to it.
- **Every step is permissionless.** Anyone may send `claim_fees`, `buyback`, `share` and `release`.
  Each checks on chain that it is due, and pays its sender a small bounty. If Bordrless disappears,
  any holder keeps the token running.
- **Each step is one call into an existing Bordrless program:** the launchpad's
  `claim_creator_fees`, the DEX's `swap`, the token program's `burn` and `transfer`, the kit's
  `share`, the bridge's `unwrap_sol`. The companion never calls anything else.

## The instance

`Companion` at `PDA(["companion", mint])`:

- `mint`, `beneficiary` (the person who launched it; never the creator on chain).
- **The split of every creator fee claim**, in bps summing to 10,000:
  - `buyback_bps`: buy the token on its own pool and burn it.
  - `holders_bps`: stream to holders through the kit's reward pool (`kit.share`; the launch needs
    holder rewards on). While holders don't hold enough for the kit to accept a share, it waits.
  - `beneficiary_bps`: accrues to the beneficiary, who withdraws it as SOL.
- **The dev bag:** made within 10 minutes of the launch, held to what max wallet allows one wallet
  before graduation, released only after the early-buyer lock ends and no faster than max wallet
  lets the beneficiary hold. `dev_buy` lets the beneficiary buy at launch through the companion. The tokens
  stay in the companion's holding and vest linearly over `vest_secs` from the launch; `release`
  sends the vested part to the beneficiary. The dev can't dump what hasn't vested.
- **Buyback limits:** at most `max_buyback` lamports per call (at least 0.01 SOL), and at most a
  quarter of what a trader pays in fees for a buy and a sell on the pool (LP, creator, holder and
  burn fees), in basis points of the pool's quote side, never more than 1%; at least
  `buyback_interval` seconds apart (1 minute to 30 days); an output no worse than the pool's own
  quote at that moment less the fees and 2%. Someone who pumps the price by `p` before a buyback of
  `v` and sells after it gains about `p * v` and pays about `p * Q * (buy fee + sell fee) / 2`, so at
  that size a sandwich always costs more than it makes.
- **The reference price:** a buyback **waits** while the pool's price is more than 3% above it. It
  is the opening price at the launch; a wait raises it toward the price by 5% for each interval since
  it last moved (up to 64 steps, so one crank after a quiet spell catches up), and a buy only ever
  lowers it, so the price a buy leaves (its own, or a front-runner's) never raises it.
- **Bounty:** `bounty_bps` (at most 1%) of what a step moves, to its sender.

The templates are presets of these fields:

| Template | Split | Dev bag |
|---|---|---|
| The token that buys itself | 100% buyback | none (no dev at all) |
| Rug-proof dev | 50% holders / 50% dev, vested | the dev buy vests over 30 days |
| Buyback and reward | 50% buyback / 50% holders | optional |

## What a companion refuses (the 2026-10-08 audit, two rounds)

- Anyone but the mint's holder making its companion: `create` needs the mint's signature.
- A launch from a listed config that pays an author (the author's claims would pay the companion
  outside its steps); every claim splits whatever the creator's holding holds above what is set
  aside, so nothing that reaches it is ever stranded.
- A beneficiary that is not a wallet when holder rewards are on (the dev bag could never reach it).
- `withdraw` before the launch, unless the mint signs it: that refunds the funding of a launch that
  never happened.

The kit excludes the launch's creator only when it is exactly a companion's creator address,
decided once when the launch is made (`KitConfig.creator_is_companion`); every other creator is an
ordinary holder.

## Limits that shape it

- **Call depth.** `create_launch` reaches stack height 4; called from the companion, height 5, which
  is Solana's limit. So the companion calls `create_launch` directly, never through another layer.
- **Kit rules on the creator.** The kit's creator wallet lock and early-buyer lock apply to the
  launch's creator, which is the companion. The companion's own vesting replaces the creator lock,
  so companion launches leave it off.
- **Transaction size.** The launch transaction carries the companion and its two addresses besides
  `create_launch`'s accounts. With the 22-address protocol lookup table it fits, a game coin's
  launch from a config naming the lottery hook included (1,177 bytes at the longest metadata).
- **Custom hooks.** A companion launch refuses a custom token hook (`CustomHookUnsupported`),
  except the hook of the game it runs (below). The hook acts inside transfers, the companion acts on
  fees.
- **Heap.** A step builds every instruction it invokes and runs within the program's 32 KiB heap,
  which is never freed. Account lookups build their error only when an account is missing (v2: the
  v1 buyback with kit rules peaked at about 32.5 KiB, now about 19 KiB; a game coin's buyback, the
  heaviest step, about 21 KiB).

## Studio

Phase 1: the templates above, configured in Studio, with no new code per launch. Phase 2:
companions written for a creator by the assistant, deployed by Studio under the same rules as
hooks (immutable or Bordrless-upgradeable). They may call only an allowlist (the Bordrless
programs, the system program, Metaplex Core, a randomness oracle) and move funds only from their
own vaults, with a cap on what they hold until a human has audited them.

## Games (v2): a lottery run by the companion

The full reference is `docs/games.md`. Status: live on mainnet since 2026-10-09. v2 is
an additive upgrade of this program (owner decision a), with `lottery_hook`
(`HqFWsCBQ416DAfevJ9TspyT5yXGGoYTCpcreiGkCgWcr`) and the crate `bordrless-game`.

- **The split.** The companion holds the pot: a fourth part of every fee claim (`pot_bps`). Every
  claim's rounding goes to the pot.
- **The tickets.** The coin's token hook keeps who holds tickets, under the game ticket standard.
  The companion only reads the hook's state and the holdings' hook data, and never calls the hook.
- **The new fields.** `game_hook`, `pot_bps`, `pending_pot` and `round_secs` come from what was
  `Companion.reserved`. The account stays 300 bytes, and every v1 companion reads them as zeros:
  no game.
- **The game ticket standard** (`crates/bordrless-game`):
  - The hook's state, `PDA(["state", mint], hook)`, starts with a fixed header after the
    discriminator: magic `BRG1`, the mint, `round_secs`, the current round and its ticket total,
    the previous round and its total.
  - Each holding's 64 bytes of hook data hold two rounds of ticket ranges `[start, start + weight)`
    and `since`.
  - A round's tickets are the tokens held since it began, registered at the holding's first write
    that round (a transfer, a burn, or anyone's `enter`). Tokens bought or received during a round
    count from the next one. Ranges only shrink.
  - Only wallets hold tickets: never the launch, its pool, the creator address, or any address off
    the curve.
  - Attempt `k` of a draw picks ticket `u64_le(sha256(R ‖ u32_le(k))[..8]) % total`.
- **`create_game`** (before the launch, signed by the mint, in the setup transaction after the
  hook's `prepare`):
  - `Game` at `PDA(["game", mint])`, 486 bytes.
  - Kind `Lottery`.
  - A split with the pot's part and no holders' part. Buyback limits are required.
  - Rounds of 5 minutes to 30 days, matching the hook's header.
  - `min_pot` from 0.1 to 1,000 SOL, `prize_bps` from 10% to 100%.
  - Claim windows of a minute to a day, and 1 to 16 attempts, all within half a round (a
    five-minute round: two attempts of a minute, or one of up to two and a half).
  - Phase 1 takes only `lottery_hook`, or a hook the protocol has given a status, never a blocked
    one. Its registry may list at most 3 extras besides the launch (`create_game_v2`, phase 2,
    and a jackpot's or a streak's launch: at most 2, which fit a packet at the site's longest name
    and URI).
- **`launch`** accepts a custom hook only when it is the game's (`launch.custom_hook ==
  companion.game_hook`, decision b), with exactly the lottery's flags (145). The hook's state is
  read from `create_launch`'s own accounts, so the transaction does not grow. Author shares stay
  refused (decision d).
- **The token's moves.** `dev_buy`, `buyback` (swap and burn) and `release` carry the hook,
  resolved from its registry.
- **`claim_fees`** of a game passes the hook's status account, which need not exist.
- **The steps**, all permissionless:
  - `draw(round, slot)` commits the round's one seed, made from `slot` (one of the last 3 slots),
    and asks ORAO for it in the same instruction, the pot paying through `PDA(["oracle", mint])`
    (or adopts a pending request ORAO already holds for the seed). A seed is never on chain without
    its request, so nobody can preview an answer and then choose whether it is drawn. A seed ORAO
    already answered, or a slot more than 3 slots old, is refused (`StaleSeed`). It rolls over
    when the round has no tickets, when it is too late (10 minutes, or a quarter of a round
    shorter than 40 minutes, and a claim window before the claims end), or when the pot can't pay
    for ORAO's request now (`OracleUnpaid`, no seed committed).
  - `reveal`.
  - `claim_prize(k)`, in attempt `k`'s window, pays the winner in SOL.
  - `expire` rolls a round over.
  - `retire` sends a pot that has paid nothing for 60 days to the buyback.

  A draw of round `r` is decided before `r + 2` (a holding remembers two rounds). A round's seed is
  final. While ORAO leaves a paid request unanswered, the pot pays for a new one only after 1, 2,
  4… rounds; the draws in between roll over at once. `draw` (on the oracle's top-up) and
  `claim_prize` pay `bounty_bps` of what they move; the others pay nothing. A game coin's `dev_buy` and `buyback`
  need more than the default 200k CU: send them with a compute-unit limit.
- **`HookStatus`** at `PDA(["hook-status", hook])` is written only by this program's upgrade
  authority (`5xsibKwtiN6ruxsYrEyWVpV3KcwuzSPbQd1n28a7spEd` on mainnet), with `set_hook_status`
  (decision c):
  - **`audited`** lifts the cap and clears any block. It is final.
  - **`pot_cap`** applies while the hook is not audited: 0.1 to 10 SOL, and 10 SOL without a
    status. The pot share above the cap goes to the buyback.
  - **`blocked`** is only for a hook that is not audited, and only an audit lifts it. Every game of
    the hook sends its pot and pot share to buyback and burn, and ends any draw. Nobody, Bordrless
    included, receives anything.
  - If the hook also refuses the companion's buyback, `burn_stranded` burns that buyback as SOL
    once no buyback has bought or waited (moved its reference price) for 30 days. Each burn
    restarts the 30 days, and so does whichever call moves a blocked game's pot into the buyback
    (a game step, a fee claim, or the burn itself, which then burns nothing): a pot always gets its
    own 30 days for a buyback to spend it. So does a fee claim that credits the buyback at least
    what it held: a share claimed after a working hook's buybacks spent everything gets its own 30
    days. Only a credit at least as large as all the buyback holds (fees paid to the game, burned
    with the rest) restarts it, so nobody can put a refusing hook's burn off for ever.
- **ORAO VRF** (`VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y`), read on mainnet on 2026-10-08:
  - The fee is **500,000 lamports**, at bytes 72..80 of the network state
    `5ER1oENnV4srxYdAynUfRzWeQCPQaqMiAp4VqyMbSqnK`. The companion refuses one above 0.005 SOL:
    draws then roll over with no seed committed.
  - A request costs 4,955,160 lamports up front, the fee and a 749-byte account's rent.
    3,108,960 comes back when ORAO answers, so a draw costs 1,846,200 net (0.00185 SOL).
  - The 137-byte answered account's rent is never returned.
  - Its three signers could withhold or bias an answer. They can't choose the tickets or the seed.
- **Limits, measured** (v0 with the 22-address table):

  | Transaction | Bytes |
  |---|---|
  | Setup | 765 |
  | Launch | 1,177 at a 128-byte URI (1,142 at the site's); stack height 5, 40 trace entries |
  | `draw` (commit and request) | 731 (about 155k CU) |
  | `claim_prize` | 586 (about 124k CU) |
  | `buyback` | 886 |
  | `dev_buy` | 891 |

  The companion's `.so` grows to 590,976 bytes, so its ProgramData must be extended by about
  251 KB (about 1.28 SOL) before the upgrade.

## Games, phase 2: the jackpot and the streak (live since 2026-10-09)

The full reference is `docs/games.md` "Phase 2". An additive upgrade of the same program again:
every phase-1 instruction is byte-identical, and live companions and lottery games read the new
fields as zeros.

- **`create_game_v2(args, kind)`** makes a game of any kind: `CreateGameArgs`, then the kind's
  settings (a jackpot's timer and minimum buy, a streak's minimum streak and weight), which must
  equal what the hook's kind header says. `Companion.game_kind` and `Companion.pot_locked` come
  from the last 10 bytes of `reserved`; the kinds' fields from `Game.reserved`.
- **Jackpot.** `settle` (anyone) closes the oldest round that is over: it pays the last
  qualifying buyer `prize_bps` of the pot in SOL if their holding still holds what they bought,
  with nothing sent since (nothing when the pot is below its minimum), else forfeits the round. A
  round over is always closed: `settle` never waits. A qualifying buy is one out of the launch's
  pool, on the curve, of at least `min_tokens`, to a wallet. No randomness.
- **Streak.** `close_epoch(e)` (anyone, during the next epoch) fixes the epoch's pot and total and
  locks the pot for its holders; `claim_share(e)` (anyone, for any holding) pays the owner its
  share, `pot * weight / total`, once (a receipt per owner and epoch, its rent the sender's,
  returned by `close_receipt` after the claims). A weight is what was held since the epoch began,
  by a holding that sends nothing; unclaimed shares and dust roll over.
- **Studio game hooks.** `create_game` takes a hook only Studio's key or the protocol's can
  upgrade (its ProgramData passed) without a status; it is not audited, so capped at 10 SOL.
- **Limits** (v0, 22-address table): setup 784 bytes, launch 1,177, `settle` 610, `claim_share`
  614, `close_epoch` 465. The `.so` grows to 656,936 bytes (extend the ProgramData by 99,688).

## Approvals: the programs a companion runs beside its own (2026-10-12)

A script companion (`create_script`, `script_step`: a builder's own program spends the pot with
the creator address signing, inside the companion's limits) and a custom hook the site offers
beside a companion run only programs Bordrless has approved, each approval bound to the program's
code (a script nobody can change needs none: below):

- **`approve(program, kind, code_hash, author)`** writes `PDA(["approval", program])`: the
  program (its program account and ProgramData passed, its `Timelock` when it has one) as a token
  hook (kind 0, read off chain) or a companion script (kind 1), `code_hash` its executable hash
  (`solana-verify`'s, recomputed on chain: any other is refused), `author` who wrote it (a
  script's author earns half the creator fees of the companions made with it). Only a program
  nobody but Bordrless can change is approved: immutable, upgradeable only by Bordrless's keys, or
  behind a hook timelock (`ProgramNotBounded`). An approval counts while the program still runs
  the approved code; an upgrade voids it until the new code is approved.
- **`revoke_approval(program)`** marks it revoked; `approve` writes it again.
- **Who signs.** The companion program's upgrade authority (the protocol's), or the script
  approver (`SCRIPT_APPROVER`, `3onVUMKm…ipJW`): a hot key of Bordrless's backend that writes and
  revokes approvals once a submission has passed the Workshop's checks, so a clean pass waits for
  nobody. It can do nothing else here (not attest, not set a hook's status, nothing of a game or
  a companion) and is not an upgrade key, so losing it costs approvals, never a program. The
  approval records which of the two wrote it (`updated_by`), and each rewrites the other's.
- **An immutable script needs no approval** (the owner, 2026-10-12: the rule the launchpad applies
  to hooks). When the script program's upgrade authority is given up, `create_script` and
  `script_step` read no approval (the approval account passed may be an empty one, and a revoked
  approval of it has no say: nobody can change its code), and the script has no author: every
  claim is split as ever, nothing to an author. A program someone can upgrade keeps the rules
  above: without a current approval it is refused (`NotApproved`).

## Snowball: a market maker run by the companion (2026-10-12, not deployed yet)

The owner's preset: the companion's fee fills a pot that trades the token on its own pool, so the
chart keeps moving. An additive upgrade of the same program, like v2: every earlier instruction,
account and error code is unchanged, and every companion made before reads as classic.

- **`create_snowball(args)`**, after `create` and before `launch`, signed by the mint (like
  `create_game`): `Companion.mode` becomes `MODE_SNOWBALL` (the last byte of what was `reserved`),
  `args.split` with `pot_bps` replaces `create`'s split (the four add up to 10,000), and
  `PDA(["snowball", mint])` keeps the market maker's terms: `step_bps` (0.5% to 20%), `buy_bps` and
  `sell_bps` (1% to 50% each), `interval` (a minute to a day), `max_trade` (at least 0.01 SOL).
  Refused after the launch, on a game, twice, or out of bounds (`BadSnowball`, `BadSplit`). A game
  is refused on a Snowball companion (`BadGame`).
- **Every fee claim** adds the pot's part to `Companion.pending_pot` (the same field a game's pot
  uses, part of what is set aside): `PotFunded` with nothing to the buyback. No hook, so no cap and
  no block.
- **`snowball_step`** (anyone, a bounty on a trade), at most once an `interval`, from a minute
  after the launch and the end of any early-buyer window (`SnowballNotDue` otherwise):
  - the pool's price at or below the reference less `step_bps`: a **buy** of `buy_bps` of the pot;
  - at or above the reference plus `step_bps`: a **sell** of `sell_bps` of the tokens it holds,
    the SOL back into the pot;
  - between the two: `SnowballNotDue`.
  - The reference is the companion's opening reference until the first step, then the price each
    trade leaves. When the side the price calls for has nothing to trade (less than 0.001 SOL
    worth), the reference **follows** the price instead (`SnowballFollowed`, no bounty), so the
    market maker stays centred on the market.
- **Limits, the buyback's:** a trade moves at most `max_trade` and `pool_share_bps` of the pool's
  quote side (a quarter of a buy's and a sell's fees, never more than 1%), and takes no less than
  the pool's own quote less the fees and 2%. Pushing the price to trigger a trade only makes the pot
  buy lower or sell higher.
- **The tokens it holds** sit in the creator's token holding beside any dev bag;
  `Snowball.tokens_held` counts them, so a sell never touches the dev bag (tested with kit rules and
  a dev bag).
- **Events:** `SnowballCreated`, `SnowballTraded` (side, SOL, tokens, bounty, the price before and
  after, the pot and the tokens after), `SnowballFollowed`.

Tests: `programs/tests/tests/companion_snowball.rs` (a dip bought and a rise sold with the
reference following each trade; the same with kit rules beside a dev bag; the refusals), and
`snowball_vectors.rs`, which renders `programs/tests/vectors/snowball.json` for the SDK. A trade
costs about 236k compute units (283k with kit rules).

Site side (the monorepo): the SDK's `companion.createSnowball` / `snowballStep`, `decodeSnowball`,
`snowballMove`; the launch request's `companion.snowball: { potBps, stepBps, sizeBps }` (steps 1%,
2%, 3% or 5%, sizes 10%, 25% or 50%, a minute apart, at most 5 SOL a trade); the keeper's 30-second
`snowballs` loop; the token page's market maker figures. All of it stays off until the upgrade is
live: `SNOWBALL_OPEN=1` on the API and `NEXT_PUBLIC_SNOWBALL_OPEN=1` on the site.

## Payouts: a script pays holders through the companion (2026-10-12, not deployed yet)

The owner's Loss cover preset needs the pot to pay holders, each up to a sum its own hook has
measured. The primitive is general: any pair (a hook that marks eligibility in its holdings, a
script that asks for payouts) can use it with its own rules. An additive upgrade of the same
program: every earlier instruction, account and error code is unchanged.

- **The answer, not a call.** A script's `companion_step` may return a `PayoutAnswer { version: 1,
  entries: Vec<{ account: u8, lamports: u64, tag: u32 }> }` in its return data (Borsh, `1 + 4 +
  13 * n` bytes, at most 16 entries). `account` is an index into the step's remaining accounts, a
  holding of the launch's mint; `lamports` 0 is a touch (recorded, not paid); `tag` is passed to
  the hook untouched. The return data is cleared before the call; data another program set (a
  trade's hooks leave theirs) is no answer, so a script that trades keeps working; the script's own
  that is no answer fails the step (`BadPayoutAnswer`). The runtime refuses a call back into the
  companion, so the script names and the companion pays, as the token program's deltas and the
  strategies' `entitle` work.
- **The terms**, `PayoutTerms` at `PDA(["payout-terms", mint])` (128 bytes), made by
  `create_script_v2(args, payout { step_bps, holding_bps, window_bps, window_secs })` beside the
  script, the payer paying their rent (`BadPayoutTerms` out of bounds: each share 1 to 10,000,
  the window 10 seconds to a day; `PayoutTermsSet`). A script made with `create_script` has none:
  its first answer with an entry fails (`PayoutsOff`).
  `set_payout_terms(payout)` (2026-10-15), accounts `[authority (signer), companion, terms
  (writable), event_authority, program]`, replaces the terms at any time, before or after the
  launch, within the same bounds (`BadPayoutTerms`), signed by the launch's beneficiary
  (`Companion.beneficiary`, the wallet that launched it, not the companion's creator PDA) or the
  script approver (`SCRIPT_APPROVER`); anyone else `NotTermsAuthority`. The window restarts
  (`window_start`, `window_pot`, `window_paid` zeroed: the next step opens a window at the pot as
  it stands, under the new terms); `paid_total`, `payouts`, `last_paid_at` and the wallet-payouts
  flag are kept; `PayoutTermsSet`. The terms name no recipient: they only bound how fast the pot
  pays holders (or allowed wallets), so a looser set only speeds what the approved script already
  pays; who is paid is still the script's answer under the checks below, and the launcher is never
  a holding payout's recipient. The mint must have a custom hook with
  `WRITES_HOOK_DATA` (`PayoutsNeedHook`): a kit token or a hook without hook data can carry no
  record, so it can't be paid.
- **The checks, in order**, after the step's own (the pot spent and nothing else, the dev bag, the
  lamports, custody), every one before any payment: the budget, `min(step_bps of the pot at the
  step's start, window_bps of the window's base less what the window paid, the pot after the
  script less the bounty)`; the window's base (2026-10-15) is the pot after the script plus what
  the window has paid (the pot as if the window had paid nothing), never less than the pot at the
  window's first step (`window_pot`). Paying moves lamports from the pot to `window_paid` one for
  one, so payouts never shrink the cap and fees arriving mid-window are payable in that window;
  the cap is never below the frozen one it replaced, so a script that computes the old one still
  answers within it; the pot less the bounty bounds every step whatever the window, the holding cap `holding_bps` of it; then each entry: the
  index, a holding of the mint, named once, within the cap and the budget (`PayoutOverBound`), its
  owner eligible (never the launch, its pool, the creator address, the companion, the script, its
  state, the terms, the hook, the default key or an address off the curve: `PayoutTarget`), not
  the launcher (`PayoutToBeneficiary`); with lamports, the owner's account among the remaining
  accounts, writable, payable and not left below its rent-exempt minimum (`PayoutTarget`). A touch
  needs no owner account.
- **The payments**: one `unwrap_sol` of the total (the bounty with it), a transfer per paid entry.
  **The records**: a CPI per entry into the launch's hook, `record_payout(lamports, tag)` with
  `sha256("global:record_payout")[..8] ++ u64 ++ u32` and `[payout_authority (signer), mint,
  holding (writable), hook state PDA(["state", mint], hook) (writable), hook_authority
  PDA(["hook-authority"], hook), token program, token event authority]`, signed by
  `PDA(["payout-authority"])`, a program-wide address that signs nothing else. The hook writes its
  record through the token program's `write_hook_data`; a hook that can't record an entry fails
  the step. Depth: the step (1), the hook (2), the token program (3), its event (4). This is the one
  place the companion calls a hook: the hook can't re-enter it, and its worst case is refusing.
- **The accounting**: `pending_pot` less the total and the bounty; the window's and the terms'
  counters; one `ScriptPayout { companion, program, holding, owner, lamports, tag, window_paid,
  paid_total, pot_after }` per entry, touches included; `ScriptStepped` unchanged.
- **The bounty** of a step with an answer of `n` entries is `SCRIPT_BOUNTY * min(n, 8) / 8`: an
  empty answer pays nothing, a thin step earns less than its fee, a full one the bounty. A step
  with no answer keeps the flat bounty.
- **What a pair can do with it.** A script names holdings its author controls: it pays, per step,
  at most `step_bps` of the pot, at most `holding_bps` of that per holding, at most `window_bps`
  of the pot per window, only to wallets holding the mint, never the launcher, never any program's
  account; nothing leaves but SOL of the pot, and every entry is an event with the owner. The gate
  stays Bordrless's approval, which reads the terms a pair asks for; `revoke_approval` stops the
  steps at once.
- **Limits.** The runtime's instruction trace (64 a transaction) binds a step before its size: a
  touch costs four inner instructions (the hook's record, the token program's write and its event,
  the `ScriptPayout` event), a payment five (a transfer more), on a base of eleven (the compute
  budget's two instructions and the step, the script's call, the unwrap and its four, the bounty's
  transfer, the step's event; two more when the step makes the creator's token holding). So a step
  pays at most 10 holdings or touches 13 (measured in `companion_payouts.rs` and `loss_cover.rs`);
  the answer's 16 is the layout's bound.

Errors appended: `PayoutsOff`, `PayoutsNeedHook`, `BadPayoutAnswer`, `PayoutTarget`,
`PayoutToBeneficiary`, `PayoutOverBound`, `BadPayoutTerms`. Constants: `PAYOUT_TERMS_SEED`,
`PAYOUT_AUTHORITY_SEED`, `PAYOUT_AUTHORITY` (`8XJcBCYmYyggTQBNvK3RausfyRMpmtjJTjKd53W3StkG`, bump
251), `MAX_PAYOUTS_PER_STEP`, `PAYOUT_BOUNTY_FULL`, `RECORD_PAYOUT_DISCRIMINATOR`. Tests:
`programs/tests/tests/companion_payouts.rs` (every rule, with the fixture script
`fixtures/scripts/payouts`, which answers whatever its state holds) and `payout_vectors.rs`
(`vectors/payouts.json`).

## Loss cover: the creator fee pays the holders who are under water (2026-10-12, not deployed yet)

The owner's preset, built on the payouts above: a pair Bordrless owns, `loss_cover_hook`
(`6KMdXC3Fxovp3pnUsdQuUbderPzZtTA4t6R1jacYJWQr`) and `loss_cover_script`
(`9mRXU9gpWY3B13AyuCCtNjHnKt1pUFBQ6vJDLTwgakhm`), each with its README. The hook keeps every
holding's average entry price, what it holds, the cover it was paid and the round it was last
recorded in; the script pays every under-water holder the same fraction `f = min(1, B / L)` of its
uncovered loss, `B` the pot at the round's start and `L` the uncovered loss the previous round
measured, both on chain. A round is many short steps, each holding once a round.

- **The launch**: `create`, `loss_cover_hook.prepare(payer, mint)` (the mint signs; the pool's
  address follows from the launchpad's LP fee), `create_script_v2(... program: loss_cover_script,
  state_len: 128, init_data: the six terms ..., payout { every cap 10,000, window_secs: round_secs
  })`, the mint signing all three; then `launch` from a config naming the hook with flags 145 and
  no kit rules. The launcher's holding is never paid (`PayoutToBeneficiary`, and the script skips
  it).
- **The hook's rules**: a buy's basis is the swap's effective curve price (read from the pool
  after the DEX wrote its reserves and before it delivered, pinned in `loss_cover_hook.rs` against
  the DEX's write order), capped at 20% above a slow reference (5% a step, once in 10 minutes); a
  plain transfer in at the spot; a send, a sell or a burn keeps the cover pro rata and clears an
  emptied holding; the launch, the pool, the creator address and every off-curve address are
  exempt; `record_payout` takes only the companion's payout authority and a holding with a record.
- **The script's rules**: rounds of `round_secs` (a minute to a day), starting only on a pot of at
  least `min_pot_lamports` (below it a step fails with `NothingToDo`: no slot, no measuring); the
  first round and the round after a gap measure only; a holding counts from `min_holding` and
  `min_loss`, `min_age_secs` after its last buy, once a round (the hook's `round`); each payment is
  `uncovered * b / l_prev`, capped at `uncovered`, the round's budget and the companion's; a round
  whose budget runs out touches the rest, and the next round includes them in `l_prev` and catches
  up at a smaller fraction; a step with nothing to measure or pay fails. A sender chooses only
  which holdings a step carries and in what order: it cannot change `f`, pay anyone twice a round
  or more than its fraction, or inflate `L`.
- **Measured** (`loss_cover.rs`, v0 with a compute limit and price, the protocol table and the
  pair's addresses): a paying step of 7 pairs and a measuring step of 13 holdings fit a packet and
  the trace; 8 pairs are over the packet with the protocol table alone (1,233 bytes) and 10 fit it
  with a per-coin table of the pairs; 14 touches and 11 pairs do not fit the trace, so the keeper's
  batches are 7 and 13 (`vectors/loss-cover.json`, `batchPaying`, `batchMeasuring`); a coin's first step ever
  also creates the creator's token holding (two instructions more) and carries 12 touches at most.

Site side (the monorepo, `docs/loss-cover.md`): the SDK's coders from the three IDLs,
`companion.createScriptV2`, `decodePayoutTerms`, `payoutAuthorityAddress`,
`lossCoverHook.prepare/accounts/recordPayout`, the record and state decoders, `lossCoverOwed`,
`lossCoverFraction`, the step's accounts; the keeper's candidates and cursor; the launch preset,
the token page's panel. All of it stays off until the upgrade and the two programs are live.
