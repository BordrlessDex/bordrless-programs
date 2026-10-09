# Games: a lottery coin run by its companion

Status: built and tested, **not deployed** (2026-10-08). Phase 1 of the monorepo's
`docs/studio-companions.md` ("audited purse, Studio rules"). Three pieces:

| Piece | Where | What it is |
|---|---|---|
| `bordrless_companion` v2 | `programs/bordrless_companion`, same id `6ZUM1gWBH9hBBNoJoaVAGwSftyZ6CUda6vUZTW9MsJuo` | An additive upgrade. It holds the pot, draws and pays. Every v1 instruction, account, event and error code is unchanged. |
| `lottery_hook` | `programs/lottery_hook`, `HqFWsCBQ416DAfevJ9TspyT5yXGGoYTCpcreiGkCgWcr` | A lottery coin's token hook: it keeps who holds tickets. It never sees SOL, never refuses a transfer and takes no cut. |
| `bordrless-game` | `crates/bordrless-game` | The game ticket standard v1: the header and slot layouts, the rules a hook applies, and the helpers the companion reads with. |

Randomness comes from ORAO VRF (`VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y`). The companion only
reads the hook's state and the holdings' hook data; it never calls the hook. Phase 1 has the
lottery only. A last-buyer jackpot and a holding streak are later kinds, appended to `GameKind`.
This page is the reference; `docs/companions.md` has the summary.

## In plain terms

Part of every creator fee goes into a pot. Each round (an hour to 30 days), every holder's tokens
held since the round began are their tickets. Once the round is over, anyone can start a verifiable
draw; the pot pays the holder of the drawn ticket in SOL, if they still hold their tokens. Nobody
can take the pot: not the launcher, not the hook's author, not Bordrless. The most Bordrless can do
to a bad game hook is send its games' pots to the buyback, where they are burned.

## The owner's decisions (2026-10-08)

- **(a) An additive upgrade.** Companion v2 is an upgrade of the same program. Live companions and
  the 300-byte `Companion` account stay valid. The new fields come from `Companion.reserved`.
- **(b) The launch transaction doesn't grow.** `create_game` stores the game's hook, `pot_bps` and
  the round length in the `Companion` (`pending_pot` lives there too). `launch` checks
  `launch.custom_hook == companion.game_hook` without the `Game` account.
- **(c) The kill switch only for hooks not audited.** `HookStatus.blocked` may be set only on a
  hook whose status is not audited, and an audit clears a block. Blocking sends the pot share and
  the pot to buyback and burn, paying no person. The protocol authority writes `HookStatus`: the
  companion program's upgrade authority, `5xsibKwtiN6ruxsYrEyWVpV3KcwuzSPbQd1n28a7spEd` on mainnet
  (read from its ProgramData on 2026-10-08). A hook that isn't audited has its pot capped at 10 SOL,
  and the pot share above the cap goes to the buyback.
- **(d) No author shares.** A game companion's launch refuses a listed config that pays an author,
  as v1 does (phase 2).
- **(e) Lottery only.** Phase 1 runs the lottery only. `Game.kind` is a Borsh enum and
  `Game.reserved` has 83 bytes, so a jackpot or a streak can be added without moving anything.

## Who does what

| | Sees | Does | Can't |
|---|---|---|---|
| The lottery hook | Every transfer and burn of the coin (owners, amounts, balances before), each holding's 64 bytes of hook data, its own state | Rolls the rounds; gives each holding its range of the round; cuts ranges on sends and burns | See or move SOL, refuse a transfer, take a cut, call anything from a callback |
| The companion | The hook's header and each holding's hook data (read only), ORAO's accounts, its own | Splits fees four ways, commits a draw's seed, asks ORAO, pays the winner, rolls rounds over | Move a holder's tokens, call the hook, pay anyone but a winner, a step's bounty, the beneficiary's own split, the buyback |
| ORAO | The seeds asked of it | Signs each seed; the randomness is the XOR of its signers' signatures | Choose the round's tickets or the seed |
| The protocol authority | | Writes a hook's status: audited, a lower pot cap, or blocked | Take a pot, block an audited hook, lift the 10 SOL cap without an audit |

