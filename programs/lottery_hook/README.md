# Lottery hook

**A lottery coin's token hook.** Every round, the tokens a holder has held since the round began
are their tickets. The Bordrless companion holds the pot, draws a verifiable ticket once the round
has ended and pays the holding that holds it, as long as that holding still has the tokens. This
program only keeps the tickets: it never sees SOL, never refuses a transfer and takes no cut.

| | |
| --- | --- |
| Hook program | `HqFWsCBQ416DAfevJ9TspyT5yXGGoYTCpcreiGkCgWcr` (not deployed yet) |
| Standard | Game ticket standard v1, `crates/bordrless-game` |
| Flags | `BEFORE_TRANSFER \| BEFORE_BURN \| WRITES_HOOK_DATA` (145) |
| IDL | [`idl/lottery_hook.json`](../../idl/lottery_hook.json) |

## Tickets

Rounds are `floor(unix_time / round_secs)`, with `round_secs` from an hour to 30 days, fixed when
the hook is prepared for the mint. Each round has a ticket space `[0, total)`. A holding's tickets
are a range of it, `[start, start + weight)`, kept in the holding's 64 bytes of hook data:

| Bytes | Field |
| --- | --- |
| 0..4 | `round: u32`: the round the holding was last written in (its current range's round) |
| 4..12 | `start: u64` |
| 12..20 | `weight: u64` (0: no tickets that round) |
| 20..40 | the previous round's range (`round`, `start`, `weight`), so a winner who trades in the next round can still claim |
| 40..48 | `since: i64`: when the holding last sent or burned anything, or first received |
| 48..64 | zero |

**A round's tickets are the tokens held since it began.** A holding gets its range of a round at
its first write that round, and only then, for what it has held since the round began. Tokens that
arrive during a round count from the next one. Every change of a holding's balance passes through
the hook, so a holding not written yet this round holds exactly what it held when the round began.

- **Weight is tokens held through the round.** Splitting a balance across wallets changes nobody's
  odds, and moving tokens between wallets only costs the mover: the sender's range is cut, and the
  receiver gets nothing for them until the next round. So nobody can flood a round with dead
  tickets: a round's total is at most what its holders held when it began.
- **Ranges only shrink.** A send, a sell or a burn cuts both ranges to the balance left; the cut
  tickets are dead for ever. A range always starts at the end of the round's space and never grows.
  So no ticket is ever given twice, and a round's total never goes down. A holding emptied to zero
  is cleared, so it can be closed.
- **The first write of a round** opens the holding's range:
  - a receive (a buy, a transfer in): what it held before;
  - a send, a sell or a burn: what it keeps;
  - `enter(holding)`, anyone for any holding: its whole balance.
  Any later write that round only cuts the range (a send) or leaves it (a receive, an `enter`).
  So entering someone twice can't hurt them, and dust changes nobody's tickets.
- **Holders enter once a round** (or trade) to hold tickets in that round: the keeper sends
  `enter` for every holder early in each round, and anyone can.
- **A winner's claim** is made during the round after the drawn one, while the holding still
  remembers the drawn round in its previous slot; the companion ends every claim then.
- **Who never holds tickets**: the launch (`["launch", mint]` under the launchpad), its pool (read
  from the `Launch` account), the companion's creator address (`["creator", mint]` under the
  companion), the default key, and any address off the ed25519 curve (a program's account: a pool,
  a vault, an escrow). Their hook data is never written.

The state at `["state", mint]` starts, after Anchor's discriminator, with the standard's header:
`magic` (`"BRG1"`), `mint`, `round_secs`, `round`, `total`, `prev_round`, `prev_total`, and the
jackpot fields, which this hook leaves zero. The first write of a new round rolls the header:
`(round, total)` moves to `(prev_round, prev_total)`. The companion reads the header at fixed
offsets (`bordrless_game::header_offsets`, `read_state`).

## Instructions

`prepare(round_secs)`: before the launch, once per mint. The mint's keypair signs, so only whoever
launches it chooses its rounds.

| Account | |
| --- | --- |
| `payer` | signer, writable, pays the rent of the state and the registry |
| `mint` | signer: the future mint's keypair |
| `state` | `["state", mint]`, writable |
| `registry` | `["bordrless-hook-accounts", mint]`, writable |
| `system_program` | |
| `event_authority`, `program` | `["__event_authority"]`, this program |

Its registry lists the callbacks' extras: the state (writable), then the launch (read-only). Event
`Prepared`.

`enter`: no signer, whoever pays the fee sends it. Event `Entered` when the holding's data changes.

| Account | |
| --- | --- |
| `state` | `["state", mint]`, writable |
| `mint` | |
| `holding` | the holding to enter, writable |
| `hook_authority` | `6oZ9LkAfgmhmPYjXj3okjYK5sderddEo8fp8MR4H6Gx1` (`["hook-authority"]`, signs the token program's `write_hook_data`) |
| `token_program` | `2XoEWp8cF3kRXg74eVwPAyTFhVCAztn3V88komxAvr22` |
| `token_event_authority` | the token program's event authority |
| `event_authority`, `program` | |

`before_transfer` and `before_burn`: called by the token program only, signed by its
`["hook-authority", lottery_hook]` (`CFyuaxvKmpgnSMCoNqDKCcwxnTUeW8Mm1go1t3UMsvLH`). They make no
CPI and emit no event: under a companion launch they run at stack height 5, Solana's limit. Their
effect is a pure function of the token program's `Transferred` and `Burned` events and the clock,
so an indexer can replay it. About 7,500 compute units each.

Rust builders: `lottery_hook::client::{prepare, enter, extras, state_address, registry_address}`.

## Launching with it

1. Make the mint keypair.
2. `prepare(round_secs)`, signed by the mint.
3. `create_launch` from a `LaunchConfig` naming this program with flags 145 and no kit rules; the
   hook's accounts are its registry's extras: `["state", mint]` (writable), `["launch", mint]` under
   the launchpad (read-only). Measured with the protocol lookup table and the site's longest
   metadata: 1,167 of 1,232 bytes, 38 trace entries, stack height 4.

## Tested

`programs/tests/tests/lottery_hook.rs` launches from a plain config in LiteSVM against the built
programs and mirrors every transaction through the standard's rules: after each buy, sell, send,
burn and `enter`, the header and every holding's hook data must equal the model's byte for byte.
It covers rounds rolling (and two rounds with no write), tickets only for tokens held since the
round began, the previous round's range kept for a claim and cut by a sell, entering twice, dust,
self-transfers adding no tickets, an emptied holding closing, the pool, the launch,
the companion's creator address and an off-curve owner never getting tickets (through graduation),
a 300-step random walk in which no transfer is refused, a forged callback, and the launch's size.
The crate's unit tests check the layout, the rules and a random walk's invariants (no ticket given
twice or revived, no range above its balance or above what its holder held since the round began,
one range a holding a round, a round's total at most what its holders held when it began, ranges
disjoint within the total).

## Limits

- **Dead tickets come only from holders who sell or send during a round.** Every token that leaves
  a holding ticketed this round leaves dead tickets behind, and a draw that lands on one moves to
  the next attempt. A round's total is at most what its holders held when it began, so at worst a
  holder kills its own tickets: moving tokens between wallets, or sending dust, adds none.
- **Entering is per round, and buys count from the next round.** A holder not written yet this
  round has no tickets in it until it trades or someone sends `enter` for it; tokens bought during
  a round count from the next round's first write.
- **Upgradeable** by the protocol's upgrade authority, like Half-Life, until that is revoked.
