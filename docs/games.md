# Games: coins run by their companion (lottery, jackpot, streak)

Status: phase 1 (the lottery) is **live on mainnet** since 2026-10-09 (companion v2
`4e6dbc68…`, `lottery_hook` `c5d50008…`). Phase 2 (the last-buyer jackpot, the diamond-hands
streak and Studio game hooks, "Phase 2" below) is built and tested in this repo, **not deployed**
(2026-10-09). The monorepo's `docs/studio-companions.md` ("audited purse, Studio rules") is the
design. The pieces:

| Piece | Where | What it is |
|---|---|---|
| `bordrless_companion` v2 (v2.1 with phase 2) | `programs/bordrless_companion`, same id `6ZUM1gWBH9hBBNoJoaVAGwSftyZ6CUda6vUZTW9MsJuo` | An additive upgrade. It holds the pot, draws and pays. Every v1 instruction, account, event and error code is unchanged; phase 2 adds instructions and kinds and leaves every phase-1 instruction byte-identical. |
| `lottery_hook` | `programs/lottery_hook`, `HqFWsCBQ416DAfevJ9TspyT5yXGGoYTCpcreiGkCgWcr` | A lottery coin's token hook: it keeps who holds tickets. It never sees SOL, never refuses a transfer and takes no cut. Unchanged by phase 2 (its rebuild with the phase-2 crate hashes to the deployed `c5d50008…`). |
| `bordrless-game` | `crates/bordrless-game` | The game ticket standard: the header and slot layouts, the rules a hook applies, and the helpers the companion reads with. Phase 2 adds the jackpot's and the streak's headers and rules, a launch reader, a holding reader and `write_own_hook_data`; every phase-1 offset and rule is unchanged. |
| Studio's game starters | `programs/tests/fixtures/starters/{jackpot,streak}` | Test-only reference hooks written only on the crate: what Studio's assistant starts from. Bordrless deploys none of them; Studio deploys a copy per coin under its key. |