## The game ticket standard (`bordrless-game`)

**The header.** It sits at the start of the hook's state, `PDA(["state", mint], hook)`, right
after Anchor's 8-byte discriminator, at fixed offsets. The companion reads it from raw bytes, after
checking the account's owner (the hook) and its address. Integers are little-endian.

| Offset | Field | Type | What |
|---|---|---|---|
| 8 | `magic` | `[u8; 4]` | `BRG1` |
| 12 | `mint` | `Pubkey` | |
| 44 | `round_secs` | `u32` | Rounds are `floor(unix_time / round_secs)`; 1 hour to 30 days |
| 48 | `round` | `u32` | The round of the last write |
| 52 | `total` | `u64` | That round's ticket total |
| 60 | `prev_round` | `u32` | The round before the last roll |
| 64 | `prev_total` | `u64` | Its total |
| 72 | `last_buyer` | `Pubkey` | The last qualifying buy (jackpot; zero for the lottery) |
| 104 | `last_amount` | `u64` | |
| 112 | `last_buy_at` | `i64` | |
| 120 | | | The hook's own fields start here |

A round's total, once the round has ended: the header's own round gives `total`, `prev_round`
gives `prev_total`, a round with no write gives 0, and a round before `prev_round` is forgotten
(`total_of` answers `None`), so it rolls over.

**The slots** are a holding's 64 bytes of hook data.

| Bytes | Field |
|---|---|
| 0..4, 4..12, 12..20 | The current slot: `round` (the round the holding was last written in), `start`, `weight` |
| 20..24, 24..32, 32..40 | The previous slot: an earlier round's range, kept so its winner can claim after trading again |
| 40..48 | `since: i64`: when the holding last sent or burned, or first received |
| 48..64 | Free for the hook |

A slot with `weight` 0 holds no tickets. The current one is then written as its round alone, the
previous one as zeros.

**The rules.** `on_send`, `on_receive` and `on_enter` are what a hook applies.

- **A round's tickets are the tokens held since it began.** A holding gets its range of a round
  `[start, start + weight)`, at the end of the round's ticket space, at its first write that
  round, and only then:
  - a send or a burn registers what it keeps;
  - a receive registers what it held before;
  - `enter` registers its balance.

  Tokens bought or received during a round count from the next round. So moving tokens between
  wallets, or sending dust, never adds a ticket. A round's total is at most what its eligible
  holders held when it began.
- **Ranges only shrink.** A send cuts both slots to the balance left, and what is cut is dead for
  ever. No ticket number is ever given twice. A holding left with nothing is cleared to zeros, so
  it can be closed.
