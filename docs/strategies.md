# Strategies: a builder's program decides a coin's payouts

Status: built 2026-10-09 with phase 3a (`docs/phase3a.md` §4); **live on mainnet since 2026-10-10** (companion `977a6459…`). The companion's
`Strategy` game kind, the `bordrless-strategy` crate, Studio's pro-rata starter
(`programs/tests/fixtures/strategies/pro_rata`) and a scripted test strategy
(`programs/tests/fixtures/strategies/tester`).

## In plain terms

A strategy coin launches through its companion like a lottery coin: part of every creator fee goes
into a pot the companion holds, and Bordrless's `lottery_hook` keeps who holds tickets (a holding's
weight for a period is what it held since the period began). Instead of a draw, a program written
by the coin's builder decides how much each period pays and how much each holder gets. The
companion asks it, checks every answer against hard bounds, and pays in SOL itself. The strategy
never touches the pot, can't write any account and can't call any program.

## The two questions

| Call | When | Arguments | Answer | Bounds the companion enforces |
|---|---|---|---|---|
| `plan` | `plan_period(p)`, anyone, once period `p` is over (in time: before its payments' end less a payment window) | `PlanArgs`: the period, its start and end, its ticket total, the pot, `budget_max`, totals so far, the clock | `PlanDecision { budget }` | `budget <= budget_max = budget_bps` (≤ 50%) of the pot, the pot already trimmed to its cap |
| `entitle` | `pay_strategy(p, n)`, anyone, while the period is open, once per candidate | `EntitleArgs`: the owner, its balance, weight and `since`, the period's total, budget, paid so far, `max_amount`, the clock | `Entitlement { amount }` | `amount <= max_amount = min(max_share_bps` (≤ 25%) of the budget, what is left of it`)`, never clamped |

- **The answer** is the strategy's own return data, exactly 8 bytes. An Anchor instruction that
  returns `Result<PlanDecision>` / `Result<Entitlement>` sets it.
- **A refused answer** (no data, another length, over its bound) leaves the period unplanned
  (`PeriodRejected`, the pot intact; it may be asked again until its deadline) or skips the
  candidate (`CandidateRejected`, no receipt).
  `budget == 0` skips the period; `amount == 0` pays nothing and makes no receipt.
- **A failure** (an error, a panic, running out of compute) fails the whole transaction: the runtime
  can't catch a CPI's error. Anyone may retry until the plan's deadline; a period never planned
  lapses with the pot intact (`RolledOver { Late }`). After 60 days with no period paid, anyone
  retires the pot to the buyback.
- **Clamp yourself.** The companion refuses an answer above `max_amount` rather than cutting it, so
  a strategy that answers `budget * weight / total` to a holder above a quarter of the weight pays
  that holder nothing. Answer `min(your amount, args.max_amount)`, as the starter does.

## What a strategy sees

Every account read-only, none a signer, and **no executable account**: a strategy can call no
program, not even itself.

| Index | `plan` | `entitle` |
|---|---|---|
| 0 | the `Game` | the `Game` |
| 1 | the `Companion` | the `Companion` |
| 2 | the ticket hook's state (the game header) | the ticket hook's state |
| 3 | the `Launch` | the `Launch` |
| 4 | the launch's pool | the candidate's holding |
| 5, 6 | the registry's extras | the registry's extras |

The extras are listed in the strategy's registry, `PDA(["bordrless-strategy-accounts", mint],
strategy)` (a `HookAccountList`: the magic, then Borsh), **read once, when the game is made**:
at most 2 accounts, each a fixed key or a PDA whose seeds are literals or `Account(0)` (the mint)
and `Account(1)` (the game), and each **owned by the strategy** then and at every call. That keeps
out sysvars (above all the instructions sysvar, which would show a strategy the whole transaction
and every other candidate) and any account a crank could swap. A strategy without a registry gets
no extras.

The companion's account bytes a strategy reads are as they were before the instruction; the
arguments carry everything the companion computed (the pot after any trim, the budget so far):
**read the arguments**, not the game's or the companion's bytes.

`EntitleArgs.since` is when the holding first received the coin (moved only by sends), **not**
"held since": a wallet given dust long ago and topped up just before a period has an old `since`.
Don't build a loyalty multiplier on it.

## Who is paid

A candidate is `[holding, owner, receipt]`. It is paid only when:

- the holding is the token program's holding of the coin at `["holding", mint, owner]`;
- the owner is an eligible wallet: on the curve, not executable, not owned by the sysvar program,
  none of the runtime's reserved keys, and none of the launch, its pool, the creator address, the
  companion, the game, the terms, the oracle payer, the strategy or the ticket hook;
- its weight for the period is at least `min_weight` and at most its balance;
- it has no receipt for the period yet (`PDA(["claimed", game, period, owner])`, the streak's
  `ShareReceipt`; the sender pays its rent and gets it back with `close_receipt` once the period's
  payments end; a receipt address someone pre-funded is taken over, not refused);
- an empty wallet is not offered less than its rent-exempt minimum (`BelowRent`).

A strategy is asked about one candidate at a time, and every question of a payment comes before
its receipts and payments, so nothing in the payment names the sender while the strategy answers.
It could still read the questions asked before its own in the same payment
(`sol_get_processed_sibling_instruction`): Studio's checks refuse a strategy importing that
syscall. What a crank chooses is the order in which the budget runs out (and anyone may plan and
pay in one transaction): size entitlements so their sum over every holder stays within the budget,
and make an amount independent of the candidates' order (Studio's simulator checks both, and
there is no "first N holders" starter). A candidate repeated in one payment is `AlreadyPaid`.

## The terms (`StrategyTerms`, `PDA(["strategy", mint])`)

| Field | Bounds |
|---|---|
| `strategy` | executable; immutable, timelocked (≥ 3 days, and at creation no proposal in its `Timelock`: `TimelockPending`) or Bordrless-managed, **checked again before every question** (handed to an outside key, it stops: `StrategyNotAccepted`, the pot waits and then retires); a status from the protocol sets its terms but never lets other code in; never blocked; none of the protocol's programs, the companion, the oracle, the timelock, the lottery hook or the game's own hook |
| `budget_bps` | 1 to 5,000 |
| `max_share_bps` | 1 to 2,500 |
| `max_per_tx` | 1 to 4 |
| `plan_cu_max`, `entitle_cu_max` | 1 to 150,000 and 1 to 60,000: **declared, not metered** (below) |
| `extras` | the registry's ≤ 2 accounts, fixed |

`Game.min_weight` is the least weight paid; `round_secs` (1 hour to 30 days) the period, equal to
the ticket hook's header; `claim_window_secs` (5 minutes to half a period) the least a plan leaves
for payments.

**Compute is not metered on chain.** The remaining-compute syscall is feature-gated and the feature
(`5TuppMutoyzhUSfuYdhgzD47F92GL1g89KpCZQKqedxP`) is inactive on mainnet (checked 2026-10-09); a
program calling it fails. The declared caps are what keepers budget for each call and what Studio's
simulator holds a strategy to (70% of them). A strategy that uses more than the transaction's limit
fails it.

## The pot

- The terms applied at every strategy step are the **stricter** of the ticket hook's status and the
  strategy's: blocked if either is, audited only if both are, the lower cap. `claim_fees` stays
  byte-identical and applies the ticket hook's cap only; the strategy's cap trims the pot at the
  next strategy step, before any payment.
- A planned budget is locked (`Companion.pot_locked`) until the period's payments end: a cap lowered
  since never trims it; a block moves it to the buyback with the rest.
- A block (of either) at any strategy step moves the whole pot to the buyback and ends any open
  period: nothing is ever paid to a person under a block.
- The strategy's audit counts only with the audited code's hash (`set_hook_status_v2`) and while
  the strategy is immutable or Bordrless-managed: handed to a timelock, it is capped again (and
  `set_hook_status_v2` can lift the stale audit and block it).
- A game counts as active (not retirable after 60 dormant days) only while it pays: at least 1% of
  the pot (`STRATEGY_ACTIVE_BPS`) since it last counted as active, over however many periods. Past
  its retirement, `retire` closes an open period: dust budgets keep nothing alive.

## Limits, measured (LiteSVM, `companion_strategy.rs`; after the round-1 review, which added the strategy's ProgramData and `Timelock` to both instructions)

| | Bytes (v0, 22-address table) | Compute | Heap (instrumented) | Height |
|---|---|---|---|---|
| `plan_period`, strategy burning ~130k, 2 extras | 690 | 241k | 8,570 | 2 |
| `pay_strategy`, 4 candidates, each `entitle` burning ~45k, 2 extras | 1,172 | 440k | 23,754 of 32,768 | 4 (5 through a router) |
| `pay_strategy`, 4 candidates all refused over their bound | | 119k | 21,582 | |
| `pay_strategy`, 4 candidates all paid already | | 91k | 12,544 | |
| `pay_strategy`, Studio's pro-rata starter, 4 candidates, 1 extra | | 178k | | |
| `plan_period`, no table | 654 | 59k | | |

Trace entries: 21 of 64 at 4 candidates. An audited strategy's plan rehashes its code when the
code's deploy slot moved since the last plan (an upgrade, or anyone's `ExtendProgram`): about half
a unit a byte of code (+75k for the 142 KB starter; 289k for a whole plan at a 10 MiB ProgramData
with the strategy at its cap). Keepers hold a strategy to its caps by its own calls' units (the
`Program <strategy> consumed` log lines), never by the transaction's total. Without a lookup table a payment of 3 no longer fits
(1,285 bytes): keepers use the protocol's table (4 per payment), or the 18-address core table (3).

## Writing one (Studio's starter)

`programs/tests/fixtures/strategies/pro_rata` is the shape: an Anchor program with

- `prepare(...)` (signed by the mint, before the game is made, as `lottery_hook::prepare` is): its
  state for the mint and its registry;
- `plan(args: PlanArgs) -> Result<PlanDecision>` and `entitle(args: EntitleArgs) ->
  Result<Entitlement>`, whose accounts are the prefix above (unchecked) and its own state;
- no other instruction, no CPI but `prepare`'s account creation, no lamports moved, no admin key;
- its state's bump stored at `prepare` and checked with it (`seeds = [...], bump = state.bump`):
  Anchor's bare `bump` searches for it on every call, about 1,500 units a try, so the same strategy
  would cost more on some mints than others (the independent audit's finding 7; the starter's `plan`
  and `entitle` now cost 4,107 and 4,147 units on every mint). Studio's simulator measures a
  strategy over many fresh mints and holds the largest to 70% of the declared cap.

The `bordrless-strategy` crate has the types, the discriminators, `pro_rata`, the ticket readers
(`weight_in`, `slots_of`) and a pool reader (`pool_reserves`).