Randomness (the lottery's only) comes from ORAO VRF (`VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y`).
The companion only reads the hook's state and the holdings' hook data; it never calls the hook.
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
| A game hook's registry | At most 2 extras besides the launch for `create_game_v2` (any kind), checked again at a jackpot's or a streak's launch (`MAX_GAME_HOOK_EXTRAS_V2`); phase 1's `create_game` (a lottery, as deployed) keeps 3 (`MAX_GAME_HOOK_EXTRAS`). At the site's longest name and URI, state + launch + 2 more is 1,208 bytes; state + 3 is 1,240, over the limit. Any registry the companion reads (a hook-owned account) is checked against bounds before it is decoded (`MAX_REGISTRY_*`: 1,024 bytes, 8 accounts, 16 seeds a PDA, 32 bytes a literal seed), so none can exhaust the heap |
| The steps (v0, 22-address table) | `draw` (commit and request) 731 bytes, 764 with the paid request (1,060 with no lookup table); `reveal` 469; `claim_prize` 586; `expire` 469; `claim_fees` 530; `buyback` 886; `dev_buy` 891; `release` 648 |
| Compute | `draw` 155k to 160k CU (it unwraps the top-up, calls ORAO and pays the bounty; 36k when it rolls over, 47k when it adopts a request), `claim_prize` about 125k, `reveal` 25k: each fits the default 200k a transaction gets. A game coin's `dev_buy` and `buyback` (swap and burn through the hook) take about 260k to 390k CU: send them with a compute-unit limit (450k covers the heaviest a game hook may be; the protocol's keeper simulates) |
| Heap | `draw` (commit and request, after an unanswered paid request) peaks at 12,494 bytes of the 32 KiB heap, `claim_prize` at 11,180, `burn_stranded` at 6,432 (measured with an instrumented allocator); a game coin's buyback at about 22 KiB (a kit buyback, 18 KiB). v1's peak was about 32.5 KiB, fixed in v2 by building errors only when an account is missing. The companion never copies a hook's account onto the heap: it reads the hook's state where it lies (only the headers' bytes, 0 to 592), so a hook's state may be any size, up to the runtime's 10 MiB (tested at 10 MiB) |
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

- Jackpot and streak games: phase 2, below.
- Author shares for listed game configs (decision d): still refused.
- Studio game hooks needed a `set_hook_status` each: phase 2 takes one only Bordrless can upgrade
  without it.
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

## Phase 2: the jackpot, the streak, Studio game hooks

Built and tested 2026-10-09, not deployed. Owner-approved scope: a last-buyer jackpot and a
diamond-hands streak in the companion (the monorepo's `docs/studio-companions.md` §1, examples 2
and 3), Studio game hooks taken without a status when only Bordrless can upgrade them, the
standard's helpers for both kinds, and reference starter hooks. Neither new kind uses randomness.

### In plain terms

- **Jackpot.** Every buy of at least `min_tokens` on the coin's own pool restarts a countdown.
  When it runs out, the last buyer is paid a share of the pot in SOL, if they still hold
  everything they bought and have sent nothing since. Buys count while the coin is on its bonding
  curve; the jackpot ends at graduation (the round under way is still paid).
- **Streak.** Each epoch (a week, say), part of the pot is shared among the holders who held
  through the whole epoch without sending a single token, in proportion to what they held. Sending
  anything (selling, or to another wallet, yours included) forfeits your share; what is forfeited
  or not claimed stays in the pot for the next epoch.
- **Studio game hooks.** A coin's game hook written in Studio and deployed under Studio's key is
  accepted with no vetting step: Bordrless can upgrade it (to one that issues nothing) or block it.
  It is not audited, so its pot is capped at 10 SOL.

### The owner's decisions (phase 2, 2026-10-09)

Each is a choice someone could make otherwise; the alternatives are noted.

- **(p2-a) The jackpot counts buys only on the bonding curve.** On the curve the DEX refuses
  liquidity (`CurveLocked`), so a transfer out of the launch pool to a wallet is a buy. Once
  graduated, anyone can add liquidity and take it out again: the removal is a transfer out of the
  pool that no token hook can tell from a buy, so it would be a free qualifying "buy" (free timer
  resets, a cheap last buy). After graduation the hook records no new round; the pot keeps its
  share of fees and, paying no prize, is retired to the buyback every 60 days. The alternative, a
  post-graduation jackpot, needs the DEX or the launchpad to mark swaps (a change to deployed
  programs, out of scope).
- **(p2-b) A buy after the timer ran out never takes the winner's place.** The hook keeps the
  current round and the last 8 rounds that ended (`JACKPOT_ENDED_ROUNDS`); `settle` pays the
  oldest open round first, one a transaction. A round that ended is dropped only once 8 later
  rounds have ended too, each a whole timer with no qualifying buy: counted from when it can first
  be settled (its timer ran out), a winner who holds stays payable for at least 8 timers (40
  minutes at the 5-minute floor, 80 for the starter's 10), however busy the coin, and a round
  dropped is never paid to a later buyer in its place (the later rounds are paid in their own
  order). Before the independent audit the hook kept one ended round, so this was one timer (5
  minutes at the floor). The protocol's keeper settles jackpots in their own loop every 15 seconds
  or so, before any other work. A round left unsettled 30 days is forfeited (`Stale`).
- **(p2-c) A jackpot round is closed as soon as it is settled, funded or not.** `settle` never
  waits: it pays `prize_bps` of the pot when the pot holds the game's minimum (or its cap when
  lower, or 0.1 SOL once dormant), and otherwise closes the round paying nothing
  (`JackpotUnfunded`; the pot stays). So a winner is paid what the pot can pay when the round is
  settled, and a round waiting for its pot can never be overtaken by the next one (round 1 of the
  audit: a round left open on `PotTooSmall` was overwritten). While the launch holds creator fees
  nobody has claimed that could fund the prize (the pot's part of them, counted before any bounty
  or cap), a bare `settle` is refused (`FeesUnclaimed`): the sender claims the fees and settles in
  one transaction (682 bytes with the table), so nobody can void a prize by settling first (round
  2). A prize below an empty wallet's rent-exempt minimum pays nothing either (the transfer could
  not land). The launch page's default pays 50% of the pot
  (`JACKPOT_DEFAULTS`), so the pot is never empty after a prize.
- **(p2-d) "Still holds what they bought" means no send at all since the buy.** A holding's mark
  is the number of its first qualifying buy since it last sent anything (cleared by any send, a
  burn or a delegate's transfer included); the buyer of round `n` wins if its mark is `n` or
  earlier and its balance is at least the amount. Selling and buying back (below the minimum, so
  without a new round) does not restore it. A buyer who no longer qualifies forfeits the round
  (`JackpotForfeited`); the pot stays. So does a buyer whose address can't be paid (an executable
  account, an account the sysvar program owns, or one of the runtime's 31 reserved keys, read-only
  in every transaction: a program's id or a sysvar's can be on the curve, so the hook takes it for a
  wallet), and so does any round left unsettled 30 days (`SETTLE_GRACE_SECS`) after its timer ran
  out, whatever its buyer: no round can jam the ones after it, or `retire`, for ever (rounds 1 and 2
  of the audit). A buy and a sell in one transaction take the round and forfeit it: anyone can
  void a round for a round trip's fees on `min_tokens` (inherent: the hook can't tell a holder from
  a flipper until the sell).