- **Who never holds tickets:** the launch, its pool, the companion's creator address, the default
  key, and every address off the ed25519 curve (any program's account).
- **A holding remembers two rounds.** A draw of round `r` is therefore decided within round
  `r + 1`. In `r + 2`, a holding written in both `r + 1` and `r + 2` has forgotten `r`.

**The draw index.** Attempt `k` of a draw with randomness `R` picks ticket
`draw_index(R, k, total) = u64_le(sha256(R ‖ u32_le(k))[..8]) % total`.
- The modulo favours the lowest `2^64 mod total` tickets by one part in `2^64 / total`: about
  2^-14 for a whole supply of 10^15 base units. The spec's 2^-40 was wrong.
- The holding whose range for the round contains the ticket wins, if the range is no larger than
  its balance and its owner is eligible (`wins`, `eligible`).

## The lottery hook (`lottery_hook`)

- **`prepare(round_secs)`**, for a mint that does not exist yet. The mint's keypair signs, so only
  the launcher chooses its rounds, once per mint.
  - It writes the state, `LotteryState`: 322 bytes, the header first.
  - It writes the registry, 114 bytes, listing the state (writable) and the launch (read-only).
  - It emits `Prepared`.
  - The rent is 3,515,360 lamports on mainnet's rent of 2026-10-08.
- **`before_transfer` / `before_burn`**, with flags `BEFORE_TRANSFER | BEFORE_BURN |
  WRITES_HOOK_DATA` (145).
  - They roll the header, remember the pool from the `Launch` account once it is readable, apply
    the rules to eligible owners, and answer only hook data.
  - No CPI, no event, no delta. About 7.5k CU.
  - Under a companion launch they run at stack height 5, Solana's limit.
  - Their effect is a pure function of the token program's `Transferred` and `Burned` events and
    the clock, so an indexer replays it exactly.
- **`enter`**: anyone, for any holding of the mint, nobody signing.
  - It registers the holding's balance for the current round when the holding has not been written
    this round. For a holding written this round already, or empty, it does nothing (so nobody can
    grief a holder by entering it).
  - It writes through the token program's `write_hook_data`, signed by the hook's
    `["hook-authority"]` (`6oZ9LkAfgmhmPYjXj3okjYK5sderddEo8fp8MR4H6Gx1`, bump 255).
  - It emits `Entered`.
  - Holders who don't trade in a round hold tickets in it only if someone enters them, so a keeper
    enters every holder early in each round.
- The token program's signer of its callbacks is `CFyuaxvKmpgnSMCoNqDKCcwxnTUeW8Mm1go1t3UMsvLH`.
- Every transfer of a lottery coin write-locks the hook's state (open question 8 of the spec).
- Protocol-upgradeable, like `tax_hook` and `half_life`.

## The companion's game

**`create_game(args)`**: before the launch, signed by the mint (as `create` is). It creates `Game`
at `PDA(["game", mint])`: 486 bytes, 3,119,120 lamports of rent, paid by the launcher.

- **`kind`**: `Lottery`.
- **`split` and `pot_bps`**: the four parts sum to 10,000. There is no holders' part (a game coin
  runs its own hook, so it has no kit), and buyback limits are required.
- **`round_secs`**: must match the hook's header, so `prepare` runs first, in the same setup
  transaction.
- **`min_pot`**: 0.1 to 1,000 SOL.
- **`prize_bps`**: 10% to 100%.
- **`claim_window_secs`**: 5 minutes to a day.
- **`max_attempts`**: 1 to 16, with `window × attempts × 2 ≤ round_secs`.
- **The hook** must be Bordrless's `lottery_hook`, or a hook the protocol has written a status for,
  and never a blocked one. A hook nobody vetted could refuse only the companion's own token moves
  and strand the buyback. Its registry may list at most 3 extra accounts besides the launch.
  `create_game` takes the hook's status account either way.
- It writes `game_hook`, `pot_bps` and `round_secs` into the `Companion`. The 64 bytes of
  `reserved` become `game_hook` (32), `pot_bps` (2), `pending_pot` (8), `round_secs` (4),
  `stranded_burned_at` (8) and `reserved` (10); every v1 companion reads them as zeros: no game.

**`launch`** accepts a custom hook only when it is the game's (decision b):

- The flags must be exactly 145. A hook taking transfer deltas could skim the companion's buybacks,
  and a callback the hook lacks would fail every burn.
- The hook's state for the mint must be a game header with the game's round length. The state is
  already one of `create_launch`'s accounts, so no account is added.
- Author shares stay refused (decision d).

**The token's moves.** `dev_buy`, `buyback`'s swap and burn, and `release` carry the custom hook,
whose extras the program resolves from the hook's registry, exactly as the token program's
callers do. The hook never gets the creator address's signature: the token program passes it
read-only. The kit path is unchanged.

**`claim_fees`** splits four ways, and the rounding goes to the pot. A game's claim must pass its
hook's status account (`PDA(["hook-status", hook])`, which need not exist), so leaving it out can
never lift a cap or a block.

## A round, step by step

Every step is permissionless.

