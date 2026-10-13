# Loss cover hook

**A Loss cover coin's token hook.** Every holding remembers its average entry price, the tokens
that basis covers, the cover it has been paid and the round it was last recorded in, in the 64
bytes of hook data the token program stores inside the holding. The Loss cover script reads these
and names the holders who are under water; the Bordrless companion pays them SOL from its pot and
has this program record each payment. This program only keeps the records: it never sees SOL,
never refuses a transfer and takes no cut.

| | |
| --- | --- |
| Hook program | `6KMdXC3Fxovp3pnUsdQuUbderPzZtTA4t6R1jacYJWQr` (not deployed yet) |
| Pair | `programs/loss_cover_script` |
| Flags | `BEFORE_TRANSFER \| BEFORE_BURN \| WRITES_HOOK_DATA` (145) |
| Registry extras | the state (writable), the launch (read-only), the launch pool (read-only) |

## The record

| Bytes | Field |
| --- | --- |
| 0 | `version: u8`: 1; 0 is no record (never written, or cleared) |
| 1 | `flags: u8`: bit 0, the holding sold or burned since its last buy |
| 2..10 | `entry: u64`: the average entry price, lamports per base unit times 10^12 (for a 6-decimal token, SOL per whole token is `entry / 10^15`) |
| 10..18 | `amount: u64`: the tokens the record covers (the balance as of the last write) |
| 18..26 | `paid: u64`: lamports of cover paid against this basis |
| 26..34 | `updated_slot: u64`: the slot of the last write |
| 34..42 | `paid_at: i64`: when cover was last recorded (0: never) |
| 42..50 | `basis_at: i64`: when a buy last blended the basis (0: none) |
| 50..54 | `round: u32`: the tag of the last `record_payout`: the round the script last measured or paid it in (0: none) |
| 54..64 | zero |

- **A buy** (the source's owner is the launch pool) blends the basis with the trade's curve price,
  fees aside: `Q' * 10^12 / (B' + a)`, the pool's reserves (real plus virtual) as the callback reads
  them, which the DEX wrote before it delivered the output, and the amount delivered. With a
  reference price set, the basis never takes more than 20% above it: a buyer's own spike never sets
  a basis it can be covered for. `basis_at` is set, the sold flag cleared.
- **A plain transfer in** blends at the spot now: tokens that arrive by transfer carry no loss, so
  a wallet cannot hand its loss to another.
- **A send, a sell or a burn** cuts the record to what is left: `paid` goes with the tokens pro
  rata (a partial sell keeps its cover), the entry stays, the sold flag is set. A holding left with
  nothing is cleared to zeros, so it can be closed.
- **The reference** moves at most once per 10 minutes by at most 5% toward the spot, on any
  callback, from the first spot seen. It only bounds a buy's entry.

Who never holds a record: the launch, its pool, the companion's creator address, the default key,
and every address off the ed25519 curve. The callbacks make no CPI and emit no event.

## The state, `["state", mint]` (274 bytes)

After the discriminator: `version` (8), `bump` (9), `mint` (10), `launch` (42), `pool` (74, derived at
`prepare` from the launchpad's LP fee: `["pool", mint, bridged SOL, lp_fee_bps, launchpad]` under the
DEX), `creator` (106, the companion's `["creator", mint]`), `reference: u64` (138), `reference_at:
i64` (146), `paid_total: u64` (154, lamports recorded), `records: u64` (162, entries recorded,
touches included), `prepared_by` (170), `prepared_at` (202), reserved to 274.

## Instructions

- `prepare(payer, mint (signer), state, registry, launchpad config, system)`: once per mint, before
  the launch; the mint's keypair signs.
- `before_transfer`, `before_burn`: the callbacks, signed by the token program's signer for this hook.
- `record_payout(lamports, tag)` with `[payout_authority (signer), mint, holding (writable), state
  (writable), hook_authority, token program, token event authority]`: only the companion program's
  `PDA(["payout-authority"])` signs it. The holding is of this mint with a record whose owner may
  hold one; `paid` grows by `lamports`, `round` becomes `tag`, `paid_at` moves when something was
  paid; the record is written through the token program's `write_hook_data`.

Errors: `WrongAccount`, `BadHookSigner`, `WrongMint`, `NotEligible`, `NotTheCompanion`, `NoRecord`,
`MathOverflow`. Tests: `programs/tests/tests/loss_cover_hook.rs`, `loss_cover.rs`,
`loss_cover_vectors.rs` (`vectors/loss-cover.json`).