- **(p2-e) The jackpot's knobs.** The timer is 5 minutes to 30 days (5 minutes makes stuffing the
  last blocks expensive). `min_tokens` is any amount of at least one base unit: the hook can't see
  SOL, so the minimum buy is in tokens and its SOL cost rises with the price. The starter uses
  1,000,000 tokens (0.1% of the supply).
- **(p2-f) A streak's weight is what was held since the epoch began, judged at the epoch's end.**
  A holding registers at its first write of the epoch (a receive registers what it held before;
  `enter` its balance), only if it holds at least `min_weight` and, by the epoch's end, will have
  sent nothing for `min_streak_secs`. Since `since` only moves with a send, which forfeits, the
  verdict does not depend on when the holding is written, and nobody can lock a holder out by
  entering it early (`enter` writes only a weight that registers). Tokens that arrive during an
  epoch count from the next one.
- **(p2-g) Any send forfeits the epoch's share and the last epoch's unclaimed share.** A holder
  must claim before sending anything. The epoch's total is final when the epoch ends, so a forfeit
  after it ends leaves its share in the pot (it rolls over).
- **(p2-h) A closed epoch's pot is locked for its holders.** `close_epoch` fixes `prize_bps` of the
  pot as the epoch's pot and locks it (`Companion.pot_locked`): a cap the protocol lowers
  afterwards never trims it (the pot may sit above the cap by the locked part until the claims
  end). A block takes it with the rest: no prize is paid under a block.
- **(p2-i) Claims make receipts.** `claim_share` makes `PDA(["claimed", game, epoch, owner])`,
  paid by the sender, which refuses a second claim; `close_receipt` returns its rent (1,767,840
  lamports) to that sender once the epoch's claims end. `close_epoch` pays no bounty (it moves
  nothing); `claim_share` and `settle` pay `bounty_bps` of what they pay.
- **(p2-j) Studio hooks are vetted by who can upgrade them.** `create_game` and `create_game_v2`
  take a hook whose ProgramData (passed as a remaining account, at `PDA([hook], loader)`, owned by
  the upgradeable loader) names Studio's key `CS1NRyXNCPxEUP4CRoa26cHQSeSJCxXh5SPijwFhDW6W` or the
  protocol's `5xsibKwtiN6ruxsYrEyWVpV3KcwuzSPbQd1n28a7spEd` (the launchpad's
  `HOOK_UPGRADE_AUTHORITIES`), with no status. An immutable hook (other than `lottery_hook`), or one
  under loader v4, still needs a `HookStatus`; one an outsider can upgrade is refused; a blocked one
  always. The check is made once, at `create_game`. Such a hook is not audited: its pots are capped
  at 10 SOL. Trust, said plainly: Studio's key could upgrade a game hook to rig its tickets;
  Bordrless holds that key, as it holds the power to block.
- **(p2-k) Every kind runs the same callbacks.** `before_transfer`, `before_burn` and writing hook
  data (flags 145, `GameKind::hook_flags`); the launch refuses any other set. The companion keeps
  the game's kind (`Companion.game_kind`) so the launch can check them without the `Game`.
- **(p2-l) A new instruction for the new kinds.** `create_game` stays phase 1's (a lottery,
  byte-identical); `create_game_v2(args, kind)` makes any kind, taking the kind's settings
  (`GameKindArgs`) after `CreateGameArgs`.
- **(p2-m) No pot is locked for ever, for every kind.** `retire` (60 days, or 8 rounds or
  epochs, with no prize) applies to the jackpot and the streak too: a jackpot whose rounds never
  end (buys keep coming) or never pay, or a streak nobody claims, sends its pot to the buyback. It
  waits (`DrawPending`) while a streak epoch's claims are open, and, for these kinds (which pass the
  hook's state and the fee holdings, `retire_game`), while a jackpot round over and unsettled (and
  not yet 30 days stale), or the streak epoch that just ended with weight, could be paid now, a fee
  claim included: no `retire` can front-run a `settle` or a `close_epoch` (rounds 1 and 3 of the
  audit). A state the hook broke owes nothing (retire goes on).
  Said plainly: a streak whose epochs keep closing with weight is never retired (its holders can
  always claim; unclaimed shares roll over); and a `retire` sent while a jackpot's timer is still
  running moves the pot, as a lottery's round in progress can be retired.
- **(p2-n) The starters' settings are their code's.** A Studio game hook is one program per coin,
  so its timer, minimum buy, epoch, minimum streak and weight are constants, and its `prepare` is
  Studio's standard one (anyone may send it, once per mint; it chooses nothing).
- **(p2-o) The Studio template gains the crate.** `tools/studio-builder/template` depends on the
  vendored `bordrless-game`. The dependency graph is part of every build, so every Studio hook's
  hash changes (the `sell-tax` starter: `26cdae4c…` before, `9a474f99…` now); a hook deployed
  earlier verifies against its own commit's template. The alternative is a second template for game
  projects.