| Step | When | Does | Pays its sender |
|---|---|---|---|
| `draw(round, slot)` | The round just ended, no draw in progress, and the pot ≥ `min_pot` (or the cap when lower; or 0.1 SOL once dormant) | Too late, after `last_draw` (10 minutes and a whole claim window before the claims end): rolls over (`Late`). Reads the round's total from the header (none: rolls over, `NoTickets` / `RoundForgotten`). When the pot can't pay for ORAO's request now (the breaker holding, ORAO's fee above the cap, its network state unreadable, the pot short): rolls over (`OracleUnpaid`), no seed committed. Else, in one instruction: commits the round's one seed, `sha256("bordrless-draw", mint, round, 0, slot, slot hash)`, from `slot`, one of the last 3 slots in the slot hashes sysvar; asks ORAO for it, the pot topping up `PDA(["oracle", mint])`, which pays and gets ORAO's refunds (or adopts, for free, a pending request ORAO already holds for the seed: v2 from anyone, or the deprecated v1); fixes the prize, `prize_bps` of the pot. A slot more than 3 slots old, or a seed ORAO already answered, is refused (`StaleSeed`) | `bounty_bps` of the top-up |
| `reveal` | ORAO answered, before the claims end (`round_end(r + 1)`) | Stores the randomness; the claim windows start | Nothing |
| `claim_prize(k)` | Inside attempt `k`'s window, `[revealed_at + k·window, +window)`, cut at the claims' end | Pays the holding that holds ticket `draw_index(R, k, total)`, in SOL, to its owner | `bounty_bps` of the prize |
| `expire` | No claim in any window (`NoClaim`); claims ended (`Late`, `OracleSilent`); ORAO's answer unreadable (`OracleUnreadable`) | Rolls the round over | Nothing |
| `retire` | No prize for two dormant periods (60 days, or 8 rounds), no draw in progress | Moves the pot to the buyback | Nothing; nobody is paid |
| `burn_stranded` | The hook is blocked, and for 30 days (or 4 buyback intervals) no buyback has bought or waited on its reference, no burn has run, no pot has moved into the buyback and no fee claim has credited the buyback at least what it held | Burns the buyback as SOL, at the incinerator. A pot no step had moved yet is moved into the buyback instead, and nothing is burned: the wait restarts | Nothing; nobody is paid |

- **A round has one seed, and it is final.** ORAO's devnet answers with the mainnet signers' keys,
  and their signatures are deterministic, so anyone can preview mainnet's answer to a public seed.
  - A seed known or chosen long before the draw could be ground. One made from the hash of one of
    the last 3 slots, and requested in the instruction that commits it, can't.
  - A new seed after a timeout would sell a re-draw to whoever keeps ORAO's answer out of the
    blocks. With one final seed, a censor can only delay the reveal, or roll the round over by
    censoring through the rest of round `r + 1`.
