# Companions: reward tokens without a keeper

Status: in development (2026-10-08). Program `bordrless_companion`, one audited program deployed
by Bordrless; each launch that uses it gets its own instance.

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
  `create_launch`'s accounts. With the protocol lookup table it fits for launches without a custom
  hook.
- **Custom hooks.** A companion launch may name a Studio hook. The hook acts inside transfers, the
  companion acts on fees.

## Studio

Phase 1: the templates above, configured in Studio, with no new code per launch. Phase 2:
companions written for a creator by the assistant, deployed by Studio under the same rules as
hooks (immutable or Bordrless-upgradeable). They may call only an allowlist (the Bordrless
programs, the system program, Metaplex Core, a randomness oracle) and move funds only from their
own vaults, with a cap on what they hold until a human has audited them.