- **(p2-p) Smaller trade-offs, documented.** After graduation the jackpot's pot still takes its
  fee share and reaches the buyback through `retire` (every 60 days); the buy that crosses the
  graduation threshold holds the last round uncontested. A streak's epoch pot is `prize_bps` of the
  pot when `close_epoch` runs (the keeper closes at the epoch's start, after its fee claim); a late
  close leaves as little as a claim window for claims. Holders a keeper never enters get no weight
  (anyone may enter anyone). A share below the rent-exempt minimum can't reach a wallet with no SOL.

### Who does what

| | Sees | Does | Can't |
|---|---|---|---|
| A jackpot hook | Every transfer and burn; the `Launch` account (its pool and its status) | Counts qualifying buys, names the last buyer, keeps the last 8 rounds that ended, marks each buyer's holding, clears a mark on any send | See SOL, refuse a transfer, take a cut, call anything |
| A streak hook | Every transfer and burn | Rolls the epochs, registers weights, subtracts a sender's weight from the total, forfeits a sender's last epoch | The same; its `enter` calls only the token program's `write_hook_data` |
| The companion | The hook's header and kind header, the holdings (read only) | Settles a jackpot round (pays or forfeits), closes an epoch and locks its pot, pays each share once | Call the hook, pay anyone but a round's buyer, an epoch's holder, a step's sender (`bounty_bps`) or the buyback |

### The standard, phase 2 (`bordrless-game`)

Each kind's header sits right after the base header, at offset 120 (where a lottery hook's own
fields start), with its own magic; the companion reads it only for a game of that kind. Integers are
little-endian.

**The jackpot header** (`JackpotHeader`, `jackpot_offsets`, 472 bytes). The base header's
`last_buyer`, `last_amount` and `last_buy_at` (offsets 72, 104, 112) are the current round's last
qualifying buy; a jackpot hook's `round_secs` is 0 (no rounds).

| Offset | Field | Type | What |
|---|---|---|---|
| 120 | `magic` | `[u8; 4]` | `BRJ1` |
| 124 | `timer_secs` | `u32` | A round ends this long after its last qualifying buy |
| 128 | `min_tokens` | `u64` | The least a qualifying buy delivers |
| 136 | `buys` | `u64` | Qualifying buys so far: the current round's number |
| 144 | `ended_buyer` | `Pubkey` | The round that ended last: its buyer… |
| 176 | `ended_amount` | `u64` | …what they bought… |
| 184 | `ended_at` | `i64` | …when… |
| 192 | `ended_buys` | `u64` | …and its number (0: none) |
| 200 | `earlier` | `[EndedRound; 7]` | The 7 rounds that ended before it, newest first, 56 bytes each: `buyer` (+0, `Pubkey`), `amount` (+32), `at` (+40), `number` (+48; 0: none) |
| 592 | | | The hook's own fields |

A buy that ends a round moves it to `ended_*`, the older ones shifting one down `earlier` and the
oldest dropped (`JackpotHeader::push_ended`). The companion reads the remembered rounds as a chain,
newest first: each numbered below the round after it and over by the timer before that round's
last buy (a hook under the standard leaves nothing else); it ignores whatever follows a break.

A holding's **mark** is the first 8 of its slot's free bytes (48..56): the number of its first
qualifying buy since it last sent anything; 0 after any send.