- **A seed is never on chain without its request.** Anyone can preview ORAO's answer to a known
  seed on devnet. A seed committed before its request would leave whoever sends the request (or
  doesn't) the choice of drawing it once they know who wins.
  - `draw` commits the seed and asks ORAO for it in the same instruction. When the pot can't pay
    for the request (the breaker allows no request, ORAO's terms are unreadable or above the cap,
    the pot is short), the round rolls over at once (`OracleUnpaid`) and no seed is committed.
  - The client must pass the request's account, so it must know the seed: the draw names the slot
    it is made from, one of the last 3 slots (`oracle::SEED_SLOTS`) when it lands. A keeper reads
    the newest entry of the slot hashes sysvar (`fetchSeedSlot`) and sends the draw at once; if it
    lands more than 3 slots later it fails (`StaleSeed`) and is sent again from a newer slot.
  - The seeds of the newest slots are public, but nobody can learn ORAO's answer to one before the
    slot is too old: a preview needs a devnet request to land, then the three mainnet signers'
    responses on devnet, the last of which came 7 to 17 slots after the request in the 8 devnet
    requests measured on 2026-10-09 ("Response times, measured", below). So a preview arrives 8
    slots or more after the seed's slot, where the draw must land within 3: a margin of about 3 to
    5 slots, from a small sample. A pending request someone made for the seed is adopted (it was
    made blind); one ORAO has already answered is refused (`StaleSeed`). Were ORAO's devnet ever
    to answer within about 5 slots, the window would need to shrink; keepers should alert on it.
  - A draw is made at least 10 minutes (`REVEAL_SECS`) and a whole claim window before its claims
    end (`Game::last_draw`), or the round rolls over (`Late`): a draw in a round's last moments
    leaves its winner no time to claim, and only a sender racing every keeper would make it. ORAO's
    mainnet answer took up to 319 slots (about 2 minutes) in the 8 requests measured, so the 10
    minutes cover the slowest seen more than four times over.
  - Before this (the final audit and its residuals), a seed was committed by `draw` and requested
    by a later `request_randomness`. While the pot could not pay, a holder who previewed each
    answer paid ORAO only for the seeds it won; and a draw committed just before the last request
    time left its sender a free choice of requesting it or letting the round roll over.
- **The oracle's breaker.** While ORAO has not answered the last request the pot paid for
  (`Game.paid_seed`), the pot pays for a new one only for a draw 1, 2, 4, 8… rounds later, never
  more than 30 days apart. Before that, the draw rolls its round over at once (`OracleUnpaid`),
  commits no seed and costs nothing. While `paid_seed` is set, its request account must be passed
  to `draw` (leaving it out is refused). A silent ORAO costs about 11 requests over 60 days of
  hourly rounds (0.055 SOL), where it cost one a round (7.2 SOL) before. ORAO's quorum is all
  three of its signers, so one offline stops every answer while its program still takes requests.
  One lost answer costs no draw: the next round pays again, and an answer to the last paid
  request, however late, resets the breaker. The breaker's round-3 error, `OracleUnanswered`
  (6045), is no longer returned; it is kept only so the error codes after it keep their numbers.
- **Dormancy.** A pot that has paid no prize for 30 days (or 4 rounds) is drawn from 0.1 SOL
  instead of `min_pot`. After twice that, anyone may `retire` it. No pot is locked for ever, under
  an audited hook too. The game goes on afterwards, with its clock restarted.
- **Claim races.** A winner whose claim doesn't land in its window loses to a later attempt, and
  whoever holds a later ticket could stuff the claims' write locks for a window. So claim with a
  priority fee scaled to the bounty, and resubmit.
- **The lottery's steps check `Game.kind`,** so a kind added later can't be run through them.

## HookStatus: audited, the pot cap, blocked

`HookStatus` lives at `PDA(["hook-status", hook])`: 124 bytes, 1,280,160 lamports of rent. It is
written only by `set_hook_status`, signed by the companion program's upgrade authority, which is
read from its ProgramData. Without a status, a hook is not audited, its pots are capped at 10 SOL,
and it is not blocked.

- **`audited`**: the pots aren't capped, and the audit clears a block. An audit is final: a status
  once audited stays audited, so an audited hook can never be blocked, not even by un-auditing it
  first.
- **`pot_cap`**, while not audited: 0.1 to 10 SOL. The protocol may lower it, never lift it above
  10 SOL without an audit. A fee claim sends the pot share above the cap to the buyback. A cap
  lowered later trims the pot at the next step, and a pot full at its cap is drawn at the cap.
- **`blocked`**, only for a hook not audited, and lifted only by an audit. Every game of the hook
  then sends its pot and its pot share to the buyback and ends any draw (`RolledOver`, `Blocked`).
  Nobody, Bordrless included, receives anything.
  - If the hook also refuses the companion's buyback, `burn_stranded` burns that buyback as SOL
    once, for 30 days (or 4 buyback intervals), no buyback has bought or waited.
  - Each landed buyback restarts the wait. So does each buyback that waits and moves the
    reference price (once an interval, while the price is more than 3% above it): a working hook's
    buyback that waits for its reference to catch up with a risen price is never burned. The burn
    comes only once buybacks are tried at the reference and still land nothing.
  - Each burn restarts the wait too, and so does a fee claim that credits the buyback at least what
    it held. Once a working hook's buybacks have spent everything there is nothing to buy, so
    nothing buys for a while and the wait runs on; the next claim credits an empty buyback (or one
    holding dust), and its share gets its whole 30 days, even when the claim shares a transaction,
    or the keeper's pass, with a burn. (Until the residuals' verification such a share was burned
    at once, by the protocol's own keeper too.)
  - That can't put a refusing hook's burn off for ever. Its buyback never empties, so only a claim
    crediting at least everything it holds restarts the wait, and only fees paid to the game (or
    bridged SOL given to the creator address) are credited, all of it burned with the rest. Keeping
    `v` from the burn for one more wait takes a credit of at least `v`, and the next at least `2v`.
    Credits smaller than what the buyback holds (a griefer's dust trade or donation, fees as they
    come in) restart nothing.
  - A blocked game's pot gets its own 30 days in the buyback: whichever call moves it there (a
    game step, a fee claim, or `burn_stranded`, which then burns nothing) restarts the wait. So the
    pot of a quiet coin blocked while its game is idle is never burned by the call, or the
    transaction, that moves it, and a working hook's buyback has 30 days to spend it. The
    protocol's keeper moves such a pot as soon as it sees the block (`retire`, or `expire`
    mid-draw: every game step moves it under a block).
  - A keeper away for 30 days can't be told from a hook that refuses: keepers crank the buyback
    under a block as before.
