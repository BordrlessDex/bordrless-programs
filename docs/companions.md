# Companions: reward tokens without a keeper

Status: v1 live on mainnet (2026-10-08); v2 (games, below and `docs/games.md`) built and tested,
not deployed. Program `bordrless_companion`, one audited program deployed by Bordrless; each launch
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

The full reference is `docs/games.md`. Status: built and tested, not deployed (2026-10-08). v2 is
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
  - Rounds of 1 hour to 30 days, matching the hook's header.
  - `min_pot` from 0.1 to 1,000 SOL, `prize_bps` from 10% to 100%.
  - Claim windows of 5 minutes to a day, and 1 to 16 attempts, all within half a round.
  - Phase 1 takes only `lottery_hook`, or a hook the protocol has given a status, never a blocked
    one. Its registry may list at most 3 extras besides the launch.
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
    when the round has no tickets, when it is too late (10 minutes and a claim window before the
    claims end), or when the pot can't pay for ORAO's request now (`OracleUnpaid`, no seed
    committed).
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