**The streak header** (`StreakHeader`, `streak_offsets`, 16 bytes). The base header's rounds are
the epochs (`round_secs` is the epoch's length) and its totals are exact weight totals.

| Offset | Field | Type | What |
|---|---|---|---|
| 120 | `magic` | `[u8; 4]` | `BRS1` |
| 124 | `min_streak_secs` | `u32` | By the epoch's end, a holding must have sent nothing for this long (at most a year) |
| 128 | `min_weight` | `u64` | The least weight that registers |
| 136 | | | The hook's own fields |

A streak holding's slots are the standard's: the current slot is the epoch it was last written in
and its weight that epoch (`start` 0: no ranges), the previous slot the epoch before, `since` the
last send (or first receive).

**The rules** (pure functions that never panic, fuzzed against a plain model in
`tests_phase2.rs`: 200 random walks per kind, each with coverage asserted):

- Jackpot: `qualifying_buy(launch, source_owner, destination_owner, amount, min_tokens,
  excluded)`; `jackpot_on_send` (mark cleared, `since` set; a holding emptied is cleared);
  `jackpot_on_receive`; `jackpot_on_buy` (a round whose timer ran out moves to `ended_*`, the older
  ended rounds shifting down `earlier`; the buy
  becomes the current one; the buyer's mark set unless it holds one); `settle_round(header,
  jackpot, paid, timer, now)` (the oldest round over and not settled: the oldest remembered ended
  round later than `paid`, else the current one once over); `jackpot_winner_holds`.
- Streak: `streak_qualifies(since, epoch, epoch_secs, min_streak)` (`since > 0` and `since <=
  epoch_end - min_streak`); `streak_on_send` (the epoch's weight leaves the total; the previous
  epoch's is forfeited; `since = now`); `streak_on_receive`; `streak_on_enter` (writes only a weight
  that registers); `streak_weight(hook_data, epoch, …, balance)` (what a claim may take: at least
  the floor, at most the balance, still qualifying); `share_of(pot, weight, total)` (rounded down;
  never above the pot).
- `read_launch` reads a launch's pool and status at fixed offsets (10, 74, 138) after the
  launchpad's discriminator; `read_holding` a token holding (Borsh, after its discriminator);
  `write_own_hook_data` is a game hook's one CPI (`enter`'s): the token program's `write_hook_data`,
  signed by the hook's `["hook-authority"]`, with the token program and its event authority checked.
  The programs' tests hold each to the deployed programs' layouts (discriminators, offsets, the
  token client's own `write_hook_data`).

### The companion's new steps

**`create_game_v2(args, kind)`** (before the launch, the mint signing, after the hook's
`prepare`). `CreateGameArgs` as for a lottery, then `GameKindArgs { timer_secs, min_tokens,
min_streak_secs, min_weight }`, each checked against the hook's kind header:

| Kind | `round_secs` | `claim_window_secs` | `max_attempts` | Kind settings |
|---|---|---|---|---|
| Lottery | 1 h to 30 days | 5 min to a day | 1 to 16, within half a round | all 0 |
| Jackpot | 0 | 0 | 0 | timer 5 min to 30 days, `min_tokens` ≥ 1; the hook's header with no buy yet |
| Streak | the epoch: 1 h to 30 days | the least a close leaves for claims: 5 min to a day, at most half an epoch | 0 | `min_streak_secs` ≤ a year, `min_weight` ≥ 1 |

`min_pot` (0.1 to 1,000 SOL) and `prize_bps` (10% to 100%) are as for a lottery: the share of the
pot a settle, or an epoch, pays. Accounts as `create_game`'s, plus the hook's ProgramData. It emits
`GameCreated` and `GameKindSet` (the kind's settings and the hook's terms at creation).

**`settle`** (anyone; `ClaimPrize`'s accounts with the round's buyer's holding; remaining: the
hook's state, the buyer, writable, and the bridge's `unwrap_sol` accounts):

1. Under a block: the pot to the buyback, nothing else (`PotToBuyback`).
2. The oldest round over and not settled (`settle_round` with `Game.paid_buys` and
   `Game.timer_secs`); none: `NotDue`. The holding passed must be its buyer's (`WrongHolding`).
3. The buyer (its account must be passed; the launch's fee holding too) still holds (payable:
   not executable, not the sysvar program's, not a reserved key; the round not 30 days stale; a
   token holding of the
   mint, at the buyer's address, an eligible wallet, its mark the round's buy or earlier, its
   balance at least the round's amount, which is at least the game's `min_tokens`): with the pot
   at its minimum it is paid `prize_bps` of the pot, less the sender's `bounty_bps` of it
   (`JackpotPaid`); below it, it is refused while a fee claim could bring the pot to its minimum
   (`FeesUnclaimed`: the pot's part of the launch's unclaimed fees and of any surplus in the
   creator's holding, before bounty or cap; claim them first, in the same transaction), else the
   round closes paying nothing (`JackpotUnfunded`; so too when an empty wallet could not take so
   small a prize). Either way `paid_buys` moves to the round.
4. Otherwise the round is forfeited: `paid_buys` moves to it, the pot stays (`JackpotForfeited`,
   with its reason: `NotHeld`, `Unpayable` or `Stale`).

**`close_epoch(epoch)`** (anyone, `GameStep`'s accounts; remaining: the hook's state), during the
epoch after `epoch`:

1. The epoch whose claims ended releases its lock (`EpochEnded`: what it did not pay rolls over),
   before the hook's terms apply, so a cap lowered while it was locked trims the whole pot.
2. Under a block: the pot to the buyback; nothing else.
3. `epoch` must be the one that just ended, not closed yet (`RoundNotOver`), with the pot at its
   minimum (`PotTooSmall`).
4. After `claims_end(epoch)` less a claim window: it rolls over (`Late`). Its total from the
   header: none or forgotten, it rolls over (`NoTickets`, `RoundForgotten`).
5. Else the epoch's pot is `prize_bps` of the pot, locked; claims open until `claims_end(epoch)`
   (the end of the epoch after it) (`EpochClosed`). `Game.round`, `total`, `prize`, `epoch_paid`
   and `status` (`Revealed` while open) hold the claim epoch.

**`claim_share(epoch)`** (anyone, for any holding; accounts: the companion's, the game's, the
hook's status, the launch, the holding, its owner (writable, paid), the receipt (made here, the
sender paying), the system program; remaining: the bridge's `unwrap_sol` accounts): only for the
epoch open for claims, checked first (`NoDraw`, `DrawLate`: under a block too, so no receipt is
ever made for another epoch); during its claims, the owner's weight (`streak_weight`, at least `min_weight`, at
most the balance, still qualifying; `NoShare` otherwise) earns `share_of(epoch_pot, weight,
total)`, at most what the epoch's pot still holds; paid to the owner less the sender's bounty
(`ShareClaimed`). Once per owner and epoch: the receipt's address is taken.

**`close_receipt`** (anyone): once `claims_end(receipt.epoch)` has passed, the receipt is closed
and its rent goes to whoever paid it.

**`launch`** checks a game's flags against its kind (`GameKind::hook_flags`, 145 for every kind)
and, for a jackpot or a streak, its kind header in the hook's state; `create_game` refuses a
lottery on a jackpot's or a streak's hook (a streak's weights all start at ticket 0). **`retire`**
releases a streak epoch whose claims ended, and waits (`DrawPending`) while one is open or while a
prize is due (p2-m); a jackpot's or a streak's `retire` passes the hook's state and the launch's
and the creator's bridged-SOL holdings (`client::retire_game`).

**The layout.** `Companion.reserved` (10 bytes) becomes `game_kind` (1: `Lottery`, 0, in every
companion made before), `pot_locked` (8) and `reserved` (1). `Game.reserved` (83 bytes) becomes
`timer_secs` (4), `min_tokens` (8), `paid_buys` (8), `min_streak_secs` (4), `min_weight` (8),
`epoch_paid` (8) and `reserved` (43); every game made before reads them as zeros. `ShareReceipt`
is new (126 bytes). Errors 6049 `WrongGameKind` and 6050 `NoShare` are appended. Events:
`GameKindSet`, `JackpotPaid`, `JackpotUnfunded`, `JackpotForfeited`, `EpochClosed`, `EpochEnded`,
`ShareClaimed`. Error 6051 `FeesUnclaimed` too. The lottery's
steps still refuse other kinds (`NotAGame`), and the new ones refuse a lottery (`WrongGameKind`).

### Manipulation, considered

**Jackpot.**

| Attack | What happens |
|---|---|
| A whale wash-trades to fill the pot, then wins it back | The pot gets `pot_bps` of the creator fee; the whale pays every fee of the trade and must hold through the timer: it can't win back more than it pays |
| A sandwich or MEV bot back-runs buys to always be last | It must buy at least `min_tokens` each time, pay the fees, and hold until settled; the timer and the minimum are the knobs. Inherent to a last-buyer game |
| Dust buys to keep the timer alive | Below `min_tokens` they don't count |
| Buys through another pool, wallet-to-wallet transfers, the companion's own buybacks | Not from the launch pool, or to an excluded or off-curve owner: they don't count |
| Add and remove liquidity after graduation | Nothing counts after graduation (p2-a); a test removes liquidity from a graduated jackpot's pool and the header doesn't move |
| The last buyer sells (even one token) and buys back below the minimum | The mark is gone: forfeited (p2-d) |
| A bot buys right after the timer runs out, before `settle` lands | The ended round is kept and paid first (p2-b) |
| A round won by an address that can't be paid (a program's id) jams the rounds after it | Forfeited (p2-d): found in the audit's first round, fixed |
| A round waiting for its pot is overtaken when a later round ends | `settle` never waits (p2-c): found in the audit's first round, fixed |
| `retire` sent just before `settle` or `close_epoch` sweeps a prize that was due | `retire` waits while one is due (p2-m): found in the audit's first round, fixed |
| A round won by a sysvar (on the curve, not executable, read-only in every transaction) jams `settle` and, through the wait above, `retire` | Reserved keys forfeit, and any round 30 days stale forfeits (p2-d): found in the audit's second round, fixed |
| A bare `settle` sent before the fee claim voids a prize the unclaimed fees would fund | Refused while they could (`FeesUnclaimed`, p2-c): found in the audit's second round, fixed; the creator's surplus is counted too (third round) |
| A bare `retire` voids a prize the unclaimed fees would fund | `retire` counts what a fee claim would bring (p2-m): found in the audit's third round, fixed |
| A winner whose account can't take SOL for another reason (a legacy rent-paying account, before SIMD-0392) | Jams its round only until it is 30 days stale (third round, info) |
| Anyone settles with a wrong holding to forfeit a winner | The holding must be the buyer's own address; a winner who holds is paid, whoever sends `settle` |
| Stuffing the last seconds' blocks | The timer is at least 5 minutes |
| A hook that lies (names the dev, or ignores the minimum) | Not audited: capped at 10 SOL; Studio's fairness check and review; the protocol can block it. `settle` also checks the round's amount against the game's minimum |

**Streak.**

| Attack | What happens |
|---|---|
| Buy just before an epoch, hold through it, claim, sell | That is holding through the epoch: it shares, in proportion. `min_streak_secs` above the epoch's length asks more |
| Buy during the epoch, or receive from a friend | Counts from the next epoch: a weight is what was held since the epoch began |
| Split a balance across wallets | No gain; below `min_weight` a wallet shares nothing |
| A weight above the balance | Impossible under the rules (a weight is a balance held since the epoch began, zeroed by any send); claims check it (`weight <= balance`) |
| Self-transfer to "refresh" | Any send forfeits, to yourself too |
| Claim twice, or the same weight in two epochs | One receipt per owner and epoch; an epoch's weight is claimable only during the next epoch, and a holding remembers two epochs |
| Grief holders by claiming for them | The holder is paid; the sender pays the receipt's rent and gets it back after the claims end |
| Enter a holder early to lock it out | `enter` writes only a weight that registers, and the verdict doesn't depend on when it is written |
| Close late so holders can't claim | A close later than a claim window before the claims end rolls the epoch over (`Late`); the keeper closes at the epoch's start |
| A lowered cap mid-epoch | Never trims a closed epoch's pot (p2-h), and applies to the whole pot at the next close (fixed in the audit's first round) |
| A receipt planted under a block for a future epoch | Refused: `claim_share` checks the epoch before anything else (fixed in the audit's first round) |
| A hook that lies about weights or totals | No share exceeds what the epoch's pot holds (`pot_locked`); capped at 10 SOL while not audited |

### Studio game hooks

- **Auto-vetting** (p2-j). The tests take a jackpot and a streak starter upgradeable by Studio's
  key and by the protocol's, and refuse the same hook without its ProgramData, upgradeable by an
  outsider, immutable without a status, blocked, or with another program's or a forged
  ProgramData.
- **The starters** (`programs/tests/fixtures/starters/{jackpot,streak}`, test-only programs
  `Ay75xkGD…` and `9BmAknPD…`): written only on `bordrless-game` and `bordrless-hook`, each a
  Studio project's `src/lib.rs` as is (it builds in the monorepo's template). Studio's standard
  `prepare` (`[payer, mint, state, registry, system]`), the registry `[state (w), launch (r)]`,
  callbacks that answer hook data only (no CPI, no event, no lamports), and, for the streak, an
  `enter` (`[state (w), mint, holding (w), hook authority, token program, token event authority]`)
  whose one CPI is `write_own_hook_data`. Settings: jackpot timer 10 minutes, minimum buy 1,000,000
  tokens; streak epochs a week, minimum streak a week, minimum weight 100,000 tokens.
- **What Studio must add** is in the monorepo's `docs/games-handover.md` (phase 2): the `game`
  project kind, the starters, the `checks.ts` exception for `write_own_hook_data` outside
  callbacks, the simulator's game scenarios and fairness check, and the review prompts.

### Limits, measured (phase 2)

| | Value |
|---|---|
| Setup (`create`, the starter's `prepare`, `create_game_v2`), v0 with the 22-address table | 784 bytes (both kinds) |
| The launch through the companion with a starter hook, at the longest URI the tests send | 1,177 bytes, 40 trace entries, stack height 5, 335k (jackpot) to 340k (streak) CU; with 2 registry extras besides the launch at the site's longest name and URI, 1,210 bytes |
| `settle` / `close_epoch` / `claim_share` / `close_receipt` / the streak's `enter` (v0, table) | 643 / 465 / 614 / 290 / 393 bytes |
| Compute | `settle` 122k to 131k CU when it pays (35k to 37k when unfunded or forfeited), `claim_fees` and `settle` in one transaction 247k to 298k (682 to 694 bytes: send it with a compute limit; the keeper's fallback is 450k), `claim_share` 131.8k, `close_epoch` 25.9k, `retire_game` 21k to 38k (551 bytes), the streak's `enter` about 7k; a buy through a starter hook about 120k to 128k; a transfer about 40k |
| Heap (instrumented allocator) | `settle` 11,650 bytes, `claim_share` 11,674, `close_epoch` 4,614, of 32 KiB (measured before the independent audit's fix, with the starter's state copied in; the state is no longer copied, whatever its size) |
| The companion `.so` (SBPF v3, `programs.sh`'s build) | 656,936 bytes, 99,688 more than phase 1's 557,248. Mainnet's ProgramData holds 557,248: `solana program extend` by 99,688 bytes (about 0.69 SOL at 6,960 lamports a byte) before the upgrade |
| The starters (SBPF v3) | jackpot 170,616 bytes, streak 186,056 |
| Accounts | `ShareReceipt` 126 bytes (1,767,840 lamports, returned); the starters' states 8 + 584 + 2 + 96 + 32 (jackpot) and 8 + 128 + 2 + 128 + 32 (streak) bytes |
| A hook's state | Any size up to the runtime's 10 MiB: the companion reads the headers in place (tested at 40 KiB, 1 MiB and 10 MiB: `settle`, `close_epoch` and `retire_game` work). A hook should still keep its state small: every callback it runs loads it |

### Failure modes (phase 2)

| What goes wrong | What happens |
|---|---|
| A jackpot's timer runs out while the pot is below its minimum | `settle` closes the round paying nothing (`JackpotUnfunded`, p2-c), unless unclaimed fees would fund it (then claim and settle together) |
| A round's buyer can't be paid (a program's address, a sysvar, a reserved key) | Forfeited (p2-d) |
| Nobody settles a round for 30 days | The next `settle` forfeits it |
| Nobody settles while 8 later rounds end (8 timers at least) | The oldest is dropped; `settle` goes on with the oldest remembered (p2-b) |
| A hook's state grows (a history, a leaderboard) | Nothing: the companion reads only the headers, in place |
| The last buyer sold or sent anything | `settle` forfeits the round; the pot stays |
| Nobody buys after graduation | The jackpot ends; the pot is retired to the buyback every 60 days |
| Buys never stop for 60 days | No round ends: the pot is retired to the buyback (p2-m) |
| Nobody closes a streak epoch during the next one | It rolls over (its total is soon forgotten) |
| A holder sends before claiming | Its share stays in the pot and rolls over |
| Nobody claims | The epoch's pot is released at the next close and rolls over |
| A hook blocked mid-epoch | The pot and the locked part go to the buyback; the epoch ends; nobody is paid |

### For integrators (phase 2)

The SDK (the monorepo's `packages/sdk`) mirrors every builder, account and rule, held to
`programs/tests/vectors/companion-games.json` (its `phase2` section) by `gameKinds.test.ts`:
`companion.createGameV2`, `createGameWithProgramData`, `settle`, `retireGame`, `closeEpoch`, `claimShare`,
`closeReceipt`, `receiptAddress`, `decodeShareReceipt`, `fetchShareReceipts`; `gameKinds.ts`
(the kind headers, `settleRound`, `jackpotWinnerHolds`, `qualifyingBuy`, `streakWeight`, `shareOf`,
`streakClaims`, `streakHoldingsToEnter`, `studioGameHook.prepare/enter/accounts`).

The keeper (`apps/server/src/keeper/games.ts`, `jackpots.ts`):

- **Jackpot settles run in their own loop** (`crankJackpots`, every 15 seconds, and first in every
  companions pass, before any coin's slower work): one batched read of every jackpot's game, hook
  state and status, then every due round settled in turn, oldest first (`settleRound` on the
  hook's header, read as the program reads it; up to 9 a coin a pass), each send watched for 30
  seconds at most, a few coins at once; a pass over 60 seconds is logged. A settle refused for
  unclaimed fees (`FeesUnclaimed`) is sent again with a fee claim in the same transaction (450k
  units when it can't be dry-run: the bundle takes up to about 300k). `retire_game` when due, never
  in a pass where a due settle did not land; under a block, `retire` (it moves the pot).
- **Streak:** `enter` the holders who qualify and are not written this epoch (any time in the
  epoch); `close_epoch` once an epoch is over (refused for a pot below its minimum, it is sent again
  with a fee claim in the same transaction); `claim_share` only where the keeper's bounty pays for
  it: a bounty of at least 50,000 lamports (about twice a claim's fee plus the yield the receipt's
  1,767,840 lamports of rent forgo until returned), or a share of at least 0.05 SOL; at most 16 a
  pass, 64 receipts outstanding a game and 512 in all (about 0.9 SOL of rent). A holder with a
  smaller share claims it (and pays the receipt's rent, returned by `close_receipt`) through the SDK
  or the site. `close_receipt` for its own receipts once their claims end (the receipts it paid for
  are kept in memory, read once a game; no scan each pass); `retire` when due, never in a pass
  where a due `settle` or `close_epoch` did not land; under a block, `retire`.
- **Its balance:** read once a pass; below 0.1 SOL it skips claims and enters (logged) and keeps
  settles, draws, prize claims, retires and receipt closes going.