- `set_hook_status(lottery_hook, audited)` belongs after the audit. It isn't needed for
  `create_game`, only to lift the 10 SOL cap.

## Randomness: ORAO VRF

Mainnet, read through the public RPC on 2026-10-08 (slot 454,671,549):

| | Value |
|---|---|
| Program | `VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y` (classic, not the callback variant). Upgradeable by one key, `9ZTHWWZDpB36UFe1vszf2KEpt83vwi27jDqtHQ7NSXyR` |
| Network state | `5ER1oENnV4srxYdAynUfRzWeQCPQaqMiAp4VqyMbSqnK`. Fee at bytes 72..80: **500,000 lamports (0.0005 SOL)**. Treasury `9ZTHWW…`. Three fulfilment authorities, all required |
| Rent | 749 bytes (a pending request): 4,455,160 lamports. 137 bytes (fulfilled): 1,346,200. A system account: 650,240 |
| A request, up front | 4,955,160 lamports: the fee plus the pending account's rent |
| Refunded on the answer | 3,108,960 (the 612 freed bytes), to the oracle payer, which uses it for the next request |
| Net per answered draw | **1,846,200 lamports (0.00185 SOL)**. The 137-byte account's rent stays locked: ORAO has no instruction that closes it |
| The companion's cap | It refuses a fee above `MAX_REQUEST_FEE` (0.005 SOL): each draw then rolls over at once with no seed committed (`OracleUnpaid`), and the pot is eventually retired |

**Response times, measured** on 2026-10-09 through public RPC, read-only, from the request's slot
to the slot of the last of the three mainnet signers' responses. The samples are small: they
support `SEED_SLOTS` (3) and `REVEAL_SECS` (600 s), not a bound on ORAO's future speed.

| | Requests | Slots to the last response |
|---|---|---|
| Mainnet, classic VRF | 8 | 7, 15, 19, 23, 234, 250, 254, 319 (about 3 s to 2 minutes; one signer, `A9iZdg…`, often lags by about 100 s) |
| Devnet, classic VRF: the preview path (its network state lists the three mainnet signers and a fourth) | 8, and a ninth still unanswered when read | 7, 8, 8, 8, 8, 8, 9, 17; the first response came 3 to 6 slots after the request |

- The figure these docs gave before, 4 to 5 slots on mainnet, came from 4 requests on 2026-10-08.
  It doesn't hold: mainnet often takes 1 to 2 minutes. `REVEAL_SECS` still covers the slowest
  answer seen more than four times over.
- A devnet preview of a slot's seed arrives at least 8 slots after that slot (1 for the devnet
  request to land, then 7 or more), and a draw must land within 3 slots of it (about 5, should the
  seed slot's leader hold its block for the next leader's grace ticks): a margin of about 3 to 5
  slots. Keepers should alert if devnet ever answers within about 5 slots.
- ORAO's callback VRF (`VRFCBe…`) signs with the same three keys and answered mainnet requests 6 to
  7 slots after them, but devnet ones thousands of slots after, and it signs a 64-byte message, not
  the 32-byte seed: it is no faster preview path. Testnet, Eclipse, Sonic, SOON and Fogo have no ORAO
  network state at its address.

