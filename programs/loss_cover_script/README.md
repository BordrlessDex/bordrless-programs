# Loss cover script

**A Loss cover coin's companion script.** The companion calls it every interval with the creator
address signing; it reads the Loss cover hook's records and answers which holders are under water
and what each gets, as a `PayoutAnswer` in its return data. It moves nothing itself: the companion
checks the answer against the payout terms, pays each owner SOL from the pot and has the hook record
it.

| | |
| --- | --- |
| Script program | `9mRXU9gpWY3B13AyuCCtNjHnKt1pUFBQ6vJDLTwgakhm` (on mainnet) |
| Pair | `programs/loss_cover_hook` |
| Approval | `approve(kind script, hash, author = default)`: no author share |
| `state_len` | 128 |

## The terms (`companion_init`'s `init_data`, Borsh, 34 bytes)

`cover_bps: u16` (the part of a loss the pot covers when it can, 1,000 to 10,000), `min_holding:
u64` (base units, at least 1), `min_loss: u64` (lamports, at least 1), `min_age_secs: u32` (how long
after its last buy a holding waits before it counts, at most 30 days), `round_secs: u32` (60 to
86,400; the companion's payout window must be the same), `min_pot_lamports: u64` (the least the pot
holds for a round to start, 0 to 100 SOL; 0: none).

## The state, `["script-state", mint]` under the companion (128 bytes)

After the discriminator: `version` (8), `mint` (9), the six terms (`cover_bps` 41, `min_holding`
43, `min_loss` 51, `min_age_secs` 59, `round_secs` 63, `min_pot_lamports` 67), then the rounds:
`round: u32` (75; 0 before the first step), `b: u64` (79; the round's budget: the pot at its first
step), `l_prev: u64` (87; the uncovered loss measured over the previous round), `l_cur: u64` (95;
measured so far this round), `round_paid: u64` (103), `measured: u32` (111), `rounds: u32` (115),
`paid_total: u64` (119), one reserved byte.

## A step

1. The pool's spot, `Q * 10^12 / B`, from the prefix's pool account.
2. The round, `floor(now / round_secs)`. A new round starts only when the pot holds at least
   `min_pot_lamports` (below it, every step answers `NothingToDo`: no interval slot, no measuring,
   `l_prev` untouched). When it starts: `l_prev = l_cur` if the previous round was stepped, else 0;
   the counters reset; `b` is the companion's pot; `CoverRound { mint, round, b, l_prev }` is logged.
3. The terms (by address among the accounts) and the companion's bounds, recomputed exactly.
4. Every holding of the mint among the accounts, in the order given: a record of version 1
   covering its whole balance, not measured this round (`record.round != round`), at least
   `min_holding`, under water by at least `min_loss` (`cover_bps` of its loss, less what it was
   paid), old enough; never the launcher's own, the launch's, the pool's, the creator address's or
   a program's. Its payment: `min(uncovered * b / l_prev, uncovered, what the round's budget has
   left, what the companion's budget has left, the holding's cap)`; a touch (0) when the budget is
   out or the round measures (`l_prev` 0). What it does not pay joins `l_cur`. A payment needs the
   owner's account among the accounts, writable, no program's and not left below its rent-exempt
   minimum; otherwise the holding is skipped for a step that carries it right.
5. Nothing to measure or pay: `NothingToDo` (the step fails, so it pays no bounty and takes no
   interval slot). Otherwise the answer, at most 16 entries; `round_paid` and `paid_total` grow by
   what it answered.

The accounts a step takes after the companion's six (`[creator, mint, launch, companion, pool,
state]`), looked up by key: the `PayoutTerms` (writable), the hook program, the payout authority,
the hook's state (writable), its `["hook-authority"]`, the token program and its event authority,
the bridge's `unwrap_sol` accounts, then the candidates: a holding (writable: the hook records into
it) and, in a paying round, its owner (writable). A paying step carries 7 pairs with the protocol
table (8 are over the packet; 10 at most with a per-coin table of the pairs), a measuring one 13
holdings (the runtime's instruction trace of 64: four inner instructions a touch, five a payment,
on a base of eleven; a coin's first step ever, which also creates the creator's token holding, two
more).

Errors: `BadTerms`, `NotInitialized`, `WrongMint`, `WrongAccount`, `NoPrice`, `NoTerms`,
`NothingToDo`. Tests: `programs/tests/tests/loss_cover.rs`, `loss_cover_vectors.rs`
(`vectors/loss-cover.json`).
