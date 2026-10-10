# Phase 3a: open the edges

Status: design, 2026-10-09, **with the owner's decisions of 2026-10-09 (§0a), which override the
text below wherever they differ**. Nothing in this document is deployed. It is the first step
toward Uniswap-v4-like freedom for builders on the Bordrless Token Standard: code that Bordrless
did not write may decide payouts, act on its own cuts, and be upgraded by its author in public.
Each part is checked against Solana's limits and against the programs as deployed (README,
`docs/hooks-v2.md`, `docs/companions.md`, `docs/games.md`). In the monorepo it builds on
`docs/studio-companions.md` §2 option B, `docs/studio.md`, `apps/server/src/studio` and
`tools/studio-builder`.

Where this document says "today", it means the code in this repository at `d342d31` and the
monorepo at `25d7d60`.

## 0a. Owner decisions (2026-10-09)

The open questions of §15 are answered. Where the rest of this document says otherwise, this
section wins; the sections below are edited to match where it was simple to do so.

| # | Question (§15) | Decision |
|---|---|---|
| 1 | Minimum timelock delay | **3 days** (`MIN_DELAY = MIN_ACCEPTED_DELAY = 259,200 s`). The maximum stays 365 days. A delay can only be lengthened, per hook. |
| 2 | `hook_timelock`'s own upgrade authority | **Bordrless-upgradeable until it is audited**, then finalized (immutable). Until then every label says "the timelock itself can still be changed by Bordrless" (§7), computed from its ProgramData. |
| 3 | Creators' control of Studio-built code | **Yes, opt-in.** At deploy the creator chooses *Bordrless-managed* (today's default, upgradeable by Studio's key) or *timelocked by me* (a `hook_timelock` whose author is the creator's wallet, delay ≥ 3 days). `register` therefore takes the `author` as an argument, so Studio's key can hand a program straight to the creator's timelock. Labels say which (`managed` / `timelocked`, with the author). |
| 4 | Strategy caps | The proposed defaults: budget ≤ 50% of the unlocked pot per period, ≤ 25% of a budget per holder, a 10 SOL pot until audited, periods of 1 hour or more. |
| 5 | Audits tied to code | Yes. An audit records the executable hash (`set_hook_status_v2`, which recomputes the hash on chain and refuses a mismatch); a timelocked or author-upgradeable program is refused an audit: it must be finalized (immutable) first. The label shows "audited" only while the hash is current. |
| 6 | Deferred actions | **`hook_vault` ships in 3a**: burn / sell for SOL to a wallet fixed before launch / sell and buy-and-burn another token, the policy fixed before launch, the sells and buys guarded by a port of the companion's audited buyback guards. "Sold for SOL to <wallet>" is labelled as a creator tax. |
| 7 | Phase-2 game hooks and authority donation | **Closed.** A game hook the companion takes without a `HookStatus` (Studio's key, the protocol's key, or a timelock) must also carry a current Bordrless Studio attestation (`HookAttestation`, not revoked, its build hash equal to the code on chain). Bordrless's `lottery_hook` is unchanged (taken by id). There are no live phase-2 games, so nothing live changes. Strategies keep §2.2's column (an immutable or timelocked strategy needs no attestation: a strategy can't refuse a token move, and its caps bound it). |
| 8 | The attester key | A **separate hot key**, used only by Studio's worker: `~/.config/solana/bordrless-mainnet/studio-attester.json` (never printed or committed), its public key a constant (`STUDIO_ATTESTER`). Its blast radius: labels, and which unaudited game hooks may enter the companion (each capped at 10 SOL and blockable). Revocable by the attester or the companion's upgrade authority. |
| 9 | `hook_timelock` for the protocol's own programs | **No.** Bordrless's core programs are not put behind the timelock. |

## 0. Decisions in one table

| # | Part | Decision | Programs touched |
|---|---|---|---|
| 1 | Labels and vetting classes (§2) | One classification of "who can change this code", shared by the launch program, the companion and the SDK: **immutable / timelocked(N days) / Bordrless-managed / author-upgradeable**, with **audited** and **blocked** shown on top. The label is computed from on-chain accounts and never stored, because a stored label goes stale. | crate `bordrless-hook` (new module), SDK |
| 2 | Timelocked upgrades (§3) | A new program, `hook_timelock`. A per-program PDA holds the upgrade authority. The delay is at least 3 days (§0a), set at registration, and can only be lengthened. Proposals are public with an on-chain code hash. Anyone can execute once the delay has passed; the author can cancel. The authority can only ever move to "none" (immutable). The program is made **immutable after its audit**. | new `hook_timelock`; **launch upgrade** (timelock class in `check_hook_authority`); **companion upgrade** (same class in vetting) |
| 3 | Strategy calls (§4) | A new game kind, `Strategy`, in the companion. Tickets come from `lottery_hook`, so a builder writes only the strategy. The companion CPIs the strategy twice: `plan` once a period (the budget) and `entitle` **once per candidate** (that holder's amount). Every account is passed read-only and without a signer. The answer comes back as return data. The companion checks eligibility, receipts, the budget, the per-holder cap and the pot cap, and reads the strategy's upgrade class again before every question (as built: compute can't be metered on chain, §17). A strategy that fails makes the transaction fail. A period that is never planned lapses, and the pot keeps its funds. | **companion upgrade** (additive), new crate `bordrless-strategy` |
| 4 | Deferred actions (§5) | **No action queue owned by the hook.** A hook sends its cut, as a delta (which hooks can already return), to up to 3 "slot" holdings of a new audited program, `hook_vault`. Each slot has a policy fixed before launch (burn, sell for SOL to a fixed wallet, or sell and buy-and-burn another token), run by a permissionless crank under the companion's buyback guards. A hook cannot do this itself: moving its own coin re-enters it. | new `hook_vault`; **no token or DEX change** |
| 5 | Open deploy CLI and attestations (§6) | `bordrless hook\|strategy build/sim/deploy/propose/execute/finalize/verify` in the SDK repo. **No permissionless on-chain attestation**: anyone can attest anything, so it adds nothing. Instead, **Bordrless signs an attestation** (`HookAttestation` in the companion) after Studio's worker rebuilds the source, matches the hash on chain and runs the simulator. The CLI's local simulation is a developer tool, not evidence. | companion (attestation account) |
| 6 | Risk labels for terminals (§7) | SDK `hookRiskLabel(connection, program)` and the pure `riskLabelOf(accounts)`, held to Rust vectors. Integration guide 05 is rewritten around it. | SDK, docs |

Programs that change: `bordrless_companion` (one or two additive upgrades) and `bordrless_launch`
(one small additive upgrade). New programs: `hook_timelock` and `hook_vault`. **`bordrless_token`,
`bordrless_swap`, `bordrless_bridge`, `bordrless_kit`, `lottery_hook` and `half_life` do not
change.**

## 1. What exists today (the facts this design rests on)

| Fact | Where |
|---|---|
| A custom token hook must be immutable, or upgradeable only by Studio's key `CS1NRy…W6W` or the protocol's `5xsib…pEd`. This is a compile-time constant. | `programs/bordrless_launch/src/constants.rs:75-83` (`HOOK_UPGRADE_AUTHORITIES`) |
| The rule is checked once, at `create_config` (the hook's ProgramData is the one remaining account). `create_launch` does **not** check it again. | `launch_config.rs:99-148` (`check_hook_authority`), `:193-200`; `launch.rs:409-414` ("checked when the config was made … so it still holds") |
| The companion imports the same constant. Phase 2 accepts a game hook without a `HookStatus` when its ProgramData names one of the two keys. | `bordrless_companion/src/constants.rs:211-212`; `instructions/game.rs:222-244` (`upgradeable_by_the_protocol`), `:387-397` |
| `HookStatus` (`audited`, `pot_cap`, `blocked`) is keyed by the **program id**, not by its code. `set_hook_status` zeroes `reserved` on every write. | `state.rs:447-461`; `game.rs:1374-1425` (`s.reserved = [0; 32]` at `:1417`) |
| Space left in existing accounts: `Companion.reserved` has 1 byte, `Game.reserved` has 43, `HookStatus.reserved` has 32. | `state.rs:100, 317, 460` |
| The token program calls `before_transfer` whenever the flag is set, with no exemption when the hook itself is the caller. | `bordrless_token/src/instructions/transfer.rs:109-113`, `hooks.rs` (`HookCall::invoke`) |
| A token hook's value is its deltas: at most 3 a transfer, **only in its own mint**, only to writable holdings of the mint passed as extras, never the source or destination. A token callback never answers a burn. | `bordrless_token/src/hooks.rs` (`apply_deltas`); `docs/hooks-v2.md` §1.1, §2.4, §0.6 |
| On a swap, a token hook's cut is measured by the DEX, and Bordrless takes a 25% share of its value in SOL from the trader. | `docs/hooks-v2.md` §3.1, §5.8 |
| Hooks make no CPI from callbacks. This is a protocol rule (the kit), a Studio static check (`no-cpi`) and a simulator failure. A game launch already runs at stack height 5. | `docs/hooks-v2.md` §4.12; monorepo `apps/server/src/studio/checks.ts:95-113`; `docs/companions.md` "Limits that shape it" |
| Half-Life burns its own furnace from outside a callback (`stoke`). That works only because it has **no burn callback** (`FLAGS` = transfer, delta and data). | `programs/half_life/src/lib.rs:68-70, 238-270` |
| The upgradeable loader's `SetAuthority` (unchecked) needs only the **present** authority's signature, so anyone can hand their program to `CS1NRy…` without Studio's consent. `Upgrade` requires the buffer's authority to equal the program's. | agave 4.3 `solana-bpf-loader-program/src/lib.rs:549-610` and `:415` |
| `ExtendProgram` is **permissionless** (no authority), adds at least 10,240 bytes (SIMD-0431), and **resets the ProgramData `slot`**. | same file `:797-990`; `solana-loader-v3-interface` `MINIMUM_EXTEND_PROGRAM_BYTES = 10_240` |
| `sha256` costs 85 + len/2 CU. `sol_remaining_compute_units` is a syscall. | agave 4.3 `solana-syscalls/src/lib.rs:213, 519` |
| The companion's measured costs: `claim_share` 614 bytes, 131.8k CU, 11,674 bytes of heap; a game coin's buyback about 21–22 KiB of the 32 KiB heap; `.so` 656,936 bytes. | `docs/games.md` "Limits, measured (phase 2)"; `docs/companions.md` |
| Studio deploys every hook under `CS1NRy…`; the creator never gets the authority. Studio records the build hash and simulator result only in its database (`studio_projects.build`), not the template commit. | monorepo `apps/server/src/studio/deploy.ts:73, 271-286`; `tools/studio-builder/README.md:31-35` |
| The integration guide infers "Built with Bordrless Studio" from `upgradeAuthority === STUDIO_UPGRADE_AUTHORITY`. | `bordrless-sdk/docs/integration/05-hooks-and-risk.md` "Who can upgrade a hook" |

Two findings come out of this table. Both shape the design.

- **Authority donation.** Anyone can make a program upgradeable "only by Bordrless" by handing its
  authority to `CS1NRy…`. For the launchpad this is harmless: the donor loses control, so the
  code is as fixed as an immutable hook, which the launchpad accepts anyway. For the companion,
  phase 2's rule then takes the donated game hook with no status: capped at 10 SOL, blockable, and
  Bordrless could upgrade it. That is bounded, but **it skips Studio's review**. The integration
  guide's "Built with Studio" inference is simply wrong for such a program. Fix: provenance comes
  from a Bordrless attestation (§6), never from the authority key alone.
- **`audited` is per program, not per code.** An audited program that is later upgraded stays
  "audited". Today only Bordrless can upgrade a program it audited, so the risk is Bordrless's own.
  With timelocks, an audited hook could be upgraded by its author after the delay; its pot is
  uncapped, and (decision c) an audited hook can never be blocked. Fix (§2.3): an audit is recorded
  with the executable hash, and only a program that is immutable or Bordrless-managed can be
  marked audited.

## 2. Labels and vetting classes (the foundation)

### 2.1 Classes

`bordrless_hook::authority` (a new module in the existing crate, so the launch program, the
companion, `hook_vault` and the SDK vectors share one implementation):

```rust
pub enum AuthorityClass {
    Immutable,                       // loader 2; upgradeable with no authority; loader v4 finalized
    Timelocked { delay_secs: u32 },  // authority == hook_timelock PDA of this program, Timelock account valid
    Protocol,                        // authority ∈ HOOK_UPGRADE_AUTHORITIES (Studio's or the protocol's key)
    Author,                          // anyone else
}
pub fn classify(program: &AccountInfo, programdata: &AccountInfo, timelock: Option<&AccountInfo>)
    -> Result<AuthorityClass, ClassError>;
```

- **Immutable**: the same byte checks as `check_hook_authority` today (`launch_config.rs:99-148`).
- **Timelocked**: the ProgramData authority equals `PDA(["timelock", program], HOOK_TIMELOCK_ID)`.
  The `Timelock` account at that address is owned by `HOOK_TIMELOCK_ID`, its discriminator
  matches, its `program == program`, and its `delay_secs >= MIN_ACCEPTED_DELAY` (3 days, §0a). The PDA
  is derived on chain (one `find_program_address`). The account is read for the delay, so a later
  rule can ask for more.
- **Protocol**: the authority is in `HOOK_UPGRADE_AUTHORITIES` (unchanged).
- Loader v4 programs that are not finalized: `Protocol` or `Author`, as today. The timelock serves
  only the upgradeable loader (v3).

### 2.2 What each program accepts

| Class | Launch: custom token hook (`create_config`) | Companion: game hook (`create_game*`) | Companion: strategy (§4) |
|---|---|---|---|
| Immutable | yes (today) | only with a `HookStatus` (today: an unvetted hook could refuse the companion's buyback) | **yes**: a strategy can't refuse a token move |
| Timelocked ≥ 3 days | **yes (new)** | **yes (new)**, with a current Studio attestation (§0a, 7): capped at 10 SOL, blockable | yes |
| Protocol key | yes (today) | yes (phase 2) | yes |
| Author-upgradeable | no (`HookUpgradeable`) | no | no |
| Any, with a `HookStatus` | n/a | yes unless blocked (today) | yes unless blocked |

Because the timelock program can move an authority only to "none" (§3.4), a config made while
its hook was timelocked keeps a valid class without a re-check of the class at `create_launch`. As
built (independent audit X1, §17), the config records that its hook is timelocked and every launch
from it reads the hook's `Timelock` again, refusing one that holds a proposal: the class needs no
re-check, the notice does.

### 2.3 Audited, tied to code

New instruction `set_hook_status_v2(hook, args, audited_hash: [u8; 32])`, signed by the
companion's upgrade authority as `set_hook_status` is. It writes `audited_hash` into
`HookStatus.reserved[0..32]`. With `audited = true` it requires the hook's ProgramData (passed) to
classify as `Immutable` or `Protocol`: **a timelocked program is finalized first, then audited.**
`set_hook_status` (v1) gets the same refusal for timelocked programs. This tightens the protocol
authority's own instruction and changes nothing for live games. (As built, §17: v1 refuses only
when the ProgramData is passed, so old clients behave as before; the protocol audits through v2,
v1 keeps a hash v2 recorded, and only an audit with a hash lifts a strategy's cap.)

The companion keeps treating `audited` per program in its steps, because hashing the program
every step is unaffordable. The label (§7) shows "audited" only when `audited_hash` equals the
current executable hash.

### 2.4 Provenance

A Bordrless attestation (§6, `HookAttestation`) is the only source of "built by Studio". A
`Protocol`-class program without a current attestation is labelled "upgradeable by a Bordrless
key, not built by Studio".

## 3. `hook_timelock`: timelocked author upgrades

### 3.1 Why a new program (and not Squads, and not a launch constant)

- **A launch constant can't name per-program PDAs.** `HOOK_UPGRADE_AUTHORITIES` is a fixed list,
  so the launch program must be upgraded in any case. The smallest upgrade is a new branch in
  `check_hook_authority` (§2.1) that takes the `Timelock` account as a second remaining account of
  `create_config`. Old clients pass one account and behave exactly as before.
- **Interim without a launch upgrade (not recommended).** Because `create_launch` never re-checks,
  a hook can get a config while `CS1NRy…` holds its authority, after which Studio's key hands it to
  a timelock. That works only for hooks Studio holds, puts Bordrless's hot key in every handover,
  and builds on a check-once quirk an auditor would rightly question. Use it only if the launch
  upgrade slips, and only for Studio-deployed hooks.
- **Squads v4 timelocks** can upgrade through CPI, but their members can execute any transaction,
  including `SetAuthority` to a key, so the authority can escape after a delay. A config
  transaction can also change the delay. Classifying it would mean parsing a third party's layout.
  A purpose-built program whose only exit is "immutable" is simpler to trust and to audit.
- **One PDA per program, not one global authority.** A bug can't let a proposal for program A
  upgrade program B, and a label names the timelock directly.

### 3.2 Accounts

`Timelock` at `PDA(["timelock", program])` under `hook_timelock`. **It is also the program's
upgrade authority.** A program-owned PDA with data can sign through `invoke_signed`. About 290
bytes:

| Field | Type | |
|---|---|---|
| `version`, `bump` | u8, u8 | |
| `program`, `programdata` | Pubkey ×2 | |
| `author` | Pubkey | May propose, cancel, lengthen, finalize, hand over |
| `pending_author` | Pubkey | Two-step handover (default: none) |
| `delay_secs` | u32 | 3 days to 365 days; never shortened |
| `pending` | `Option<Pending { buffer: Pubkey, hash: [u8; 32], len: u32, proposed_at: i64, eta: i64 }>` | One proposal at a time |
| `upgrades` | u32 | |
| `created_at`, `last_upgraded_at` | i64 ×2 | |
| `reserved` | [u8; 32] | |

### 3.3 Instructions

| Instruction | Signers | Does |
|---|---|---|
| `register(delay_secs)` | the current upgrade authority (`author`), a payer | Checks the program (upgradeable loader, its ProgramData, authority == signer), `MIN_DELAY (3 d) <= delay <= MAX_DELAY (365 d)`. Creates `Timelock` and CPIs the loader's `SetAuthorityChecked(programdata → Timelock PDA)`, with the PDA signing. Event `TimelockRegistered`. Works when the author is a multisig PDA (CPI from Squads). |
| `propose()` | author | The buffer (owned by the loader) must already have **buffer authority == the Timelock PDA**. The author set it with the loader's `SetAuthority`, which needs no signature from the new authority. From then on nobody but the timelock can write or close it, so its bytes are fixed. Computes `hash = sha256(buffer[37..37+len])` on chain, with `len` the buffer's length less trailing zero bytes (passed as an argument and checked: the byte at `len-1` is non-zero, and every byte after it is zero). This is the same hash as `solana-verify get-executable-hash`. Refuses `len` > 2 MiB (about 1.05M CU of hashing) and refuses while another proposal is pending. Sets `eta = now + delay_secs`. Event `UpgradeProposed { program, buffer, hash, len, eta }`. |
| `cancel()` | author | CPI loader `Close(buffer → author)`, with the PDA signing. Event `UpgradeCancelled`. |
| `execute()` | **anyone** | Requires `eta <= now < eta + EXECUTE_WINDOW (30 d)` and the buffer equal to `pending.buffer`. CPI loader `Upgrade(programdata, program, buffer, spill = author, rent, clock, authority = PDA)`. The loader moves the buffer's lamports to the spill, so they go to the author, never to the sender. Event `Upgraded { program, hash, slot }`. If the new code is larger than the ProgramData, anyone sends the loader's `ExtendProgram` first; it is permissionless and at least 10 KiB. |
| `expire()` | anyone | After the execute window, closes the buffer to the author and clears the proposal. A stale approval can't land months later. |
| `reclaim_buffer(buffer)` | author | Closes a buffer whose authority is the PDA but which is not the pending one (a mistake), refunding the author. |
| `lengthen(delay_secs)` | author | `new >= old`. A pending `eta` becomes `max(eta, proposed_at + new)`. |
| `propose_author(new)` / `accept_author()` | author / new | Two-step handover of the author role. Timing is unchanged. |
| `finalize()` | author | No pending proposal. CPI loader `SetAuthority(programdata, PDA, None)`: the program becomes immutable. Immediate: immutability only removes power. Event `Finalized`. |

**The invariant the audit must prove:** no instruction, in any state, sets the program's or a
buffer's authority to any key other than the Timelock PDA or `None`, or closes the program. There
is no "unregister", and Bordrless has no override. A lost author key leaves the program as it is,
upgradeable never again (it can't even be finalized).

Errors: `DelayTooShort`, `DelayTooLong`, `NotAuthor`, `WrongBuffer`, `BufferAuthority`,
`BufferTooLarge`, `BadLength`, `ProposalPending`, `NoProposal`, `TooEarly`, `Expired`,
`DelayShortened`, `NotUpgradeable`, `WrongProgramData`. Events are emitted with `emit_cpi!`, as in
every Bordrless program, so the indexer decodes them from inner instructions.

### 3.4 Limits

| | |
|---|---|
| Depth | `execute`: timelock (1) → loader (2). Through a multisig author: 3. |
| Transaction size | `register` about 9 keys, `propose` about 7, `execute` 10 (programdata, program, buffer, spill, rent, clock, loader, PDA, timelock program, sender): under 500 bytes |
| Compute | `propose` about 0.5 CU a byte: about 125k for a 250 KB hook, 1.05M at the 2 MiB cap (send it with a compute limit). `execute`: the loader's deploy verification, to be measured in LiteSVM against the 4.3 loader before the limits are fixed. |
| Heap | No copies: the buffer is hashed in place |
| Accounts | `Timelock` about 290 bytes, made by CPI (far under the 10 KiB a CPI can create). It never grows. |
| Same-slot | The loader refuses an upgrade in the slot of a deploy or extension: a griefer's `ExtendProgram` delays `execute` by one slot |

### 3.5 The timelock's own upgrade authority

If Bordrless can upgrade `hook_timelock`, it can release every authority it holds. A label of
"timelocked 30 days" is honest only once `hook_timelock` is immutable. The plan is a small program
of about 200 KB, audited in full, then **finalized**. Until then, the label adds "the timelock
program itself can still be changed by Bordrless" (§7), computed from `hook_timelock`'s own
ProgramData.

### 3.6 Launch and companion acceptance

- **Launch upgrade:** `check_hook_authority(program, programdata, timelock: Option<&AccountInfo>)`
  calls `bordrless_hook::authority::classify`. `create_config` passes the `Timelock` as
  `remaining_accounts[1]` when the hook is timelocked. A new error, `HookTimelockInvalid`, is
  appended. Every other path is byte-identical. `HOOK_TIMELOCK_ID` is a constant, so
  `hook_timelock` is deployed before the launch upgrade. (As built, §17: a timelocked hook is taken
  only while its `Timelock` holds no proposal, `HookTimelockPending`, at the config and at every
  launch from it, which passes the `Timelock` last.)
- **Companion upgrade:** `create_game`, `create_game_v2` and `create_strategy_game` use the same
  `classify`. A timelocked game hook is taken without a status, capped at 10 SOL and blockable,
  exactly like a Studio hook. (As built, §17: with a current attestation, and never while its
  timelock holds other code; a timelocked strategy never while its timelock holds a proposal.)

## 4. Strategy calls (the companion)

### 4.1 What a strategy can and cannot do

The audited companion keeps custody of the pot and asks a builder's program two questions:

1. **`plan`**: how much of the pot this period pays (a budget).
2. **`entitle`**: how much this one holding gets.

It cannot move funds, write any account, or see more than one candidate at a time. The companion
checks every answer against hard bounds and pays in SOL itself.

**Why one candidate per call.** Nothing on Solana lists a mint's holders on chain, so a crank
chooses which holdings a transaction carries. A strategy that picks among candidates ("the top
3") would let the crank leave out rivals. Calling `entitle` once for each holding means a holder's
amount depends only on that holding and on shared state, never on who else was in the
transaction. What remains is first-come ordering against the period's budget. Strategies size
entitlements so their sum stays under the budget (for example `budget × weight / total`); the
Studio simulator checks that against every simulated holder (§4.9). "Top N" strategies can't be
verified on chain, and 3a does not offer them.

**Why tickets come from `lottery_hook`.** Eligibility needs "held since the period began", and only
a game-ticket-standard hook records that (`bordrless-game` slots: ranges cut by sends, `since`). A
strategy coin launches with Bordrless's audited `lottery_hook` (flags 145, through the existing
lottery `LaunchConfig`s). A holding's weight for period `r` is its ticket range for round `r`. So
**a builder writes only the strategy**; no per-coin hook code. A vetted hook with a lottery-format
header is also accepted. Jackpot and streak headers are not.

### 4.2 State

`GameKind::Strategy` is appended (Borsh index 3), with `hook_flags()` = 145. `Game` keeps the period
exactly as a streak keeps its epoch (`round`, `total`, `prize` = the budget, `epoch_paid`,
`status = Revealed` while open, `Companion.pot_locked` = the unpaid budget).

`StrategyTerms` at `PDA(["strategy", mint])` (new, about 145 bytes). It is a separate account so
that `Game.reserved` (43 bytes) stays free for later kinds.

| Field | Bounds |
|---|---|
| `strategy: Pubkey` | Executable; not a protocol program, the companion, ORAO, the game hook or the system program; vetted (§2.2) |
| `budget_bps: u16` | Most of the unlocked pot one period may pay: 1 to `MAX_STRATEGY_BUDGET_BPS` (5,000) |
| `max_share_bps: u16` | Most one holder gets, in bps of the period's budget: 1 to 2,500 |
| `max_per_tx: u8` | 1 to `MAX_STRATEGY_PER_TX` (4, fixed by transaction size, §4.7) |
| `plan_cu_max: u32`, `entitle_cu_max: u32` | At most 150,000 and 60,000 |
| `periods_planned: u32`, `paid_total: u64`, `last_plan_at: i64` | Running totals |
| `reserved: [u8; 32]` | |

`Game.min_weight` (phase 2's field) is the least weight that may be paid (at least 1). `round_secs`
(1 hour to 30 days) is the period and must equal the ticket hook's header. `claim_window_secs`
works as a streak's: the least time a plan leaves for payments.

### 4.3 Instructions

**`create_strategy_game(args: CreateGameArgs, s: StrategyArgs)`**, signed by the mint before the
launch, as `create_game` is.

- Accounts: those of `create_game`, plus `StrategyTerms` (init), the strategy program, its
  ProgramData, its `HookStatus` (it need not exist), and an optional `Timelock`.
- `args.kind == Strategy`, the split has a pot part and no holders' part, and buyback limits are
  set (as for every game).
- The ticket hook's header is in lottery format with `round_secs == args.round_secs`.
- The strategy's registry, `PDA(["bordrless-strategy-accounts", mint], strategy)` (a
  `HookAccountList`), lists at most 2 extras, **each owned by the strategy program**. That keeps
  out sysvars (above all the instructions sysvar, which would show the strategy the whole
  transaction and every other candidate) and any account a crank could swap.
- `create_game_v2` refuses `Strategy` (`WrongGameKind`), so no strategy game exists without its
  terms.
- Events: `GameCreated`, `StrategySet { game, strategy, class, budget_bps, max_share_bps,
  max_per_tx, min_weight, audited, pot_cap }`.

**`plan_period(period)`** (anyone, during period + 1). This is the streak's `close_epoch`:

1. A period whose payments ended releases its lock (`EpochEnded`).
2. The terms are the **stricter of the ticket hook's and the strategy's `HookStatus`**: blocked if
   either is, audited only if both are, the lower cap. Under a block, the pot goes to the buyback
   and nothing else happens.
3. `period` is the period that just ended and has not been planned (`RoundNotOver`). The pot is
   at least `min_pot` (`PotTooSmall`, an error: the keeper sends it again together with a fee
   claim). It is not later than `claims_end(period) − claim_window` (`Late`: the period lapses).
   Its total comes from the header (none or forgotten: `NoTickets` / `RoundForgotten`, lapsed).
4. (As built: the strategy's class is read again from its ProgramData and `Timelock`, and an
   author-upgradeable strategy is refused, `StrategyNotAccepted`; compute is not metered, §17.)
   The companion computes `budget_max = min(bps(pot − pot_locked, budget_bps), cap headroom)`,
   clears return data, reads `sol_remaining_compute_units()` and CPIs
   `strategy::plan(PlanArgs)` with the read-only prefix `[game, companion, hook_state, launch,
   pool]` plus the registry's extras.
5. It reads the return data: it must be set **by the strategy** (`get_return_data().0 ==
   strategy`), exactly 8 bytes, and decode as `PlanDecision { budget: u64 }`. The strategy must
   have used at most `plan_cu_max`, and `budget <= budget_max`. Any failure here (no data, data set
   by another program, malformed, over budget, over compute) **closes the period with nothing**:
   `PeriodRejected { reason }`, the pot untouched, the period marked planned. `budget == 0` is
   `PeriodSkipped`. (As built, round 1: a refused answer leaves the period **unplanned**, to be
   asked again until its deadline, so nobody voids a period by asking at a chosen moment.)
6. Otherwise `pot_locked = budget`, payments open until `claims_end(period)`: `PeriodPlanned {
   period, budget, total, cu }`.

**`pay_strategy(period, n)`** (anyone, while the period is open).

- Accounts: the sender (payer), companion (w), game (w), terms, both statuses, launch, the hook's
  state, the strategy program, the creator address (w) and the system program.
- Remaining: the strategy's extras, then the bridge's `unwrap_sol` accounts, then `n` triples
  `[holding, owner (w), receipt (w)]`, with `1 <= n <= max_per_tx`.

For each candidate, in order (as built, round 1: steps 1 and 2 run for every candidate first,
then the receipts and payments, so while a strategy answers, the only earlier instructions of the
payment are other questions, never a receipt naming the sender; a candidate repeated in one
payment is `AlreadyPaid`; the strategy's ProgramData and `Timelock` follow the fixed accounts):

1. **The checks** (fail: `CandidateRejected { owner, reason }`, nothing paid, no receipt, the
   rest go on):
   - The holding is the token program's holding of the mint at `["holding", mint, owner]`.
   - The owner is eligible: a wallet on the curve, not executable, not owned by the sysvar
     program, not one of `RESERVED_KEYS`, and none of the launch, the pool, the creator address,
     the companion, the game, the terms, the oracle payer, the strategy or the hook.
   - The owner's weight for `period` (its ticket range for the round, from hook data) is at least
     `min_weight` and at most its balance.
   - No receipt exists yet: `PDA(["claimed", game, period, owner])`, the phase-2 `ShareReceipt`,
     so `close_receipt` works unchanged.
2. **`entitle`**: CPI `strategy::entitle(EntitleArgs)` with the read-only prefix `[game, companion,
   hook_state, launch, holding]` plus the extras. The return data must come from the strategy,
   be 8 bytes, decode as `Entitlement { amount }`, and the CPI must stay within `entitle_cu_max`.
   Then `amount <= bps(budget, max_share_bps)` and `amount <= budget − paid`, else the candidate
   is rejected (**fail closed: never clamped**, so a buggy strategy that answers `u64::MAX` pays
   nobody, rather than the cap to everyone). `amount == 0` pays nothing and makes no receipt.
3. The receipt is made (the sender pays its rent and gets it back through `close_receipt` after
   `claims_end`).

Then **one** `unwrap_sol` of the sum, followed by a system transfer to each owner (less
`bounty_bps` to the sender, as `claim_share` does). `epoch_paid` and `pot_locked` are updated, and
`settled_at` moves, so a strategy that pays is never "dormant" (as built, round 1: only once the
period has paid at least `STRATEGY_ACTIVE_BPS`, 1%, of the pot; dust keeps nothing alive). One event: `StrategyPaid { period,
payments: Vec<(owner, amount)>, bounty }`.

**Reused unchanged:** `claim_fees`, `close_receipt`, `burn_stranded`. **`retire`** gains the
Strategy branch: it waits (`DrawPending`) while a period is open with budget left, as it does for a
streak. A strategy whose plans always fail or pay 0 leaves the pot unpaid, so after 60 days
(`RETIRE_DORMANT_PERIODS`) anyone retires it to the buyback. **No pot is locked for ever.**

Errors appended after 6051: `StrategyNotAccepted`, `StrategyAccounts`, `StrategyRegistry`,
`TooManyCandidates`, `PeriodNotOpen`, `WrongGameKind` (reused).

### 4.4 What happens when a strategy misbehaves

| Strategy does | Result |
|---|---|
| Panics, errors, loops until compute runs out | **The whole transaction fails.** Solana can't catch a CPI's error, so the companion can't "roll over in the same transaction". Nothing moves. Anyone can retry until the plan deadline, after which the period lapses with the pot intact. The keeper logs it. |
| Returns nothing, the wrong length, garbage, or data set by a program it called | `plan`: `PeriodRejected` (as built: the period stays unplanned until its deadline). `entitle`: that candidate is skipped. (A strategy is given no executable account, so it can call no program at all.) |
| Returns more than the bounds allow | Same: rejected, never clamped |
| Uses more compute than its terms allow | (As built: not measurable on chain, §17.) The transaction fails if the strategy exhausts it; keepers budget the declared caps and refuse a dry run above them; Studio's simulator holds a strategy to 70% of them. |
| Tries to write an account or move lamports | Impossible. Every account is passed read-only with no signer, a callee can't raise a privilege, and its own PDAs are not passed writable. |
| CPIs into the companion | Refused by the runtime (A→B→A) |
| CPIs elsewhere | (As built: impossible, no program account is passed.) |
| Reads earlier instructions of the payment (`sol_get_processed_sibling_instruction`) | It sees the earlier candidates' questions only (round 1 orders receipts after every question). Studio's checks refuse a strategy importing that syscall; the payout of an order-dependent strategy built elsewhere depends on the crank's order, as its label can't show. |
| Reads the instructions sysvar, or a candidate's neighbours | Not passed. Registry extras must be owned by the strategy. |
| Answers differently for the same inputs (reads `Clock`) | Allowed. The companion bounds the amounts, not their fairness. |
| Is rigged (pays its author's wallets the maximum) | Bounded: each wallet must hold tokens it held since the period began, at least `min_weight`; one holder gets at most `max_share_bps` of a budget that is at most `budget_bps` of a pot capped at **10 SOL** while unaudited. Worst case for a rigged, unaudited strategy at the limits: 5 SOL per period across at least 4 eligible wallets, as long as the pot holds. The label says so (§7). Bordrless can block it. |
| Is upgraded | Managed: by Bordrless only. Timelocked: after N days of public notice, with the proposal's hash on chain. Immutable: never. Handed to an outside key: refused at the next plan or payment (round 1). |

**The kill switch.** `set_hook_status(strategy, blocked)` applies to strategies as to game hooks:
only while not audited, lifted only by an audit. At the next strategy step the pot, including the
locked budget, goes to the buyback, and nothing is ever paid to a person. Blocking the ticket hook
does the same.

### 4.5 Bounds, consolidated

| Bound | Value | Enforced |
|---|---|---|
| Pot | 10 SOL unless the ticket hook and the strategy are both audited; lower if either status says so | `enforce_terms` at every strategy step. `claim_fees` stays byte-identical and applies only the ticket hook's cap; the strategy's cap trims the pot at the next strategy step, before any payment. |
| Budget per period | at most `budget_bps` (5,000 max) of the unlocked pot | `plan_period` |
| Per holder per period | at most `max_share_bps` (2,500 max) of the budget, one receipt | `pay_strategy` |
| Rate | one plan per period of 1 hour to 30 days; payments only until the period after ends | `plan_period`, `claims_end` |
| Per transaction | at most `max_per_tx` (4) candidates | `pay_strategy` |
| Recipients | eligible wallets with weight ≥ `min_weight` held since the period began; never the pool, launch, creator, companion, game, oracle payer, strategy or hook | `pay_strategy` |
| Compute | `plan_cu_max` ≤ 150k, `entitle_cu_max` ≤ 60k | declared, not metered (§17) |
| Return data | exactly 8 bytes (the runtime's maximum is 1,024) | decode |

The beneficiary is not excluded: they can always hold through another wallet, so excluding the key
would be for show. Lottery tickets don't exclude it either.

### 4.6 The interface (`crates/bordrless-strategy`, new)

```rust
pub const PLAN: [u8; 8]    = sighash("global:plan");
pub const ENTITLE: [u8; 8] = sighash("global:entitle");
pub const REGISTRY_SEED: &[u8] = b"bordrless-strategy-accounts";

pub struct PlanArgs { pub version: u8, pub mint: Pubkey, pub period: u32, pub period_start: i64,
    pub period_end: i64, pub total: u64, pub pot: u64, pub budget_max: u64,
    pub periods_planned: u32, pub paid_total: u64, pub now: i64 }
pub struct PlanDecision { pub budget: u64 }

pub struct EntitleArgs { pub version: u8, pub mint: Pubkey, pub period: u32, pub owner: Pubkey,
    pub balance: u64, pub weight: u64, pub since: i64, pub total: u64, pub budget: u64,
    pub paid: u64, pub max_amount: u64, pub now: i64 }
pub struct Entitlement { pub amount: u64 }
```

A strategy is an Anchor program whose `plan` and `entitle` return `Result<PlanDecision>` /
`Result<Entitlement>` (Anchor sets the return data, as hooks do for `HookReturn`). The prefix
accounts are those of §4.3, all read-only. There is no signer, because a strategy is a pure
function and anyone calling it directly learns only what simulation would show. The crate
re-exports the `bordrless-game` readers (slots, header, `parse_holding`) and the pool's reserves
reader.

### 4.7 Limits, estimated (the tests measure and assert each)

| | Estimate | Basis |
|---|---|---|
| Depth | companion (1) → strategy (2); events by self-CPI (2); `unwrap_sol` bridge (2) → token (3). Through a router, one more each: well under 5. | §4.12 of hooks-v2 |
| `plan_period` | about 650 bytes (v0, table); 60k + the strategy's CU (≤ 150k) | `close_epoch` 465 bytes / 25.9k CU, plus terms, status, strategy, pool, 2 extras |
| `pay_strategy`, 1 candidate | about 810 bytes | `claim_share` 614 bytes, plus terms, strategy status, strategy, hook state, 2 extras (≈ 33 bytes each) |
| each further candidate | +about 102 bytes (3 keys and indices) | so **4 candidates ≈ 1,120 bytes** of 1,232. If a test exceeds 1,232 with 2 extras, `MAX_STRATEGY_PER_TX` drops to 3. |
| `pay_strategy` compute, 4 candidates | about 110k + 4 × (checks and receipt ~20k + entitle ≤ 60k) ≈ 430k worst case | `claim_share` 131.8k with its single unwrap and transfer |
| Heap | about 11.7 KB + ~1.2 KB per candidate (the entitle instruction, receipt creation, transfer) ≈ 17 KB of 32 KiB, with one unwrap and one event for the whole batch | `claim_share` 11,674 bytes; the heap is never freed |
| Trace entries | about 20 of 64 at 4 candidates | |
| Accounts | `StrategyTerms` ~145 bytes; receipts 126 bytes (1,290,320 lamports at mainnet's 5,080 a byte; 1,767,840 at LiteSVM's default rent; returned) | |

### 4.8 Strategy vetting, labels and caps

Vetting is §2.2's strategy column. A Studio-built strategy is deployed under `CS1NRy…` (managed)
or, if the owner says yes to open question 3, a timelock whose author is the creator. An outside
strategy must be immutable or timelocked. `HookStatus` serves strategies unchanged (the PDA is
keyed by program id). An audit lifts the 10 SOL cap only when the ticket hook is audited too.

### 4.9 How a Studio strategy is written

- **Template:** `tools/studio-builder/template` vendors `bordrless-strategy`. The project kind is
  `strategy`, and `src/lib.rs` has `plan`, `entitle` and an optional standard `prepare` that
  writes the registry.
- **Starters:**
  - pro-rata (`budget × weight / total`);
  - loyalty (weight × a multiplier growing with `now − since`, normalised so the sum stays under
    the budget);
  - "first N holders over a threshold get a flat amount";
  - a decaying budget (a share of the pot that shrinks each period).
- **Static checks** (new rule set in `checks.ts`):
  - no CPI at all (not even `write_own_hook_data`), no lamports, no `realloc` or `close`;
  - no `Signer` in any instruction but `prepare`'s payer: a strategy keeps no admin switch;
  - `plan` and `entitle` return exactly the crate's types;
  - the registry lists only the strategy's own PDAs (no sysvars, unlike hooks today:
    `checks.ts:313`).
- **Simulator** (`studio-sim strategy`):
  - a coin with `lottery_hook`, 50 to 500 simulated holders with random weights, `since`
    values and balances, over 12 periods;
  - checks that `plan` and `entitle` never fail, return 8 bytes and stay under 70% of their CU
    limits;
  - checks that the sum of `entitle` over **every** simulated holder is at most the budget, every
    period;
  - checks determinism (same inputs, same outputs, with `now` fixed);
  - checks that nothing goes to an excluded owner, even when the harness offers one;
  - fuzzes 1,000 random holding states.
  - Report: `pass`, the worst-case share of one holder, the CU percentiles.
- **Review prompts:** "does any path favour a fixed address, the author, or a value only the
  author can set?"

## 5. Deferred actions for hooks (`hook_vault`)

### 5.1 What a hook can accrue, and why it can't act on it itself

- **Token side.** A custom token hook's only value is its cuts: up to 3 deltas per transfer, in
  its **own coin**, to holdings of that coin. On swaps the DEX counts them as cuts and takes
  Bordrless's 25% share in SOL from the trader. A launched coin's pool hook is always the launch
  program, so **a creator's hook never touches SOL**. A hook PDA can hold lamports someone sends
  it, but nothing routes trade SOL there.
- **Acting from a callback.** A callback can't act. Hooks make no CPI from callbacks, both by rule
  and because a game launch already runs at height 5. A token-program CPI from a callback would
  re-enter the token program, which the runtime refuses.
- **Acting outside a callback.** The hook can't act on its own coin there either. To sell or send
  its cut, the hook's PDA would call the DEX or the token program, which calls the coin's hook
  `before_transfer` (no exemption for the caller: `transfer.rs:109-113`). That is **hook → swap →
  token → hook: re-entry, refused.** Half-Life's `stoke` works only because it has no burn
  callback. A hook could move *other* tokens (bridged SOL) from its PDAs, but it has none: it can't
  turn its coin into SOL.

So **whatever executes must be a program other than the hook**. A queue owned by the hook can
safely do exactly what its deltas already do and no more, because the trust boundary is the hook
itself. A hook could always name its author's holding as a delta target, so an executor adds no new
power over holders. What it adds is safe execution: sandwich-resistant selling, no keeper, and a
destination fixed in public before the launch.

### 5.2 Why not a hook-owned action ring

The ring as briefed:

- needs a writable, ever-growing (or wrapping) account on every transfer;
- overflows when trades outpace cranks;
- carries parameters (pools, recipients) that the executor would have to validate anyway;
- and buys ordering, which none of the target uses ("buy X with my cut", "burn my cut", "route my
  cut to a vault") need.

**The minimal primitive is the delta itself:** the hook sends each cut to the vault slot whose
policy it wants. The slot's balance is the queue. That needs **no change to the hook protocol, the
token program or the DEX**. A monotonic "intent counter" header (a hook asking for an action with
no cut attached) is deferred to 3b, until a real use needs it.

### 5.3 Accounts

- `Vault` at `PDA(["vault", mint])` under `hook_vault`, about 500 bytes. Fields:
  - `mint`, `hook`, `creator`;
  - `slots: [SlotPolicy; 3]` (3 = `MAX_DELTAS`), each with `done`, `last_at`, `pending_sol`,
    `reference_price`;
  - `bounty_bps` (≤ 100), `max_sell_bps` (≤ 100: bps of the pool's quote side per call),
    `interval` (60 s to 30 days);
  - `created_at`, `last_activity_at`, `reserved`.
- Slot owners at `PDA(["slot", mint, [i]])`: system-owned with no data, like the companion's
  creator address. Each has a holding of the coin (**the delta target**) and, for selling slots, a
  holding of bridged SOL.

`SlotPolicy` is fixed at creation:

| Policy | `execute(i)` |
|---|---|
| `Burn` | Burns the slot's coin balance. The coin's `before_burn` runs if it subscribes; the depth is vault (1) → token (2) → hook (3). No bounty. |
| `SellForSol { to: Pubkey }` | Sells at most `max_sell` of the slot's coin on the coin's launch pool, unwraps the proceeds and sends them to `to` (a wallet fixed before launch), less the bounty |
| `SellBuyBurn { pool: Pubkey }` | Step 1 sells into `pending_sol`. Step 2 (`execute_buy(i)`) buys token X on `pool` (a Bordrless DEX pool quoted in bridged SOL, X ≠ the coin) and burns X |

### 5.4 Guards

The guards are a **port of the companion's audited buyback guards** (`steps.rs`), mirrored for sells:

- at most `max_sell_bps` of the pool's quote side per call (at most 1%, and less on a low-fee pool,
  as `pool_share_bps` does), so a sandwich costs more than it makes;
- at least `interval` apart;
- an output no worse than the pool's own quote less fees and 2%;
- a sell **waits** while the price is more than 3% below a reference price. The reference starts at
  the pool's price at `open_vault` and is lowered toward the price by 5% an interval while waiting;
  a sell only ever raises it.

The buy leg uses the buyback guards as they are. The bounty is `bounty_bps` of the SOL moved, as
the companion pays it.

**After the round-1 audit (`log-3a-audit-vault-r1.md`), as built:**

- The slice is the vault's, not each slot's: each of the `n` selling slots sells at most `1/n` of
  it a sale, once an interval (round 2: split evenly, so no slot can starve another and be retired
  for it; a first-come shared window let the first crank take it all), and a vault takes at most
  one buy slot a pool (`DuplicatePool`). The slice counts only the fees nobody gets back (`vault_share_bps`: Bordrless's
  share of the creator fee, not the fee, since the creator recovers it), so the creator's own
  sandwich does not pay either.
- After a guard wait the slot's next attempt is a minute later (`waited_at`, `buy_waited_at`; round
  2: not a whole interval, which a one-block dip could force): no trade follows a wait in the same
  transaction or block, and retries within the interval don't step the reference again. Every trade
  restarts the reference's clock. A wait counts as a run for `retire`.
- `create_vault` requires the mint not to exist yet (no data, system-owned), and refuses a
  `SellForSol` wallet that can't take SOL for good (executables, loader- or sysvar-owned accounts,
  the runtime's reserved keys, every slot owner index, the vault, the mint).
- A `SellBuyBurn` slot declares `max_cut_bps` for a custom-hook token bought (in its `min_out`); a
  buy of a kit token with max wallet is cut to the cap before graduation; whatever was sent to the
  slot's owner is burned before the buy.
- Hook registries are resolved in place (no decode), so a registry at the bounds can't exhaust the
  heap (it did, decoded twice in one buy).
- **Residual:** any number of vaults (and the companion's buyback) may buy the same token X on its
  pool. A bundle that cranks every due buy on one pool stacks their slices; each alone is safe, `k`
  together pay a sandwich once `k` slices exceed half the round-trip fees. Closing it needs a
  protocol-wide per-pool limiter (deferred).

### 5.5 Instructions

| Instruction | Who | Does |
|---|---|---|
| `create_vault(args)` | the mint's keypair before launch (as the companion's `create`), or the mint's `hook_authority` for a token made outside the launchpad | Writes the policy. It never changes after this. |
| `open_vault()` | anyone, once the mint exists | Creates the slot holdings (the sender pays rent) and checks `mint.hook_program == vault.hook`. It also sets the reference prices. Until it runs, the hook must not name the slots: a delta to a holding that doesn't exist fails the transfer (`InvalidDeltaAccount`). The template's helper answers no cut while `slot_holding.data_len() == 0`, as Half-Life waits for `light`. |
| `execute(i)`, `execute_buy(i)` | anyone | As in §5.3, with a bounty |
| `retire(i)` | anyone | After 60 days with a balance and no execution (the hook refuses the vault's transfer, or the guard waits for ever), burns the slot's coin (if the hook refuses burns too, the tokens stay in a PDA that has no other exit), and sends a stuck `pending_sol` to the incinerator, as `burn_stranded` does. Nobody receives anything. |

Depth: vault (1) → swap (2) → token (3) → coin hook (4), and swap (2) → launch hook (3); the buy leg
reaches X's hook at 4; `unwrap_sol` reaches 3. Through a router, 5. Transactions are about the size
of a companion `buyback` (886 bytes). The coin's hook may take a cut from the vault's own sell. It
cannot name the vault's source holding (`apply_deltas` forbids the source), so a loop between slots
only shrinks. **Token and DEX: no change.**

### 5.6 Studio

- Hooks with cuts may name vault slot holdings in their registry. `checks.ts`'s registry rule
  gains the token program's holding PDA of a `hook_vault` slot owner.
- The launch form shows each slot's policy and, for `SellForSol`, the destination wallet in plain
  words: "a cut sold for SOL to <wallet>". It is a creator tax in effect, and the label says so.

### 5.7 Cut from the brief, with reasons

- **"Stake fees."** There is no staking program, and the kit (holder rewards) is refused with a
  custom hook (`CustomHookWithKitRules`).
- **Routing cuts into a companion.** A companion refuses delta-taking hooks; game hooks have flags
  145.
- **Hook-owned queues and counters.** Deferred to 3b (§5.2).

## 6. Open deploy CLI and attestations

### 6.1 What adds trust and what doesn't

| Claim | Who can make it | Trust it adds |
|---|---|---|
| "I ran the simulator on build H and it passed", written on chain by the deployer | anyone, about anything | **None.** The simulator is deterministic, so anyone with the source can rerun it, and a self-attestation proves nothing a rerun wouldn't. Without the source it is an unverifiable sentence. |
| The on-chain code equals a reproducible build of published source (`solana-verify`, OtterSec's verify PDA with repo and commit) | the program's authority | Real but narrow: the code is what the source says. It says nothing about safety. Already the ecosystem standard, and what the protocol's own programs use. |
| "Bordrless rebuilt this source, the hash matches the deployed code, the static checks and the simulator passed (cut ≤ X, no refusal), and the AI review found no critical or high issue" | **only Bordrless's attester key** | Meaningful, provided one trusts Bordrless's pipeline. One key for terminals to trust, and it can be revoked. |

**Recommendation:** no permissionless attestation account. Ship (a) the Bordrless-signed
attestation and (b) OtterSec verify PDAs as the source pointer, both shown by `hookRiskLabel`.

### 6.2 `HookAttestation` (in the companion)

The companion already holds the protocol's say over programs (`HookStatus`), and it is being
upgraded anyway. A dedicated registry program would be one more audit for a write-only record.

`PDA(["attest", program])`, about 250 bytes (≈ 0.0026 SOL), paid by Studio. Fields:

| Field | |
|---|---|
| `program` | |
| `build_hash` | `solana-verify` executable hash |
| `source_hash` | sha256 of the frozen source |
| `template_commit` | 20 bytes. Studio stores none today (`tools/studio-builder/README.md:31-35` asks for it). |
| `sim_version`, `sim_pass`, `cut_max_bps`, `cap_bps` | |
| `review` | pass or warn |
| `kind` | 0 token hook, 1 game hook, 2 strategy (as built: a game hook is taken only with a kind-1 attestation) |
| `programdata_slot` | at attestation |
| `attested_at`, `attester` | |
| `revoked`, `revoked_at` | |
| `reserved` | as built: `reserved[0] = 1` when the protocol revoked it (that build is never attested again) |

Instructions:

- `attest(program, args)`: signed by `STUDIO_ATTESTER`, a constant naming a new hot key used only
  by the worker, not `CS1NRy…`.
- `revoke(program)`: signed by the attester or by the companion's upgrade authority. (As built,
  round 1: a revocation by the upgrade authority sticks to the build: `attest` refuses that hash
  again, `RevokedByProtocol`; only new code can be attested. An attestation is also not current
  while the program's `Timelock` holds a proposal of other code.)

**Why `programdata_slot` is not enough:** `ExtendProgram` is permissionless and resets the slot.
So anyone can make an attestation look stale for about 0.07 SOL, though never make a stale one
look current. Readers therefore treat an equal slot as a fast "unchanged", and otherwise recompute
the executable hash of ProgramData and compare it with `build_hash` (§7).

### 6.3 The CLI (as built: the `bordrless` bin of `@bordrless/sdk`, `src/cli/bordrless.ts`)

| Command | Does |
|---|---|
| `bordrless hook build` | `solana-verify build` in `solanafoundation/solana-verifiable-build:4.3.0` against the Studio template's pinned `Cargo.lock`; prints the executable hash |
| `bordrless hook sim` | Runs `studio-sim` locally (a published Docker image with the protocol binaries pinned by `SHA256SUMS`, the same image as the worker's); same report as Studio. A developer tool: it proves nothing to anyone else. |
| `bordrless hook deploy --immutable \| --timelock 3d` | Deploys, then finalizes or `register`s a timelock in the next transaction. Refuses to leave an author-upgradeable program, because the launchpad would refuse it. Uploads the OtterSec verify PDA (`solana-verify export-pda-tx`). |
| `bordrless hook propose <buffer>` / `execute` / `cancel` / `finalize` / `lengthen` | The timelock |
| `bordrless hook verify` | Uploads the source to Studio's `POST /studio/verify`. The worker rebuilds it at the given template commit, compares the hash with the chain, runs the static checks, the simulator and the review, and, if all pass, has the attester write `HookAttestation`. Rate-limited and paid like a Studio build. |
| `bordrless strategy …` | The same set for strategies, with `studio-sim strategy` |
| `bordrless hook prepare / config` | The registry and a `LaunchConfig`, through the SDK's existing `buildCreateConfig` |

## 7. Risk labels for terminals

### 7.1 The function

```ts
export interface HookRiskLabel {
  program: PublicKey;
  class: 'immutable' | 'timelocked' | 'managed' | 'author' | 'missing';
  delaySecs?: number;                 // timelocked
  pending?: { hash: string; eta: number; buffer: PublicKey } | null;  // a proposal waiting
  timelockProgramUpgradeable?: boolean;
  audited: 'current' | 'stale' | false;   // HookStatus.audited and audited_hash vs the current executable hash
  blocked: boolean;                     // HookStatus.blocked (a companion game's kill switch)
  potCap: bigint | null;                // the companion's cap for games and strategies
  provenance: 'protocol' | 'studio' | 'unattested' | 'none';
  studio?: { buildHash: string; simPass: boolean; cutMaxBps: number; current: boolean };
  verifiedSource?: { repo: string; commit: string } | null;   // OtterSec verify PDA
  severity: 'low' | 'medium' | 'high';
  words: string;                        // one sentence, the same on every surface
}
export async function hookRiskLabel(connection: Connection, program: PublicKey,
  opts?: { hash?: 'auto' | 'always' | 'never' }): Promise<HookRiskLabel>;
export function riskLabelOf(accounts: RiskAccounts, now: number, executableHash?: string): HookRiskLabel;
```

One `getMultipleAccounts` reads:

- the program, its ProgramData, `Timelock` and `hook_timelock`'s own ProgramData;
- `HookStatus` and `HookAttestation` (the companion);
- the OtterSec PDA.

The executable hash (a download of the ProgramData) is computed only when an attestation or an
audit hash needs comparing and the slot differs (`'auto'`). Terminals cache it by ProgramData slot.
(As built, round 1: the program and both ProgramData accounts are read as 48-byte headers, the code
is downloaded only when a hash is needed, and the cache is keyed by the deploy slot of the v3
ProgramData or the loader-v4 header.)
`riskLabelOf` is pure and is held to `programs/tests/vectors/risk-labels.json`, rendered from
`bordrless_hook::authority` like the existing vectors.

### 7.2 Precedence and words

1. `missing`: "This hook's program is gone: every transfer fails." (Also an upgradeable program
   whose ProgramData was closed.)
2. `blocked`: "Bordrless blocked this game's code: its pot goes to buyback and burn." (high)
3. A program handed to its timelock's address with no `Timelock` there: class `author`, "Bordrless
   refuses this hook: its upgrade key was handed to a timelock address that was never set up."
   (high; independent audit, finding 8: the launchpad refuses it with `HookTimelockInvalid` and the
   companion can't class it, so it never reads as immutable.)
4. `author`: "Its owner can change this code at any time." (high)
5. `timelocked` with a pending proposal: "Its author has proposed new code (hash …), live from
   <date>." or "…, executable now." (high)
6. `audited: 'current'`: "Audited." (low)
7. `timelocked`: "Its author can change this code with N days of public notice. Bordrless hasn't
   checked it." With a current Studio attestation: "… Checked automatically by Studio, not
   audited." Then "The timelock itself can still be changed by Bordrless." while that is true.
   (medium; independent audit X5: a notice period says nothing of what the code does, so a
   timelocked hook carries the same "not checked" words as immutable code.)
8. `managed`, `provenance: 'studio'`: "Built and checked automatically by Bordrless Studio, not
   audited. Bordrless can change it." (medium)
9. `managed`, `protocol`: "Bordrless's own code, not audited here. Bordrless can change it." Else
   `unattested`: "Upgradeable by a Bordrless key; not built by Studio." (medium)
10. `immutable`: "Nobody can change this code. Bordrless hasn't checked it." Or, with a current
    attestation: "Nobody can change this code. Checked automatically by Studio, not audited."
    (medium)

**Callees** (SDK, independent audit X5): given the coin's mint (`hookRiskLabel(…, { mint })`), the
label reads the hook's registry for it and classes every executable account it names by key other
than the hook itself and Bordrless's and the runtime's programs (`label.callees`). The label's class
is the weakest of the hook's and its callees' (author < timelocked < managed < immutable), its
severity at least the weakest callee's (author: high; timelocked, managed: medium), and its words
end "It can call <key>…, whose owner can change it at any time." (or "…, whose author can change
it after a public delay." / "…, which Bordrless can change."). The Rust reference labels one
program and has no callee vectors; the SDK's tests cover this part. Studio refuses a registry naming
another program, statically and on the dry run of `prepare`.

For strategy games, the token page shows both labels (the ticket hook's and the strategy's) and
the pot cap that results.

### 7.3 The integration guides (`bordrless-sdk/docs/integration`)

- **05-hooks-and-risk.md:**
  - Replace "Who can upgrade a hook" (its table and the
    `upgradeAuthority === STUDIO_UPGRADE_AUTHORITY` snippet, which mislabels donated programs)
    with `hookRiskLabel`, the classes and the precedence.
  - Add "Timelocked hooks" (events, pending upgrades, what a terminal should show).
  - Add "Strategy coins" under "Game coins".
  - Add "Where a hook's cut goes" (vault slots and their policies).
- **02-reading-tokens.md:** the `Strategy` kind, `StrategyTerms`, receipts.
- **03-indexing-trades.md:**
  - the new events: `hook_timelock` (`UpgradeProposed`, `Upgraded`, …), the companion's
    (`PeriodPlanned`, `PeriodRejected`, `StrategyPaid`, `CandidateRejected`, `HookAttested`) and
    `hook_vault`'s;
  - trust only events emitted through each program's event authority, never a strategy's logs.
- **07-other-languages.md:** the label algorithm as a byte-level recipe, with the vectors.

## 8. Program changes, per program

| Program | Change | Why | Size (rent at mainnet's 5,080 lamports a byte; as built: §17) |
|---|---|---|---|
| `hook_timelock` | **new** | §3 | 241,832 B as built: ≈ 1.23 SOL of ProgramData rent. Deployed at its exact size, then finalized after the audit. |
| `bordrless_launch` | upgrade: the timelock branch in `check_hook_authority`, `HookTimelockInvalid`, `HookTimelockPending` (independent audit X1) | `HOOK_UPGRADE_AUTHORITIES` is a constant, and the timelock PDA is per program | 552,264 B as built: fits mainnet's 554,240-byte ProgramData, no extension |
| `bordrless_companion` | upgrade A (3a.1): `classify` in game vetting, `set_hook_status_v2`, `HookAttestation` (`attest`, `revoke`). Upgrade B (3a.3): `GameKind::Strategy`, `StrategyTerms`, `create_strategy_game`, `plan_period`, `pay_strategy`, the strategy branch of `retire`. Every existing instruction byte-identical. | §2, §4, §6 | A: +15–25 KB. B: +60–90 KB. At 10 KiB minimum steps, ≈ 0.6–0.8 SOL of extension in all. |
| `hook_vault` | **new** | §5 | 341,984 B as built: ≈ 1.74 SOL |
| `bordrless_token`, `bordrless_swap`, `bordrless_bridge`, `bordrless_kit`, `lottery_hook`, `half_life`, `tax_hook` | **none** | | |
| crate `bordrless-hook` | `authority` module | shared classification | |
| crate `bordrless-strategy` | new | §4.6 | |

Order: deploy `hook_timelock` (its id becomes a constant), then upgrade the launch, then
companion A, then companion B, then deploy `hook_vault`. Each upgrade is preceded by its
`solana program extend` and a verified build.

## 9. Phasing inside 3a

| Step | Ships | Depends on | Why this order |
|---|---|---|---|
| **3a.1: honest labels and timelocks** | `hook_timelock`; the launch upgrade; companion A; SDK `hookRiskLabel` and the timelock builders; guide 05; Studio's attester and its backfill of existing Studio hooks | audit 1 | Smallest surface, and every later step needs it. Opening deployment to outsiders without honest labels would be backwards. |
| **3a.2: open deploy** | `@bordrless/cli`; Studio `POST /studio/verify`; timelock events in the watcher | 3a.1 live | Off-chain work, no new program |
| **3a.3: strategies** | companion B; `bordrless-strategy`; the Studio `strategy` kind, its starters, checks and simulator; the keeper's plan/pay loop; site | audit 2 | The CPI into untrusted code is the riskiest piece and gets its own audit round |
| **3a.4: deferred actions** | `hook_vault`; the Studio registry rule; the launch form's vault policies; the keeper's crank | audit 3 | A new money-moving program with swap legs. Can move to 3b without blocking anything above (open question 6). |

## 10. SOL and rent

The figures are for mainnet at **5,080 lamports per byte** (rent-exempt, including the 128 bytes of
account overhead): read from mainnet's rent sysvar on 2026-10-10 (`solana -um rent 0` answers
0.00065024 SOL = 128 × 5,080 lamports). Earlier drafts of this document used 6,960 (the old
3,480 × 2 years); read the rent sysvar again before deploying. Sizes are the local builds of §17.

| Item | One-off | Notes |
|---|---|---|
| `hook_timelock` deploy | ≈ 1.230 SOL | ProgramData (241,832 + 45 B) and the program account; plus a buffer of the same size while deploying (≈ 1.229 SOL, returned) |
| `hook_vault` deploy | ≈ 1.739 SOL | plus ≈ 1.738 SOL of buffer while deploying, returned |
| Launch upgrade | 0 | 552,264 B fits the 554,240-byte ProgramData; buffer ≈ 2.806 SOL while it exists |
| Companion extension | ≈ 1.153 SOL | `ExtendProgram` by 227,000 B (656,936 → 883,936); buffer ≈ 4.491 SOL while it exists |
| **Total spent** | **≈ 4.12 SOL** | plus fees (about 0.01–0.05 SOL with a priority fee) |
| **Peak balance needed** | **≈ 6.9 SOL** | at the companion step, run in the order of §17's runbook with each buffer closed into its upgrade before the next |

| Per use | Who pays | Cost |
|---|---|---|
| `Timelock` (279 bytes) | the author | ≈ 0.00207 SOL |
| A proposal's buffer | the author | (the program's size + 165) × 5,080, returned on execute, cancel or expiry |
| `StrategyTerms` (~145 bytes) | the launcher | ≈ 0.0014 SOL |
| Strategy receipts | the sender | (126 + 128) × 5,080 = 1,290,320 lamports each, returned |
| `HookAttestation` (224 bytes) | Studio's attester | 1,788,160 lamports (≈ 0.0018 SOL) per program |
| `Vault` and 3 slots (coin and bridged-SOL holdings) | the launcher (the vault) and its creator (the holdings, at `open_vault`) | ≈ 0.004 SOL + ≈ 0.0017 per holding |

## 11. Threat model

| Actor | Wants | Stopped by |
|---|---|---|
| A hook author with a timelock | Swap in malicious code quietly | The proposal is public with its hash for ≥ 3 days, and labels show it as high severity. They can't shorten the delay or move the authority (§3.3). **Residual:** holders can only exit if the current hook lets them sell, so a hook that is already a honeypot gains nothing from the notice. |
| The same author, with an audited hook | Get an uncapped pot, then upgrade | A timelocked program can't be audited (§2.3). A label shows a stale audit. |
| A thief of the author's key | Propose malicious code, or cancel the real author's proposals | The delay gives the same notice. The author role can be a multisig. Nobody can recover a lost role, by design. |
| Bordrless | Bypass a timelock | Only by upgrading `hook_timelock` → finalize it after the audit (§3.5). Until then the label says so. |
| A donor of authority to `CS1NRy…` | Look Studio-built, enter the companion without review | Provenance comes only from attestations (§2.4). The companion's strategy and game acceptance treats the hook as managed: capped, blockable, and Bordrless can replace the code. Owner decision 7 (§0a): a game hook taken without a status also needs a current Studio attestation, so a donated program does not enter a game. |
| Anyone | Make an attestation look stale | `ExtendProgram` changes the slot but not the hash. Readers fall back to the hash. |
| A crank | Bias a strategy's payouts by choosing candidates | `entitle` sees one candidate. Omitting someone changes no one else's amount, only the order in which the budget runs out. Receipts stop repeats. |
| A crank | Front-run payments to exhaust the budget | Inherent to first-come; strategies sized to the total (simulator-checked) leave nothing to race for |
| A strategy author | Pay their own wallets | The per-holder cap, the budget cap and the 10 SOL pot cap; weight held since the period began; the block switch; the label; Studio's "favours a fixed address" review |
| A strategy | Write, move funds, re-enter, spoof answers, starve the crank of compute | Read-only metas and no signer; no program account passed (no CPI at all); return data's program checked; class re-checked before every question; compute not metered (§17) |
| A sybil | Split a balance to get several entitlements | `min_weight`. A pro-rata strategy gains nothing from splitting; a flat-per-holder strategy does, so the simulator flags it and the review names it. |
| A sandwich bot | Profit from the vault's sells and buys | The ported buyback guards: slices smaller than the round-trip fees, a reference price, 2% slippage |
| A hook | Strand vault funds by refusing the vault's moves | `retire` burns after 60 days |
| A griefer | Delay a timelock's `execute` | `ExtendProgram` in the same slot blocks one slot only |

## 12. Audit plan

Three rounds, in the shape of the companion's (an internal pass, an independent audit, a
verification of the fixes):

1. **`hook_timelock`, with the launch and companion-A diffs.** Focus:
   - the authority invariant over every instruction and every state;
   - that buffers are immutable from proposal to execution;
   - the spill destination;
   - the length and trailing-zero rule of the proposal hash, against `solana-verify`;
   - `classify` byte parsing (loader v3 and v4 headers, forged ProgramData and Timelock accounts,
     another program's PDA);
   - `set_hook_status_v2`'s refusals;
   - `attest` and `revoke` signers.
   Then make `hook_timelock` immutable.
2. **Companion B (strategies).** Focus:
   - privilege on the CPI (every meta read-only, no signer, no writable account a strategy could
     receive);
   - return-data provenance and length;
   - (compute metering: not built, §17);
   - heap at 4 candidates;
   - candidate validation (each exclusion; holdings forged or owned by another mint);
   - the receipt PDA collision with streak receipts (different `game`, so impossible; prove it);
   - the interplay of `pot_locked` with caps, blocks, `retire` and `claim_fees`;
   - the plan deadline and lapse;
   - the combined terms of two statuses;
   - that every phase-1 and phase-2 instruction is byte-identical (the test suite as is).
3. **`hook_vault`.** Focus:
   - the economics of the guard port (the companion's sandwich argument re-derived for sells);
   - PDA signer scope (a slot owner signs only for its own holdings);
   - that policies are immutable;
   - `open_vault` before and after the first delta;
   - interaction with hooks that take cuts from the vault's own moves, refuse them, or refuse
     burns;
   - depth on the buy leg through a kit launch;
   - `retire`.

## 13. Tests (LiteSVM suites in `programs/tests/tests`)

- **`timelock.rs`:**
  - register (authority moved, SetAuthorityChecked, refused for a non-authority);
  - propose (buffer authority must be the PDA; the hash equals `solana-verify`'s on the real
    `tax_hook.so`; the trailing-zero rule; over 2 MiB);
  - execute too early, at the eta, after the window;
  - the upgraded program runs its new code;
  - cancel, expire and reclaim refund the author;
  - lengthen moves a pending eta; shortening is refused;
  - finalize makes the program immutable;
  - a property test: random instruction sequences never leave an authority other than the PDA or
    `None`;
  - extend-then-execute; a same-slot extension blocks only that slot;
  - a multisig author through CPI.
- **`hook_authority.rs`** (extended):
  - `create_config` takes a timelocked hook (≥ 3 days);
  - it refuses a forged Timelock (wrong owner, wrong program, another program's PDA, a
    discriminator mismatch), a missing second account, and a loader-v4 program;
  - old clients (one remaining account) behave byte-identically.
- **`companion_strategy.rs`:**
  - creation bounds;
  - happy path over 3 periods;
  - every row of §4.4 with a test-only strategy (`programs/tests/fixtures/strategies/*`: panic,
    loop, garbage, nested return data, over budget, over compute, writes an account, CPIs the
    companion, reads an extra not owned by it);
  - every candidate exclusion;
  - duplicates within one transaction;
  - budget exhaustion order;
  - a block mid-period; a cap lowered mid-period;
  - retire after 60 days of rejected plans;
  - **transaction size ≤ 1,232 at `max_per_tx` with 2 extras, the heap measured with the
    instrumented allocator, compute at the CU caps, and depth through a router**.
- **`companion_attest.rs`:** attest, revoke, signers, and the label vectors.
- **`hook_vault.rs`:**
  - deltas into slots; each policy;
  - sandwich simulations mirrored from `launch_money.rs` and the companion's buyback tests;
  - a hook refusing the vault, then `retire`;
  - `open_vault` ordering;
  - a buy leg on a kit launch; depth.
- **Vectors:** `risk-labels.json`, `strategy.json` (instruction bytes), `timelock.json`, rendered
  from Rust and checked by SDK tests, as `companion-games.json` is today.

## 14. What Studio (the cofounder) must build

1. **The attester.** A new hot key and a worker step after `finishBuild`, which writes
   `HookAttestation` when the build, checks, simulator and review pass. Record the template commit
   with every build (`tools/studio-builder/README.md:31-35`). Backfill attestations for every
   deployed Studio hook, by rebuilding at its own commit; a mismatch gets no attestation and is
   flagged. Revoke an attestation when the watcher sees a hash change.
2. **The watcher** (`apps/server/src/keeper/hooks.ts`):
   - accept timelock PDAs as an authority;
   - on `UpgradeProposed`, fetch the buffer, simulate it, show a banner with the countdown, and
     write an attestation for the pending code if it passes, otherwise warn;
   - treat authority donated to `CS1NRy…` with no Studio project as "not Studio".
3. **Labels on the site.** Replace `hookAbout.ts`'s three kinds with `hookRiskLabel` words
   everywhere: the token page, the launch form, the marketplace and Studio.
4. **Deploy options** (owner decision 3: yes): immutable / timelocked (N ≥ 3 days, author
   = the creator's wallet) / managed (today's default).
5. **The `strategy` project kind:** the template with `bordrless-strategy`, the starters (§4.9),
   the static rule set, the `studio-sim strategy` scenario and its report in `StudioSim`, the
   review prompts, and the launch form for strategy coins (`lottery_hook` config, `StrategyArgs`
   with bounded sliders).
6. **Keepers:**
   - strategy `plan_period` at each period's start (with a fee claim when the pot is short);
   - `pay_strategy` in batches of `max_per_tx` where the bounty covers the fee and receipt yield
     (as built, round 1: skipped once the budget is spent; a failing batch retried one by one),
     as for streak claims; holders self-claim on the site;
   - `close_receipt`;
   - the vault crank;
   - timelock `execute` at the eta (anyone can, but someone should).
7. **`POST /studio/verify`** for the CLI: source in, rebuild, compare with the chain, attest.
   Rate-limited and paid.
8. **The registry rule** for vault slot holdings (`checks.ts:313-316`), and the vault policy UI.

## 15. Open questions for the owner (answered: §0a)

1. **The minimum delay.** 7 days (proposed); 3 days is friendlier to builders, 14 safer for
   holders. The maximum is 365.
2. **`hook_timelock` immutable on day one after its audit** (proposed), or upgradeable by the
   protocol for a bake period, with the label saying so meanwhile.
3. **Should Studio offer creators a timelocked authority over their own Studio-built hooks and
   strategies?** Today they never get one (`docs/studio.md`). Yes gives them v4-like ownership;
   no keeps "managed" as Studio's promise.
4. **Strategy caps:**
   - budget at most 50% of the pot per period;
   - at most 25% of a budget per holder;
   - 10 SOL pot while unaudited;
   - periods of 1 hour or more (a rigged strategy drains at most one budget per period; a 1-day
     minimum would slow that 24 times).
5. **Audits tied to code.** Refuse audits of timelocked programs and record the audited hash
   (proposed). Optionally also make the companion re-check the hash at `create_game*`.
6. **Deferred actions in 3a or 3b,** and whether `SellForSol` to a creator's wallet (a SOL tax
   collected through the hook's cut) is allowed at all, as a product and wording matter.
   Bordrless's 25% share of cuts makes it revenue-positive. Its alternatives today are worse:
   manual dumping of cuts.
7. **Phase-2 game hooks and authority donation:** require a current Studio attestation for
   `CS1NRy…`-class game hooks at `create_game*` (backfill first), or accept the bounded risk as
   today.
8. **The attester key** is a hot key on the worker. Its blast radius: labels and nothing else (no
   acceptance rule depends on it). Acceptable?
9. **Use `hook_timelock` for the protocol's own programs** (README "Upgrade authority",
   hooks-v2 §4.14)? It would make Bordrless's upgrades public and delayed, at the cost of delaying
   emergency fixes by the same N days.

## 16. Not in 3a

- Option A of `docs/studio-companions.md` (Studio companion programs holding funds).
- Strategies that rank candidates ("top N"), or that use randomness.
- Intent counters for hooks.
- Staking.
- CPI from callbacks.
- Custom pool hooks on launches.
- Any change to the token program or the DEX.

## 17. As built (2026-10-09, not deployed)

Everything above is built and tested in this repository and the monorepo, with the owner's
decisions of §0a. Nothing is deployed. Where the build differs from the design, this section wins.

### What changed from the design, and why

- **No compute metering of strategies on chain.** `sol_remaining_compute_units` is feature-gated
  and the feature (`5TuppMutoyzhUSfuYdhgzD47F92GL1g89KpCZQKqedxP`) is **inactive on mainnet**
  (`solana feature status`, read only, 2026-10-09). A program calling it fails, and a deploy of one
  would be refused. `plan_cu_max` / `entitle_cu_max` are therefore declared caps (keepers budget
  them, Studio's simulator holds a strategy to 70% of them); a strategy over the transaction's limit
  fails the transaction (§4.4 row 1). `AnswerFault::OverCompute` and the events' `cu` fields are
  gone.
- **A strategy can call nothing.** No executable account is passed to it (not even its own
  program), so it can make no CPI at all: "return data set by a program it called" can't happen.
  The suite asserts that a recursion or a call into the companion fails the transaction.
- **A strategy's extras are fixed at `create_strategy_game`.** The registry is read once, resolved
  against `[mint, game]` (`Seed::Account(0)` and `(1)`), at most 2 accounts, each owned by the
  strategy then and at every call (`StrategyTerms.extras`).
- **Receipts can't be blocked by pre-funding**: a receipt address someone funded is topped up,
  allocated and assigned, as Anchor's `init` does. A candidate whose account would stay below its
  rent-exempt minimum after the payment (an empty wallet paid too little, a legacy rent-paying one)
  is skipped (`BelowRent`) instead of failing the transaction.
- **`attest` checks the hash on chain.** The companion recomputes the executable hash from the
  ProgramData and refuses an attestation of code that is not deployed; it records the ProgramData's
  slot. An attestation is current while not revoked, passing, and of the code as it is (the same
  slot, else the hash recomputed: an `ExtendProgram` changes the slot, not the code).
- **Game hooks taken without a status** (Studio's key, the protocol's, or a timelock) need a
  current attestation (owner decision 7): `HookNotAttested` otherwise. `lottery_hook` is taken by id
  as before; a hook with a status needs none. The phase-2 suites now write Studio's attestations in
  their fixtures (the deployed instructions and vectors are unchanged; the client builders
  `create_game_v2_attested` / `create_game_attested` append the attestation).
- **Audits tied to code.** `set_hook_status_v2` needs the hook's program, ProgramData and timelock,
  refuses an audit of a timelocked or author-upgradeable program, recomputes the hash and keeps it
  in `HookStatus.reserved` (`audited_hash`). `set_hook_status` (v1) is unchanged for old clients;
  with the hook's ProgramData among its remaining accounts (`set_hook_status_checked`, what the SDK
  sends) it refuses the same audits. A v1 audit records no hash, which labels show as "stale".
- **`register(delay, author)`** takes the author as an argument (owner decision 3), so Studio's key
  can hand a program straight to its creator's timelock.
- **The vault is for launchpad coins** (the coin's launch's `custom_hook` is the vault's hook); a
  token made outside the launchpad is deferred. See `programs/hook_vault/src/lib.rs` and the log for
  the guard port's details.

### Changed by the adversarial review, round 1 (2026-10-10)

Reports: `bordrless-games-work/log-3a-audit-{custody,manip,integration,vault}-r1.md`; each PoC
(`programs/tests/tests/audit_3a_*_r1.rs`, the SDK's and the server's `audit3a.test.ts`) now
asserts the fixed behaviour.

- **Strategies.**
  - The strategy's class is read again before every `plan` and `entitle` (its ProgramData and
    `Timelock` follow the fixed accounts); handed to an outside key, it is refused
    (`StrategyNotAccepted`). A status never lets an author-upgradeable strategy in, and a
    strategy's audit lifts its cap only with a recorded hash (v2).
  - A refused plan answer leaves the period unplanned (asked again until its deadline).
  - `pay_strategy` asks every candidate first, then makes the receipts and payments; a candidate
    repeated in one payment is `AlreadyPaid`.
  - A payment refreshes `settled_at` only once its period has paid `STRATEGY_ACTIVE_BPS` (1%) of
    the pot.
  - The starter's `prepare` needs the mint's signature.
- **Attestations.** Only a kind-1 (game hook) attestation vets a game hook; a protocol
  revocation sticks to the build (`RevokedByProtocol`, error 6065, appended); an attestation is not
  current while the `Timelock` holds a proposal of other code.
- **Hashing.** `trimmed_len` scans the zero tail in chunks with `sol_memcmp` (anyone can grow a
  ProgramData to 10 MiB with zeros): `create_game_v2` with the hash recomputed at 10 MiB costs
  263k units (it was over 1.4M after +1 MiB), `attest` 154k, `set_hook_status_v2` 173k.
- **Audits.** v1 keeps a hash v2 recorded. v1 as deployed still audits without the program's
  accounts (the protocol's operators use v2; kept for old clients and the phase-2 suites).
- **Classes.** Only an account owned by `hook_timelock` is read as a timelock (the launch's error
  for an author key passed second is `HookUpgradeable` again). Labels: a closed program is
  `missing`; one handed to its timelock's address with no `Timelock` is frozen (`immutable`).
- **Accepted, not changed on chain:** a planner can plan and pay in one transaction; a strategy
  that depends on candidate order (first come) is unfair, so Studio's simulator checks that
  amounts don't depend on order, and the first-N starter is dropped (M-1). `since` is the first
  receive, not "held since": a loyalty starter must not use it (M-3). The strategy reads the
  game's and companion's bytes as they were before the step: read the arguments (I8).
  `finalize` needs SBPF v3 code once `disable_sbpf_v0_v1_v2_deployment` is active: the CLI refuses
  to put other code behind a timelock (I7).

### Changed by the adversarial review, round 2 (2026-10-10)

Reports: `bordrless-games-work/log-3a-audit-r2.md` (companion) and
`log-3a-audit-r2-vault-integration.md` (vault, SDK, keeper, server). PoCs `audit_3a_r2.rs` and
`audit_3a_vault_r2.rs` now assert the fixed behaviour.

- **Audits follow the code.** A strategy's audit counts only while the strategy is immutable or
  Bordrless-managed (read before every step): handed to a timelock, its pot is capped again.
  `set_hook_status_v2` may lift an audit it shows stale (the program no longer auditable, its code
  not the recorded hash, or no hash recorded) and cap or block the hook in the same call; an audit
  is otherwise final, and v1 can never lift one. Labels show such an audit as stale.
- **A dormant strategy always retires.** Activity is what the game paid since it last counted as
  active (`StrategyTerms.paid_at_active`), at least 1% of the pot over however many periods; past
  `retirable_at`, `retire` closes an open period, and a strategy's plans never hold it back.
- **A protocol revocation is a hold on the program**: `attest` refuses the program from then on,
  whatever code it runs (the protocol can still give it a status).
- **The starter's `prepare`** takes over a registry address someone funded first.
- **Vault**: each selling slot gets its own part of the vault's slice per interval (no slot can
  starve another into retirement), and a wait's cooldown is short (see the log).

### Changed by the adversarial review, round 3 (2026-10-10)

Report `bordrless-games-work/log-3a-audit-r3.md`; PoCs `audit_3a_r3.rs` now assert the fixes.

- A strategy's class accounts (ProgramData, `Timelock`) left out or forged are refused before any
  terms apply in `plan_period` and `pay_strategy` (they could void an audit and trim an uncapped
  pot on a no-ticket or lapsed period).
- A strategy's audit holds only while its code is the recorded hash: `create_strategy_game` and
  `plan_period` recompute the hash when the code's deploy slot moved since it last held
  (`StrategyTerms.audit_ok` / `audit_slot`); a payment takes the plan's check as it is, so no
  `ExtendProgram` can change a payment's terms.

### Changed by the independent audit (2026-10-10)

Reports: the independent manipulation and integration reviews; PoCs
`programs/tests/tests/indep_3a_{manip,integration}.rs` now assert the fixes (log:
`bordrless-games-work/log-3a-fix.md`).

- **X1/X2, "N days of notice" must be whole (Medium).** A timelocked program is accepted only while
  its `Timelock` holds no proposal:
  - the launch: `create_config` (and `create_listed_config`) refuse a timelocked custom hook with a
    proposal pending or executable (`HookTimelockPending`, 6045, appended); a config made on a
    timelocked hook records it (`LaunchConfig.reserved[0] = TIMELOCKED_HOOK`), and **every
    `create_launch` from such a config passes the hook's `Timelock` as its last account** and is
    refused while it holds a proposal. One account (+33 bytes): a jackpot or streak coin's launch
    through the companion at the site's longest metadata measures 1,210 of 1,232 bytes with it.
    Configs made before (or on any other hook) are read and launched exactly as before.
  - the companion: `create_strategy_game` refuses a timelocked strategy whose `Timelock` holds any
    proposal (`TimelockPending`, 6066, appended); a status-less game hook was already refused while
    its timelock holds other code than the attested build (`attestation_current`).
  - **Residual (documented, `x1_residual_*`):** a config made while its hook was Bordrless-managed
    is not flagged; if Studio's key later hands that hook to a creator's timelock, launches from the
    old config don't read the timelock. Only Bordrless's key can make that move, so Studio registers
    a "timelocked by me" hook's timelock **before** making any config; the server refuses to build a
    launch while the hook's timelock holds a proposal; the label reads high. A game hook given a
    status by the protocol is the protocol's own vetting: don't write one for a timelocked hook with
    a proposal pending (and block it if one appears).
  - A proposal of the *same* code is refused too (comparing it with the code on chain would cost a
    hash of the program): the author cancels it first.
- **X3, the vault's references (Low).** `open_vault` is the vault creator's (`Vault.creator`) or needs
  the mint's keypair (`NotOpener`, 6026, appended), and the sells' reference opens at
  `min(pool price, the launch's opening price)`: a pump around the open (a sandwich of the
  creator's own open included) can't poison it; each sale then raises it 5% at most. The launch and
  `open_vault` don't fit one transaction (measured): the creator sends `open_vault` next.
  Residual: a buy slot's reference is the other pool's price at the open (a sandwich of the
  creator's open can start it low; buys then wait while it catches up).
- **X4, a game hook's audit follows its code (Low).** An audit recorded with a hash
  (`set_hook_status_v2`) lifts a game hook's cap only while the hook is immutable or
  Bordrless-managed and runs that code, as a strategy's (R3-F2): every step that applies the hook's
  terms (game creation, `claim_fees`, the lottery's, jackpot's and streak's steps, `retire`,
  `plan_period`) needs the hook's ProgramData (`ProgramAccounts` without it: leaving it out neither
  keeps the audit nor trims an audited pot), rehashes the code only when its deploy slot moved since
  `Game.hook_audit_slot` (from `Game.reserved`, 43 → 34), and caps the pot at 10 SOL when it fails.
  `pay_strategy` takes its plan's check. A v1 audit (no hash, as deployed) is read as before.
- **X5 (labels).** See §7.2: timelocked and immutable labels say Bordrless hasn't checked the code
  unless a current attestation exists; a program handed to its timelock address with no `Timelock`
  reads as refused, not immutable; `hookRiskLabel(..., { mint })` reads the hook's registry and
  takes the weakest class of any program it can call.
- **Integration.** The CLI sets a compute limit on every transaction (propose: `ceil((15,000 +
  len/2) × 1.15)`, a 2 MiB proposal measured at 1,057,101 units) and takes `--priority-fee`;
  `pnpm admin attest|revoke-attestation` and Studio's deploy pass write attestations; the pro-rata
  starter stores its state's bump (its `plan` and `entitle` cost the same on every mint: 4,107 and
  4,147 units over 32 mints); `programs.sh` and `deploy.sh` know `hook_timelock` and `hook_vault`.

### Sizes (SBPF v3, local `cargo build-sbf`; the deploy uses the verifiable Docker build)

| Program | `.so` (bytes) | Mainnet ProgramData | Needed (at 5,080 lamports a byte) |
|---|---|---|---|
| `bordrless_companion` (upgrade) | 883,936 | 656,936 | `ExtendProgram` +227,000 B (≈ 1.153 SOL) |
| `bordrless_launch` (upgrade) | 552,264 | 554,240 | none (fits) |
| `hook_timelock` (new) | 241,832 | | ≈ 1.230 SOL rent |
| `hook_vault` (new) | 341,984 | | ≈ 1.739 SOL rent |

Rent is 5,080 lamports a byte on mainnet (plus 128 bytes of account overhead; read 2026-10-10).
Permanent cost ≈ 4.12 SOL; upgrade buffers (refunded) need ≈ 4.49 SOL (companion) and ≈ 2.81 SOL
(launch) while they exist; write transactions ≈ 0.01 SOL. Each Studio attestation account ≈ 0.0018
SOL (the attester pays). `bordrless_token`, `bordrless_swap`, `bordrless_bridge`, `bordrless_kit`,
`tax_hook` and `lottery_hook` rebuild to their mainnet hashes (`pub mod authority;` is declared last
in `bordrless-hook`'s `lib.rs` so no panic location above it moves).

### Deploy runbook (verified order; nothing deployed yet)

Each step with a verifiable build, `solana program show` before and after, and the deployer's
balance checked against the peak below. `scripts/solana/deploy.sh` takes `KEYS_DIR` (mainnet:
`~/.config/solana/bordrless-mainnet`) and `BUFFER_DIR`; the plain `solana program deploy` /
`extend` commands are equivalent.

| # | Step | Why here | SOL (needed at the step / spent) |
|---|---|---|---|
| 0 | Pause Studio game launches (the handover's F-I1) | the companion upgrade needs attestations for status-less Studio game hooks | 0 |
| 1 | Deploy `hook_timelock` (`BBUzaam…`) | its id is a constant in the launch and the companion | 2.46 / 1.23 |
| 2 | Upgrade `bordrless_launch` (fits; no extension) | reads `hook_timelock` accounts | 2.81 temporarily / 0 |
| 3 | `solana program extend` the companion by 227,000 B, then upgrade it | attestations and strategies; the extension first or the upgrade fails | 5.64 / 1.15 |
| 4 | Fund the attester (≈ 0.1 SOL) and backfill attestations (`pnpm admin attest <program> game --commit <c>` per deployed Studio game hook; `token` for token hooks) | only the upgraded companion writes them | 0.1 / ≈ 0.0018 each |
| 5 | Resume Studio game launches | | 0 |
| 6 | Deploy `hook_vault` (`5cojoU…`) | a new program nothing depends on | 3.48 / 1.74 |

**Peak balance ≈ 6.9 SOL** (step 3: 1.23 already spent + 1.15 extension + 4.49 buffer), ≈ 4.12 SOL
spent in all, plus the attester's 0.1 SOL and fees. Close any leftover buffer
(`solana program show --buffers`, `solana program close <buffer>`).