- **ORAO's deprecated v1 `request` still works**, at the same address as a v2 request for the
  same seed. So anyone may request the seed of one of the newest slots before a draw names it.
  The draw adopts a pending one: it was made blind, its answer equals the v2 one, and ORAO answers
  v1 today. Refusing it would make every such squat a sure failure of that draw.
  - If ORAO stopped answering v1, each squat would cost its round, with no new seed: a new seed
    would let a squatter who previews answers on devnet pick among draws.
  - A game squatted for months would have its pot retired.
  - Keepers should watch for v1 squats.
- **Layouts are bound by length.** A pending v2 request is 749 bytes, a fulfilled one 137, a v1
  one 780 (every ORAO account on mainnet). A layout changed under the same type name fails closed
  (`OracleAccount`, or `OracleUnreadable` at `expire`). A network state the companion can't read
  rolls each draw over (`OracleUnpaid`).
- **A change to ORAO's request instruction** that made the pot's own request fail would fail every
  draw with it: no seed is committed, so none is left open. Keepers should alert on failing draws;
  the companion would need an upgrade.
- **Trust, said plainly.** ORAO's signers could withhold an answer (the round rolls over) or answer
  so late that only the first attempts fit. The last of them to sign could bias it: ed25519
  signatures aren't unique, and the last signer sees the others'. They can't choose the round's
  tickets or the seed. The token page says "Randomness by ORAO VRF: ORAO's three signers produce
  it. They could withhold or bias a draw; Bordrless can't."

## Limits, measured

| Limit | Value |
|---|---|
| Setup transaction (`create`, `prepare`, `create_game`) | 765 bytes, v0 with the protocol's 22-address table |
| The launch through the companion | 1,177 bytes with the longest metadata in the tests (a 128-byte URI); 1,142 at the site's Pinata URI. 35 keys, 40 trace entries, stack height 5, 328,180 CU |
| A game hook's registry | At most 3 extras besides the launch (`MAX_GAME_HOOK_EXTRAS`). At the site's URI, state + launch + 2 more is 1,208 bytes; state + 3 is 1,240, over the limit |
| The steps (v0, 22-address table) | `draw` (commit and request) 731 bytes, 764 with the paid request (1,060 with no lookup table); `reveal` 469; `claim_prize` 586; `expire` 469; `claim_fees` 530; `buyback` 886; `dev_buy` 891; `release` 648 |
| Compute | `draw` 155k to 160k CU (it unwraps the top-up, calls ORAO and pays the bounty; 36k when it rolls over, 47k when it adopts a request), `claim_prize` about 125k, `reveal` 25k: each fits the default 200k a transaction gets. A game coin's `dev_buy` and `buyback` (swap and burn through the hook) take about 260k to 390k CU: send them with a compute-unit limit (450k covers the heaviest a game hook may be; the protocol's keeper simulates) |
| Heap | `draw` (commit and request, after an unanswered paid request) peaks at 12,494 bytes of the 32 KiB heap, `claim_prize` at 11,180, `burn_stranded` at 6,432 (measured with an instrumented allocator); a game coin's buyback at about 22 KiB (a kit buyback, 18 KiB). v1's peak was about 32.5 KiB, fixed in v2 by building errors only when an account is missing |
| Program sizes | The companion `.so` is 590,976 bytes (v0 build); its mainnet ProgramData holds 339,416, so it needs `solana program extend` by about 251,560 bytes (about 1.28 SOL at 5,080 lamports per byte) before the upgrade. `lottery_hook.so` is 234,704 bytes (its ProgramData about 1.19 SOL). Re-measure with the deploy build (`programs.sh`, `--arch v3`) |
| Accounts a game adds | `Game` 486 bytes, the hook's state 322 and registry 114 (paid by the launcher); `HookStatus` 124 (paid by the protocol); one 137-byte ORAO account per answered draw. `Game` has no close instruction |

## Failure modes

| What goes wrong | What happens |
|---|---|
| Nobody held tickets in the round | `NoTickets`: rolls over, the pot keeps growing |
| The draw lands in a dead range, or the winner sold | The next attempt opens after the window; after the last, `NoClaim` |
| The winner traded in the round after | Their range moved to the previous slot: they still claim |
| Nobody drew in time | `Late`, or the round is skipped (a draw is only for the round that just ended) |
| ORAO silent | The seed waits until the claims end, then `OracleSilent`. The breaker limits what new requests cost: while it holds, each draw rolls over at once (`OracleUnpaid`), no seed committed |
| ORAO's fee above the cap, or its network state unreadable | Each draw rolls over at once (`OracleUnpaid`), no seed committed; the pot is retired after 60 days |
| ORAO's layout changed | `OracleUnreadable`: rolls over at once, no new request |
| Someone requested the seed first (v2 or v1, before the draw landed) | Adopted, for free, while pending and while the pot could pay for its own (else the round rolls over, `OracleUnpaid`). Already answered: refused (`StaleSeed`), and the draw is sent again from a newer slot |
| No prize for 30 days | Dormant: drawn from 0.1 SOL. After 60 days, `retire` sends the pot to the buyback |
| A rigged or refusing hook | Capped at 10 SOL while not audited; the protocol may block it (pot and share to buyback and burn); a buyback that has neither bought nor waited for 30 days is burned as SOL |

## Not in phase 1

- Jackpot and streak games (`GameKind` and `Game.reserved` leave room for them).
- Author shares for listed game configs (decision d).
- Studio game hooks need a `set_hook_status` from the protocol each (or a companion upgrade), since
  `create_game` takes only the lottery hook or a vetted hook.
- Bounties for `reveal` and `expire` (they pay nothing; the keeper earns on `draw`, on the
  oracle's top-up, and on `claim_prize`).
- A way to close a `Game` and get its rent back.

## For integrators

The TypeScript SDK (the monorepo's `packages/sdk`) mirrors every builder, account and rule here.
It is held byte for byte to the Rust clients by `programs/tests/vectors/companion-games.json`,
which `programs/tests/tests/game_vectors.rs` renders from the Rust reference, rewriting the file
and failing whenever it is stale.

- `companion.ts`: `createGame`, `draw`, `reveal`, `claimPrize`, `expire`, `retire`,
  `burnStranded`, `setHookStatus`, and the game's clocks (`gameLastDraw` and
  `companionStrandedAt` among them).
- `lotteryHook.ts`: `prepare`, `enter`, `accounts`.
- `game.ts`: the header and slot decoders, `drawIndex`, `findWinningHolding`.
- `orao.ts`: the network state, request reading, `drawSeed`, and the slot a draw names
  (`decodeSeedSlot`, `fetchSeedSlot`: the slot hashes sysvar's newest entry).

A keeper's loop, each round (the monorepo's `apps/server/src/keeper/games.ts` runs it every two
minutes for every launched game companion):

1. `enter` every holder not written this round, late in the round (its last tenth, at least 15
   minutes before it ends): a holding entered early and then sold from adds dead tickets.
2. `claim_fees` with the hook's status.
3. Once the round ends: `draw`, naming the newest slot hash and passing `Game.paid_seed`'s request
   while it is set. It commits the seed and requests it at once, and must land within 3 slots of
   the slot it names, which is already a slot old when read. So fetch the blockhash (and the lookup
   table) first, then read the slot at `processed` (`fetchSeedSlot`), then build, sign and send at
   once, with no dry run, no preflight and no rebroadcast (a resend can only land too late and pay
   a fee). If it fails with `StaleSeed` (it landed more than 3 slots after that slot), or isn't
   seen to land within a few seconds, send it again from a newer slot (the protocol's keeper tries
   3 times a pass). A draw the pot can't pay for rolls over by itself.
4. `reveal` as soon as ORAO answers.
5. `claim_prize` for the winner found among the mint's holdings, in its window, with a priority
   fee.
6. `expire`, `retire` or `burn_stranded` when due (`companionStrandedAt`, read again after the
   pass's fee claim, which may have restarted the wait). Under a block, while the pot holds
   anything, `retire` (or `expire` mid-draw) at once: it moves the pot to the buyback, which a
   working hook's buyback then spends.

The token page reads `Game`, `Companion.pending_pot`, the hook's header and the holder's slots.
