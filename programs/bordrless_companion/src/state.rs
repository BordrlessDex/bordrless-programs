//! A launch's companion (`docs/companions.md`).

use anchor_lang::prelude::*;

use crate::constants::*;

/// What every creator fee claim pays for, in basis points summing to 10,000.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct Split {
    /// Bought back on the token's own pool and burned.
    pub buyback_bps: u16,
    /// Streamed to holders through the kit's reward pool.
    pub holders_bps: u16,
    /// Accrued to the beneficiary, who is paid it as SOL.
    pub beneficiary_bps: u16,
}

impl Split {
    pub fn valid(&self) -> bool {
        self.valid_with_pot(0)
    }

    /// With a game's pot share (`Companion.pot_bps`, kept outside this struct so a companion made
    /// before games reads 0): the four parts sum to 10,000.
    pub fn valid_with_pot(&self, pot_bps: u16) -> bool {
        u64::from(self.buyback_bps)
            + u64::from(self.holders_bps)
            + u64::from(self.beneficiary_bps)
            + u64::from(pot_bps)
            == BPS
    }
}

/// `PDA(["companion", mint])`.
#[account]
#[derive(InitSpace)]
pub struct Companion {
    pub version: u8,
    pub bump: u8,
    /// Bump of `PDA(["creator", mint])`, the launch's creator.
    pub creator_bump: u8,
    pub mint: Pubkey,
    /// Who launched it: receives the beneficiary's part of the fees and the vested dev bag.
    pub beneficiary: Pubkey,
    pub split: Split,
    /// What a step pays whoever sends it, of what it moves (at most `MAX_BOUNTY_BPS`).
    pub bounty_bps: u16,
    /// The most one buyback spends (lamports of bridged SOL).
    pub max_buyback: u64,
    /// The least time between buybacks.
    pub buyback_interval: i64,
    /// The dev bag vests linearly over this many seconds from the launch (0: at once).
    pub vest_secs: i64,
    pub launched: bool,
    pub launched_at: i64,
    /// Tokens bought for the dev bag, and released to the beneficiary so far.
    pub dev_tokens: u64,
    pub dev_released: u64,
    /// Bridged SOL held for each purpose (in the creator's holding).
    pub pending_buyback: u64,
    pub pending_holders: u64,
    pub pending_beneficiary: u64,
    pub last_buyback_at: i64,
    /// Running totals.
    pub claimed_total: u64,
    pub spent_total: u64,
    pub burned_total: u64,
    pub shared_total: u64,
    pub paid_beneficiary_total: u64,
    pub bounties_total: u64,
    /// The buyback's reference price (quote per base unit times `PRICE_SCALE`), set at the launch and
    /// moved toward the pool's price by at most `REFERENCE_STEP_BPS` at a time, once an interval.
    pub reference_price: u128,
    pub reference_at: i64,
    // ---- v2 (games), taken from what was `reserved: [u8; 64]`, so the account keeps its size and
    // every companion made before reads zeros: no game, no pot. ----
    /// The game's token hook (`create_game`); the default key for a companion without a game.
    pub game_hook: Pubkey,
    /// The pot's part of every fee claim, next to `split` (the four sum to 10,000).
    pub pot_bps: u16,
    /// Bridged SOL held for the game's pot (in the creator's holding).
    pub pending_pot: u64,
    /// The game's round length, which its hook's header must state (checked at the launch).
    pub round_secs: u32,
    /// When `burn_stranded` last burned a blocked game's buyback, a blocked game's pot last moved
    /// into the buyback (by any step: `enforce_terms`), or a blocked game's fee claim last credited
    /// the buyback at least what it held (`Companion::restart_stranded_wait`) (0: never): the
    /// next burn waits its whole period from here, so a pot moved, or a share credited to a
    /// buyback that had spent what it was given, is never burned before keepers have had that
    /// long to spend it.
    pub stranded_burned_at: i64,
    // ---- v2.1 (jackpot and streak), from what was the last 10 bytes of `reserved`, zero in every
    // companion made before: a lottery's kind, nothing locked. ----
    /// The game's kind (`create_game`), which the launch checks the hook's flags and header by.
    pub game_kind: GameKind,
    /// The part of `pending_pot` a closed streak epoch still owes its holders: a cap lowered since
    /// never trims it (`steps::enforce_terms`), and only a block moves it to the buyback. Zero for
    /// every other kind.
    pub pot_locked: u64,
    /// `MODE_CLASSIC`, `MODE_SNOWBALL` or `MODE_SCRIPT` (2026-10-12), the last byte of what was
    /// `reserved`: zero, classic, in every companion made before.
    pub mode: u8,
}

impl Companion {
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    pub fn address(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[COMPANION_SEED, mint.as_ref()], &crate::ID)
    }

    pub fn creator(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[CREATOR_SEED, mint.as_ref()], &crate::ID)
    }

    /// Whether it runs a game (`create_game`).
    pub fn is_game(&self) -> bool {
        self.game_hook != Pubkey::default()
    }

    /// Whether it is a Snowball (`create_snowball`): its pot trades the token.
    pub fn is_snowball(&self) -> bool {
        self.mode == MODE_SNOWBALL
    }

    /// Whether it is a script companion (`create_script`): an approved builder's program decides
    /// what its pot does.
    pub fn is_script(&self) -> bool {
        self.mode == MODE_SCRIPT
    }

    /// Whether it is a classic companion (a game's included): neither a Snowball nor a script.
    pub fn is_classic(&self) -> bool {
        self.mode == MODE_CLASSIC
    }

    /// Whether its pot is its own, with no game hook to cap or block it: a Snowball's or a
    /// script's.
    pub fn has_own_pot(&self) -> bool {
        self.is_snowball() || self.is_script()
    }

    /// Everything set aside in the creator's holding of bridged SOL.
    pub fn set_aside(&self) -> Option<u64> {
        self.pending_buyback
            .checked_add(self.pending_holders)?
            .checked_add(self.pending_beneficiary)?
            .checked_add(self.pending_pot)
    }

    /// Since when a blocked game's buyback has neither bought nor waited (`burn_stranded`): the
    /// latest of the launch, the last buyback, the reference price's last move (a buyback that
    /// waited), the hook status's last write (`status_updated_at`) and the last burn, move of a
    /// blocked game's pot into the buyback, or fee claim that at least doubled it.
    pub fn stranded_since(&self, status_updated_at: i64) -> i64 {
        self.launched_at
            .max(self.last_buyback_at)
            .max(self.reference_at)
            .max(status_updated_at)
            .max(self.stranded_burned_at)
    }

    /// Under a blocked hook, a fee claim at `now` that found the buyback holding `held` and
    /// credited it at least as much again restarts the stranded wait (`stranded_burned_at`), so
    /// `burn_stranded` can't take that share before keepers have had the whole wait to spend it.
    ///
    /// - A working hook's buybacks spend what they are given. Once they have spent it all there is
    ///   nothing to buy, so for a while nothing buys and the wait runs on; the next claim credits a
    ///   buyback that is empty (or holds only dust), and the wait restarts with it. Without this, a
    ///   claim after a quiet month could be burned in its own transaction, or the next one, before
    ///   any buyback was tried.
    /// - Nobody can put a refusing hook's burn off for ever with it. Its buyback never empties (no
    ///   buyback lands), so a claim restarts the wait only if it credits at least what the buyback
    ///   already holds. Only fees paid to the game (or bridged SOL given to the creator address)
    ///   are credited, and every credit joins what is burned: keeping `v` in the buyback from its
    ///   burn for one more wait takes a credit of at least `v`, after which the buyback holds at
    ///   least `2v`, so the next wait costs twice as much. Credits smaller than what it holds (a
    ///   griefer's dust trade or donation, fees as they come in) restart nothing.
    /// - A credit smaller than what the buyback holds gets less than the whole wait only when the
    ///   buyback has held something for that long without a buyback landing or waiting: a hook
    ///   that refuses, or keepers away (which can't be told apart).
    ///
    /// The only other credit a blocked game's buyback takes is its pot's one move
    /// (`steps::enforce_terms`), which restarts the wait whatever its size: a pot is never refilled
    /// under a block. (`retire` credits the buyback only while the hook is not blocked; a block's
    /// own write restarts the wait.)
    pub fn restart_stranded_wait(&mut self, held: u64, now: i64) {
        let credited = self.pending_buyback.saturating_sub(held);
        if credited > 0 && credited >= held {
            self.stranded_burned_at = self.stranded_burned_at.max(now);
        }
    }

    /// When a blocked game's buyback may be burned as SOL: `STRANDED_SECS`, or
    /// `STRANDED_INTERVALS` buyback intervals when longer, after [`Companion::stranded_since`].
    pub fn stranded_at(&self, status_updated_at: i64) -> i64 {
        let wait = STRANDED_SECS.max(STRANDED_INTERVALS.saturating_mul(self.buyback_interval));
        self.stranded_since(status_updated_at).saturating_add(wait)
    }

    /// Tokens of the dev bag vested by `now`.
    pub fn vested(&self, now: i64) -> u64 {
        if !self.launched {
            return 0;
        }
        if self.vest_secs <= 0 {
            return self.dev_tokens;
        }
        let elapsed = (now - self.launched_at).clamp(0, self.vest_secs);
        (u128::from(self.dev_tokens) * elapsed as u128 / self.vest_secs as u128) as u64
    }
}

/// The kinds of game a companion runs. Borsh writes the variant's index, so a kind added later is
/// appended and every game made before keeps its own (a companion made before phase 2 reads 0,
/// the lottery, in `Companion.game_kind`).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub enum GameKind {
    /// A verifiable draw each round, weighted by tokens held (game ticket standard v1).
    Lottery,
    /// The last qualifying buyer wins once the timer runs out (`bordrless_game::jackpot`).
    Jackpot,
    /// Each epoch's share goes to the holders who held through it without sending
    /// (`bordrless_game::streak`).
    Streak,
    /// Phase 3a: each period, a builder's strategy program decides the budget and each holder's
    /// amount (`bordrless_strategy`), within the companion's bounds; tickets from a lottery hook.
    Strategy,
}

impl GameKind {
    /// The token hook callbacks a game of this kind needs its hook to run, exactly (checked at the
    /// launch): transfers and burns, writing hook data. Every kind keeps its state in hook data
    /// and the header; none takes a delta (which could skim the companion's buybacks) or a
    /// callback its hook may lack (`after_burn` would fail every burn).
    pub fn hook_flags(&self) -> u16 {
        match self {
            GameKind::Lottery | GameKind::Jackpot | GameKind::Streak | GameKind::Strategy => {
                LOTTERY_HOOK_FLAGS
            }
        }
    }
}

/// Where a game's draw is.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub enum DrawStatus {
    /// No draw in progress: the next one may be committed once its round is over.
    Idle,
    /// Never set since `draw` commits its seed and requests it in one instruction (a seed is never
    /// on chain without its request); kept so the variants after it keep their Borsh numbers.
    Committed,
    /// Randomness requested from the oracle, not yet revealed.
    Requested,
    /// Randomness revealed: its attempts are being claimed.
    Revealed,
}

/// `PDA(["game", mint])`: a companion's game (`create_game`), its settings fixed before the launch,
/// and its draw. The pot itself is `Companion.pending_pot` (bridged SOL in the creator's holding).
#[account]
#[derive(InitSpace)]
pub struct Game {
    pub version: u8,
    pub bump: u8,
    pub kind: GameKind,
    pub mint: Pubkey,
    /// The coin's token hook, which keeps the tickets (game ticket standard v1).
    pub hook: Pubkey,
    /// Bumps of the hook's `["state", mint]`, of `["hook-status", hook]` and of `["oracle", mint]`.
    pub state_bump: u8,
    pub status_bump: u8,
    pub oracle_bump: u8,
    /// Rounds are `floor(unix_time / round_secs)`, as the hook's header says.
    pub round_secs: u32,
    /// No draw while the pot holds less.
    pub min_pot: u64,
    /// The part of the pot one draw pays.
    pub prize_bps: u16,
    /// How long each attempt's claim is open.
    pub claim_window_secs: u32,
    /// Attempts per draw before the round rolls over.
    pub max_attempts: u8,
    pub created_at: i64,
    // ---- The draw ----
    pub status: DrawStatus,
    /// The earliest round the next draw may be for (every earlier one is drawn or rolled over).
    pub next_round: u32,
    /// The round of the current (or last) draw, and its ticket total, read from the hook's header.
    pub round: u32,
    pub total: u64,
    /// The seed's index in the round, part of its derivation: always 0. A round has one seed,
    /// which is final (never replaced while the draw lasts, however late ORAO answers).
    pub n: u32,
    /// The draw's seed and the oracle's request account for it (`oracle::request_address(seed)`),
    /// committed and requested together, by `draw`, at `committed_at` (`requested_at`, the same).
    pub seed: [u8; 32],
    pub request: Pubkey,
    pub committed_at: i64,
    pub requested_at: i64,
    /// The revealed randomness, and when.
    pub randomness: [u8; 64],
    pub revealed_at: i64,
    /// What this draw pays (fixed at the request: `prize_bps` of the pot then).
    pub prize: u64,
    // ---- Running totals ----
    pub draws: u64,
    pub prizes_paid: u64,
    pub prizes_total: u64,
    pub rollovers: u64,
    /// Lamports the pot has sent the oracle payer.
    pub oracle_total: u64,
    pub last_winner: Pubkey,
    /// When the pot last paid a prize or was retired to the buyback (0: never, and the launch
    /// counts): a pot that pays nothing for long is dormant (`Game::dormant_secs`), then
    /// retirable (`Game::retirable_at`).
    pub settled_at: i64,
    // ---- The oracle's circuit breaker (from `reserved`) ----
    /// The seed of the last request the pot paid for, until a draw is revealed (zeros: none). While
    /// ORAO has not answered it (still pending, in a form this program can't read, or gone), the
    /// pot pays for no new request until `Game::oracle_backoff_rounds` rounds after its draw's.
    pub paid_seed: [u8; 32],
    /// The round of that request's draw.
    pub paid_round: u32,
    /// Requests the pot has paid for since ORAO last answered one of this game's (a reveal, or
    /// the last paid request found answered).
    pub paid_streak: u8,
    // ---- Jackpot (from `reserved`; zero for every other kind) ----
    /// A round ends this long after its last qualifying buy (as the hook's jackpot header says).
    pub timer_secs: u32,
    /// The least a qualifying buy delivers (as the hook says; its holding must still hold the
    /// round's amount).
    pub min_tokens: u64,
    /// The last round settled (paid or forfeited), by number: only later rounds can be.
    pub paid_buys: u64,
    // ---- Streak (from `reserved`; zero for every other kind). The epochs are the rounds
    // (`round_secs`); the claim epoch is `round`, its total `total`, its pot `prize`, and claims
    // are open while `status` is `Revealed`, until `claims_end`. ----
    /// A holding shares in an epoch only if, by its end, it has sent nothing for this long.
    pub min_streak_secs: u32,
    /// The least weight that shares.
    pub min_weight: u64,
    /// What the claim epoch's pot has paid so far (bounties included).
    pub epoch_paid: u64,
    // ---- Phase 3a (from `reserved`; zero until a step checks a hashed audit). ----
    /// The deploy slot of the game hook's code when its audit last held for it (`hook_audit_ok`):
    /// an audit the protocol recorded with the code's hash (`set_hook_status_v2`) lifts the cap
    /// only while the hook is immutable or Bordrless-managed and runs that code. A step rehashes the
    /// code only when its deploy slot moved since (an upgrade, or anyone's `ExtendProgram`).
    pub hook_audit_slot: u64,
    /// The hook's hashed audit held at `hook_audit_slot`.
    pub hook_audit_ok: bool,
    /// Room for the kinds to come, zero until then.
    pub reserved: [u8; 34],
}

impl Game {
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    /// The deploy slot the hook's hashed audit last held for, if it did.
    pub fn hook_audit_memo(&self) -> Option<u64> {
        self.hook_audit_ok.then_some(self.hook_audit_slot)
    }

    /// Keeps what a step found of the hook's hashed audit.
    pub fn set_hook_audit_memo(&mut self, memo: Option<u64>) {
        self.hook_audit_ok = memo.is_some();
        self.hook_audit_slot = memo.unwrap_or(0);
    }

    pub fn address(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[GAME_SEED, mint.as_ref()], &crate::ID)
    }

    /// The oracle payer, `PDA(["oracle", mint])`.
    pub fn oracle_payer(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[ORACLE_SEED, mint.as_ref()], &crate::ID)
    }

    /// When claim attempt `attempt` opens (`None` on overflow).
    pub fn attempt_opens(&self, attempt: u8) -> Option<i64> {
        i64::from(self.claim_window_secs)
            .checked_mul(i64::from(attempt))?
            .checked_add(self.revealed_at)
    }

    /// When claim attempt `attempt` closes: a window after it opens, and never after the draw's
    /// claims end (`None` on overflow).
    pub fn attempt_closes(&self, attempt: u8) -> Option<i64> {
        Some(
            self.attempt_opens(attempt)?
                .checked_add(i64::from(self.claim_window_secs))?
                .min(self.claims_end()),
        )
    }

    /// When the current draw's claims end: with the round after the drawn one
    /// ([`claims_end`]).
    pub fn claims_end(&self) -> i64 {
        claims_end(self.round, self.round_secs)
    }

    /// The last moment a draw of `round` may be made: [`reveal_secs`] for ORAO's answer and the
    /// reveal (`REVEAL_SECS` for a round of 40 minutes or more, a quarter of a shorter one), then a
    /// whole claim window, before its claims end. Later, the round rolls over (`Late`).
    pub fn last_draw(&self, round: u32) -> i64 {
        claims_end(round, self.round_secs)
            .saturating_sub(i64::from(self.claim_window_secs))
            .saturating_sub(reveal_secs(self.round_secs))
    }

    /// How long the pot may pay no prize before the game is dormant: `DORMANT_SECS`, or
    /// `DORMANT_ROUNDS` rounds when longer.
    pub fn dormant_secs(&self) -> i64 {
        DORMANT_SECS.max(DORMANT_ROUNDS.saturating_mul(i64::from(self.round_secs)))
    }

    /// Since when the pot has paid nothing: its last prize or retirement, else the launch
    /// (`launched_at`, the companion's).
    pub fn idle_since(&self, launched_at: i64) -> i64 {
        self.settled_at.max(launched_at)
    }

    /// The least a draw at `now` needs in the pot, before any cap: the game's `min_pot`, or
    /// `MIN_MIN_POT` once the game is dormant (a pot its fees no longer grow to its minimum is
    /// still paid to a holder).
    pub fn min_pot_at(&self, now: i64, launched_at: i64) -> u64 {
        let dormant_at = self
            .idle_since(launched_at)
            .saturating_add(self.dormant_secs());
        if now >= dormant_at {
            self.min_pot.min(MIN_MIN_POT)
        } else {
            self.min_pot
        }
    }

    /// When anyone may retire the pot to the buyback (`retire`): `RETIRE_DORMANT_PERIODS` dormant
    /// periods with no prize.
    pub fn retirable_at(&self, launched_at: i64) -> i64 {
        self.idle_since(launched_at)
            .saturating_add(self.dormant_secs().saturating_mul(RETIRE_DORMANT_PERIODS))
    }

    /// Whether the pot paid for a request ORAO has not been seen answering since (`paid_seed`).
    pub fn has_paid_request(&self) -> bool {
        self.paid_seed != [0u8; 32]
    }

    /// While the pot's last paid request is unanswered, how many rounds after its draw's round
    /// (`paid_round`) a draw must be for the pot to pay for another: 1, then 2, 4, 8… for each
    /// request paid since ORAO last answered one of this game's (`paid_streak`), and never more
    /// than a dormant period's worth (`DORMANT_SECS`, at least one round). An oracle that stops
    /// answering (or answers in a form this program can't read) thus costs the pot about log2 of
    /// the rounds it stays so in requests, then one a dormant period, never one a round; one lost
    /// answer costs no draw.
    pub fn oracle_backoff_rounds(&self) -> u64 {
        let doublings = u32::from(self.paid_streak.saturating_sub(1)).min(32);
        let most = (DORMANT_SECS / i64::from(self.round_secs.max(1))).max(1);
        (1u64 << doublings).min(most as u64)
    }

    /// Whether the current draw (`round`) is far enough after the pot's last paid request
    /// (`paid_round`) to pay for a new one while that one is unanswered.
    pub fn oracle_backoff_over(&self) -> bool {
        self.oracle_backoff_over_for(self.round)
    }

    /// [`Game::oracle_backoff_over`] for a draw of `round` (one not committed yet).
    pub fn oracle_backoff_over_for(&self, round: u32) -> bool {
        u64::from(round) >= u64::from(self.paid_round).saturating_add(self.oracle_backoff_rounds())
    }

    /// ORAO answered one of this game's requests: the breaker resets.
    pub fn oracle_answered(&mut self) {
        self.paid_seed = [0; 32];
        self.paid_round = 0;
        self.paid_streak = 0;
    }
}

/// `PDA(["strategy", mint])`: a strategy game's terms (phase 3a, `docs/phase3a.md` §4.2), fixed at
/// `create_strategy_game`, beside the `Game` (whose period fields are a streak's: `round` the period
/// open for payments, `total` its tickets, `prize` its budget, `epoch_paid` what it has paid,
/// `status` `Revealed` while it is open).
#[account]
#[derive(InitSpace)]
pub struct StrategyTerms {
    pub version: u8,
    pub bump: u8,
    pub game: Pubkey,
    pub mint: Pubkey,
    /// The strategy program, asked `plan` and `entitle`.
    pub strategy: Pubkey,
    /// Bump of the strategy's `["hook-status", strategy]` (it need not exist).
    pub status_bump: u8,
    /// The extra accounts the strategy sees after the prefix (its registry, resolved when the game
    /// was made), each owned by the strategy: the first `n_extras`.
    pub extras: [Pubkey; 2],
    pub n_extras: u8,
    /// The most of the unlocked pot a period may pay (1 to `MAX_STRATEGY_BUDGET_BPS`).
    pub budget_bps: u16,
    /// The most one holder gets of a period's budget (1 to `MAX_STRATEGY_SHARE_BPS`).
    pub max_share_bps: u16,
    /// The most candidates a payment takes (1 to `MAX_STRATEGY_PER_TX`).
    pub max_per_tx: u8,
    /// The most compute `plan` and `entitle` may use (to `MAX_PLAN_CU`, `MAX_ENTITLE_CU`).
    pub plan_cu_max: u32,
    pub entitle_cu_max: u32,
    /// Running totals.
    pub periods_planned: u32,
    pub paid_total: u64,
    pub last_plan_at: i64,
    /// `paid_total` when the game last counted as active (`settled_at` moved): it counts again
    /// once `STRATEGY_ACTIVE_BPS` of the pot has been paid since, over however many periods.
    pub paid_at_active: u64,
    /// The strategy's audit, as last checked against its code: whether it held, and the deploy
    /// slot of the code it held for (a plan recomputes the code's hash only once the slot moved).
    pub audit_ok: bool,
    pub audit_slot: u64,
    pub reserved: [u8; 15],
}

impl StrategyTerms {
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    pub fn address(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[STRATEGY_SEED, mint.as_ref()], &crate::ID)
    }

    /// The extras the strategy sees.
    pub fn extras(&self) -> &[Pubkey] {
        &self.extras[..usize::from(self.n_extras).min(2)]
    }
}

impl HookTerms {
    /// The stricter of two statuses (the ticket hook's and the strategy's): blocked if either is,
    /// audited only if both are, the lower cap.
    pub fn stricter(&self, other: &HookTerms) -> HookTerms {
        HookTerms {
            audited: self.audited && other.audited,
            pot_cap: match (self.audited, other.audited) {
                (false, false) => self.pot_cap.min(other.pot_cap),
                (true, false) => other.pot_cap,
                (false, true) => self.pot_cap,
                (true, true) => self.pot_cap.min(other.pot_cap),
            },
            blocked: self.blocked || other.blocked,
        }
    }
}

/// When the claims of round `round`'s draw end: when the round after it ends. A holding keeps its
/// ranges of two rounds (the game ticket standard), so in round `round + 2` one written in both
/// `round + 1` and `round + 2` no longer holds its range of `round`; every claim of `round` is made
/// before that can happen, and a draw that can't finish by then rolls over.
pub fn claims_end(round: u32, round_secs: u32) -> i64 {
    bordrless_game::round_end(round.saturating_add(1), round_secs)
}

/// What the protocol says of a game hook, `PDA(["hook-status", hook])`, written only by the
/// companion program's upgrade authority (`set_hook_status`). Without one, a hook is not audited,
/// its pots are capped at `DEFAULT_POT_CAP` and it is not blocked.
#[account]
#[derive(InitSpace)]
pub struct HookStatus {
    pub version: u8,
    pub bump: u8,
    pub hook: Pubkey,
    /// Audited with the companion: its pots are not capped (and it is never blocked).
    pub audited: bool,
    /// The most each pot of this hook's games holds while it is not audited.
    pub pot_cap: u64,
    /// The pot share of every game of this hook, and every pot, go to the buyback: no prize is
    /// paid. Only for a hook that is not audited; an audit clears it.
    pub blocked: bool,
    pub updated_at: i64,
    pub updated_by: Pubkey,
    pub reserved: [u8; 32],
}

impl HookStatus {
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    pub fn address(hook: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[HOOK_STATUS_SEED, hook.as_ref()], &crate::ID)
    }

    /// The executable hash of the code the audit was of (`set_hook_status_v2`, phase 3a: it lives
    /// in what was `reserved`); zeros for an audit recorded by `set_hook_status` (v1), or none.
    pub fn audited_hash(&self) -> [u8; 32] {
        self.reserved
    }

    pub fn terms(&self) -> HookTerms {
        HookTerms {
            audited: self.audited,
            pot_cap: self.pot_cap,
            blocked: self.blocked,
        }
    }
}

/// Bordrless Studio's attestation of a program's code, `PDA(["attest", program])`, written only by
/// Studio's attester (`STUDIO_ATTESTER`) once Studio's worker has rebuilt the program's source,
/// matched the build's hash with the code on chain (checked again here, when written), and passed
/// the static checks, the simulator and the review. Revoked by the attester or the protocol's
/// upgrade authority. "Current" while not revoked and its `build_hash` is the program's code as it
/// is (`programdata_slot` unchanged is the fast check; an extension changes the slot, not the code,
/// so a reader then recomputes the hash).
#[account]
#[derive(InitSpace)]
pub struct HookAttestation {
    pub version: u8,
    pub bump: u8,
    pub program: Pubkey,
    /// `solana-verify`'s executable hash of the attested code.
    pub build_hash: [u8; 32],
    /// sha256 of the frozen source Studio built.
    pub source_hash: [u8; 32],
    /// The Studio template's commit the source was built against (20 bytes of a git sha1).
    pub template_commit: [u8; 20],
    pub sim_version: u16,
    pub sim_pass: bool,
    /// The largest cut the simulator saw, and the cap Studio applies, in basis points.
    pub cut_max_bps: u16,
    pub cap_bps: u16,
    /// `REVIEW_PASS` or `REVIEW_WARN`.
    pub review: u8,
    /// What it is: 0 a token hook, 1 a game hook, 2 a strategy (informative).
    pub kind: u8,
    /// The ProgramData's slot when attested.
    pub programdata_slot: u64,
    pub attested_at: i64,
    pub attester: Pubkey,
    pub revoked: bool,
    pub revoked_at: i64,
    pub reserved: [u8; 32],
}

impl HookAttestation {
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    pub fn address(program: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[ATTEST_SEED, program.as_ref()], &crate::ID)
    }
}

/// A hook's status as the steps apply it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookTerms {
    pub audited: bool,
    pub pot_cap: u64,
    pub blocked: bool,
}

impl HookTerms {
    /// A hook the protocol has said nothing of.
    pub const DEFAULT: Self = Self {
        audited: false,
        pot_cap: DEFAULT_POT_CAP,
        blocked: false,
    };

    /// The most a pot may hold: none for an audited hook, else the status's cap and never more
    /// than `DEFAULT_POT_CAP`.
    pub fn cap(&self) -> Option<u64> {
        (!self.audited).then_some(self.pot_cap.min(DEFAULT_POT_CAP))
    }

    /// The least a pot must hold to be drawn: the game's minimum, or the cap when the cap is
    /// lower (a pot full at its cap is drawn, never stuck below a minimum it can't reach).
    pub fn draw_threshold(&self, min_pot: u64) -> u64 {
        self.cap().map_or(min_pot, |cap| min_pot.min(cap))
    }
}

/// `PDA(["claimed", game, epoch_le, owner])`: a streak share claimed (`claim_share`), so nobody
/// claims an epoch twice. Its rent is the sender's, who gets it back (`close_receipt`) once the
/// epoch's claims have ended.
#[account]
#[derive(InitSpace)]
pub struct ShareReceipt {
    pub version: u8,
    pub bump: u8,
    pub game: Pubkey,
    pub epoch: u32,
    pub owner: Pubkey,
    /// Who paid the rent, and gets it back.
    pub payer: Pubkey,
    /// Paid to the owner (after the sender's bounty).
    pub amount: u64,
    pub claimed_at: i64,
}

impl ShareReceipt {
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    pub fn address(game: &Pubkey, epoch: u32, owner: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[
                CLAIMED_SEED,
                game.as_ref(),
                &epoch.to_le_bytes(),
                owner.as_ref(),
            ],
            &crate::ID,
        )
    }
}

/// A Snowball's market maker at `PDA(["snowball", mint])` (2026-10-12): its terms, fixed at
/// `create_snowball`, its reference price and the tokens it holds. Its SOL is the companion's pot
/// (`Companion.pending_pot`, `pot_bps` of every fee claim), in the creator's bridged-SOL holding;
/// its tokens are in the creator's token holding beside any dev bag, counted here so a sell never
/// touches the dev bag.
#[account]
#[derive(InitSpace)]
pub struct Snowball {
    pub version: u8,
    pub bump: u8,
    pub mint: Pubkey,
    /// A drop of this much from the reference price triggers a buy.
    pub buy_step_bps: u16,
    /// A rise of this much from the reference price triggers a sell.
    pub sell_step_bps: u16,
    /// The share of the pot a buy spends.
    pub buy_bps: u16,
    /// The share of the tokens it holds a sell sells.
    pub sell_bps: u16,
    /// The least time between two steps that trade or follow (10 seconds to a day).
    pub interval: i64,
    /// The most one trade moves, lamports (a sell: the SOL value of what it sells).
    pub max_trade: u64,
    /// The most one trade moves as a share of the pool's SOL side, real and virtual (0.25% to 10%).
    pub pool_bps: u16,
    /// Trade every interval even when the price is within its steps, on the side that rebalances what
    /// it holds (the pot keeps cycling); without it, it waits for a step.
    pub flat_trades: bool,
    /// The price the next trade is measured from (quote per base unit times `PRICE_SCALE`): the
    /// opening price until the first step, then the price each trade left, or the price a follow
    /// found. Zero until the first step.
    pub reference_price: u128,
    /// When it last traded or followed.
    pub last_at: i64,
    /// Tokens it holds (bought and not sold yet).
    pub tokens_held: u64,
    /// Running totals.
    pub buys: u64,
    pub sells: u64,
    pub sol_spent: u64,
    pub sol_received: u64,
    pub tokens_bought: u64,
    pub tokens_sold: u64,
    pub bounties_total: u64,
    pub created_at: i64,
    pub reserved: [u8; 64],
}

impl Snowball {
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    pub fn address(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[SNOWBALL_SEED, mint.as_ref()], &crate::ID)
    }

    /// The price a buy waits for: `step_bps` below `reference`.
    pub fn buy_below(reference: u128, step_bps: u16) -> u128 {
        reference * u128::from(BPS - u64::from(step_bps)) / u128::from(BPS)
    }

    /// The price a sell waits for: `step_bps` above `reference`.
    pub fn sell_above(reference: u128, step_bps: u16) -> u128 {
        reference * u128::from(BPS + u64::from(step_bps)) / u128::from(BPS)
    }
}

/// Bordrless's approval of a program's code (2026-10-12), `PDA(["approval", program])`, written only
/// by the companion program's upgrade authority or the script approver (`SCRIPT_APPROVER`;
/// `approve`, `revoke_approval`). It is bound to the
/// code: `approve` hashes the program's code on chain and refuses any other hash, and an approval
/// counts only while the program still runs that code (`approval::approved`: the ProgramData's slot
/// unchanged since, or the code hashed again). Upgrading an approved program voids its approval
/// until it is approved again. Only a program nobody but Bordrless can change is approved:
/// immutable, upgradeable only by Bordrless's keys, or behind a hook timelock.
#[account]
#[derive(InitSpace)]
pub struct Approval {
    pub version: u8,
    pub bump: u8,
    pub program: Pubkey,
    /// `APPROVAL_KIND_TOKEN_HOOK` (0) or `APPROVAL_KIND_SCRIPT` (1).
    pub kind: u8,
    /// `solana-verify`'s executable hash of the approved code.
    pub code_hash: [u8; 32],
    pub approved_at: i64,
    pub revoked: bool,
    /// Who last wrote it (the upgrade authority or the script approver that approved or revoked
    /// it).
    pub updated_by: Pubkey,
    /// The program's ProgramData slot when approved: while it is unchanged the code is the approved
    /// code (the loader refuses a second deploy, upgrade or extension in one slot), so a check needs
    /// no hash; once it moved (an upgrade, or anyone's `ExtendProgram`) the code is hashed again.
    pub programdata_slot: u64,
    /// Who wrote the program: a script's author earns `AUTHOR_SHARE_BPS` of the creator fees of
    /// every script companion made with it afterwards (`create_script` copies it into the `Script`).
    /// The default key: no author. (From what was `reserved`.)
    pub author: Pubkey,
    pub reserved: [u8; 24],
}

impl Approval {
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    pub fn address(program: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[APPROVAL_SEED, program.as_ref()], &crate::ID)
    }
}

/// A script companion's script (2026-10-12), `PDA(["script", mint])`, fixed at `create_script`: the
/// program that decides what the pot does, and how often a step may run. The pot itself is
/// `Companion.pending_pot` (bridged SOL in the creator's holding); what the script buys stays in the
/// creator's token holding beside any dev bag.
#[account]
#[derive(InitSpace)]
pub struct Script {
    pub version: u8,
    pub bump: u8,
    pub mint: Pubkey,
    /// The script: a program Bordrless approved (`APPROVAL_KIND_SCRIPT`), called with the creator
    /// address signing.
    pub program: Pubkey,
    /// The least time between two steps (10 seconds to a day).
    pub interval: i64,
    /// When it last stepped (0: never).
    pub last_at: i64,
    /// Steps run so far.
    pub steps: u64,
    pub created_at: i64,
    /// The script's author, copied from its approval at `create_script`: paid `AUTHOR_SHARE_BPS` of
    /// what every fee claim takes from the launch, unless they are the launch's beneficiary. The
    /// default key: no author (an approval naming nobody, or an immutable script, which needs no
    /// approval). (From what was `reserved`.)
    pub author: Pubkey,
    /// What the author has been paid so far (lamports).
    pub author_paid: u64,
    /// The size of the script's state, `PDA(["script-state", mint])`, which `create_script` made
    /// (zeroed, owned by the script) and every call hands the script; 0: no state, the system
    /// program's id in its place. (From what was `reserved`.)
    pub state_len: u32,
    pub reserved: [u8; 20],
}

impl Script {
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    pub fn address(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[SCRIPT_SEED, mint.as_ref()], &crate::ID)
    }

    /// The script's state: `PDA(["script-state", mint])`, owned by the script once made.
    pub fn state_address(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[SCRIPT_STATE_SEED, mint.as_ref()], &crate::ID)
    }
}

/// The bounds a script companion's payouts keep within (Loss cover, 2026-10-12),
/// `PDA(["payout-terms", mint])`, 128 bytes, made by `create_script_v2` with the script. A script
/// made with `create_script` has none, and its answers pay nobody (`PayoutsOff`). The counters are
/// the companion's: a step writes them after it paid.
#[account]
#[derive(InitSpace)]
pub struct PayoutTerms {
    pub version: u8,
    pub bump: u8,
    pub mint: Pubkey,
    /// The most one step pays, of the pot at the step's start (1 to 10,000 basis points).
    pub step_bps: u16,
    /// The most one holding gets of a step's budget (1 to 10,000).
    pub holding_bps: u16,
    /// The most a window pays, of the pot as it stood at the window's first step (1 to 10,000).
    pub window_bps: u16,
    /// The window: `floor(now / window_secs)` (10 seconds to a day: a round, or a day).
    pub window_secs: u32,
    /// The window the counters are for (its first second), the pot at its first step after the
    /// script ran, and what it has paid.
    pub window_start: i64,
    pub window_pot: u64,
    pub window_paid: u64,
    /// Lamports paid to holders all time, entries recorded all time (touches included), when.
    pub paid_total: u64,
    pub payouts: u64,
    pub last_paid_at: i64,
    pub reserved: [u8; 28],
}

impl PayoutTerms {
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    pub fn address(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[PAYOUT_TERMS_SEED, mint.as_ref()], &crate::ID)
    }

    /// The first second of the window `now` falls in.
    /// Whether the script may answer wallet payouts (`reserved[0]`, set before the launch by
    /// `allow_wallet_payouts`; 0 for every terms made before).
    pub fn wallets_allowed(&self) -> bool {
        self.reserved[0] == WALLET_PAYOUTS_ALLOWED
    }

    pub fn window_start_at(&self, now: i64) -> i64 {
        let secs = i64::from(self.window_secs.max(1));
        now.div_euclid(secs).saturating_mul(secs)
    }
}

/// The wallets that share a companion's beneficiary part (`set_payees`, 2026-10-13),
/// `PDA(["payees", mint])`: each gets its basis points of every `withdraw`'s beneficiary part, as
/// SOL; the beneficiary keeps the rest. A payee whose account can't take a payment (not passed,
/// read-only, or it would stay below its rent-exempt minimum) is owed it: it stays in the
/// companion's `pending_beneficiary` and is paid at a later `withdraw`.
#[account]
#[derive(InitSpace)]
pub struct Payees {
    pub version: u8,
    pub bump: u8,
    pub mint: Pubkey,
    /// How many of `payees` are set.
    pub count: u8,
    /// The beneficiary's own part owed: its account could not take it (not payable, or it would
    /// stay below its rent-exempt minimum), so it waits, as a payee's does, instead of failing
    /// every payee's payment with it.
    pub beneficiary_owed: u64,
    pub payees: [Payee; MAX_PAYEES],
}

/// One payee of a `Payees`.
#[derive(
    AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq, InitSpace,
)]
pub struct Payee {
    pub wallet: Pubkey,
    /// Its part of the beneficiary part, in basis points.
    pub bps: u16,
    /// Owed and not yet paid (part of the companion's `pending_beneficiary`).
    pub owed: u64,
    /// Paid all time.
    pub paid: u64,
}

impl Payees {
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    pub fn address(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[PAYEES_SEED, mint.as_ref()], &crate::ID)
    }

    /// The payees set.
    pub fn set(&self) -> &[Payee] {
        &self.payees[..usize::from(self.count).min(MAX_PAYEES)]
    }

    /// What every payee is owed (not the beneficiary's own owed part).
    pub fn owed(&self) -> u64 {
        self.set()
            .iter()
            .map(|p| p.owed)
            .fold(0u64, u64::saturating_add)
    }
}

/// How `new` lamports of beneficiary part split among `payees` (each its basis points, rounded
/// down) and the beneficiary (the rest).
pub fn payee_shares(new: u64, payees: &[Payee]) -> (Vec<u64>, u64) {
    let shares: Vec<u64> = payees
        .iter()
        .map(|p| bps_of(new, u64::from(p.bps)))
        .collect();
    let given = shares.iter().fold(0u64, |a, s| a.saturating_add(*s));
    (shares, new.saturating_sub(given))
}

/// One entry of a script's wallet answer (`PayoutAnswer` version 2, 2026-10-13): the account paid,
/// by its key (it must be among the step's remaining accounts, writable), so whoever sends the step
/// can't put another account in its place.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WalletPayoutEntry {
    pub to: Pubkey,
    pub lamports: u64,
    pub tag: u32,
}

/// A script's wallet answer: `version` `PAYOUT_ANSWER_WALLETS_VERSION`, at most
/// `MAX_PAYOUTS_PER_STEP` entries (1 + 4 + 44 * 16 = 709 bytes of return data at most).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct WalletPayoutAnswer {
    pub version: u8,
    pub entries: Vec<WalletPayoutEntry>,
}

/// One entry of a script's payout answer: `account` an index into the step's remaining accounts
/// (a holding of the launch's mint), `lamports` paid to its owner as SOL (0: a touch, recorded and
/// not paid), `tag` the script's, passed to the hook's `record_payout` untouched.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PayoutEntry {
    pub account: u8,
    pub lamports: u64,
    pub tag: u32,
}

/// What a script's `companion_step` may return: its payouts, which the companion checks against
/// the terms, pays and records (`1 + 4 + 13 * n` bytes of Borsh, `n` at most
/// `MAX_PAYOUTS_PER_STEP`). No return data, or empty, is no answer: today's step.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct PayoutAnswer {
    /// `PAYOUT_ANSWER_VERSION`.
    pub version: u8,
    pub entries: Vec<PayoutEntry>,
}

/// The pool's price: its quote side over its base side, real and virtual, times `PRICE_SCALE`.
pub fn spot_price(
    quote_reserve: u64,
    virtual_quote: u64,
    base_reserve: u64,
    virtual_base: u64,
) -> Option<u128> {
    let quote = u128::from(quote_reserve) + u128::from(virtual_quote);
    let base = u128::from(base_reserve) + u128::from(virtual_base);
    (base > 0).then(|| quote * PRICE_SCALE / base)
}

/// `reference` moved toward `spot` by at most `REFERENCE_STEP_BPS` of it (all the way when unset).
pub fn step_toward(reference: u128, spot: u128) -> u128 {
    if reference == 0 {
        return spot;
    }
    let step = reference * u128::from(REFERENCE_STEP_BPS) / u128::from(BPS);
    if spot > reference {
        spot.min(reference + step)
    } else {
        spot.max(reference.saturating_sub(step))
    }
}

/// `part` basis points of `amount`, rounded down.
pub fn bps_of(amount: u64, part: u64) -> u64 {
    (u128::from(amount) * u128::from(part) / u128::from(BPS)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deployed v1 `Companion`: 292 bytes after the discriminator, the last 64 reserved (and
    /// written as zeros by `create`). v2 takes its fields from those 64 bytes.
    const V1_INIT_SPACE: usize = 292;

    fn companion() -> Companion {
        Companion {
            version: VERSION,
            bump: 255,
            creator_bump: 254,
            mint: Pubkey::new_unique(),
            beneficiary: Pubkey::new_unique(),
            split: Split {
                buyback_bps: 10_000,
                holders_bps: 0,
                beneficiary_bps: 0,
            },
            bounty_bps: 50,
            max_buyback: 1,
            buyback_interval: 60,
            vest_secs: 0,
            launched: true,
            launched_at: 1,
            dev_tokens: 2,
            dev_released: 3,
            pending_buyback: 4,
            pending_holders: 5,
            pending_beneficiary: 6,
            last_buyback_at: 7,
            claimed_total: 8,
            spent_total: 9,
            burned_total: 10,
            shared_total: 11,
            paid_beneficiary_total: 12,
            bounties_total: 13,
            reference_price: 14,
            reference_at: 15,
            game_hook: Pubkey::default(),
            pot_bps: 0,
            pending_pot: 0,
            round_secs: 0,
            stranded_burned_at: 0,
            game_kind: GameKind::Lottery,
            pot_locked: 0,
            mode: MODE_CLASSIC,
        }
    }

    #[test]
    fn a_v1_companion_reads_as_one_without_a_game() {
        assert_eq!(Companion::INIT_SPACE, V1_INIT_SPACE);
        assert_eq!(Companion::LEN, 300);
        let c = companion();
        let mut data = Vec::new();
        c.try_serialize(&mut data).unwrap();
        assert_eq!(data.len(), Companion::LEN);
        // What v1 wrote as `reserved` is all zeros: v2 reads no game, no pot.
        assert_eq!(&data[Companion::LEN - 64..], &[0u8; 64][..]);
        let back = Companion::try_deserialize(&mut &data[..]).unwrap();
        assert!(!back.is_game());
        assert_eq!((back.pot_bps, back.pending_pot, back.round_secs), (0, 0, 0));
        assert_eq!(back.set_aside(), Some(4 + 5 + 6));
        assert_eq!(back.stranded_burned_at, 0);
        // The stranded buyback's clock: the latest of its five times, then 30 days (or 4
        // intervals).
        assert_eq!(c.stranded_since(2), 15, "the reference's last move");
        assert_eq!(c.stranded_at(20), 20 + STRANDED_SECS);
        let weekly = Companion {
            buyback_interval: 30 * 86_400,
            stranded_burned_at: 99,
            ..c.clone()
        };
        assert_eq!(weekly.stranded_at(0), 99 + 4 * 30 * 86_400);
        let far = Companion {
            stranded_burned_at: i64::MAX - 1,
            ..c.clone()
        };
        assert_eq!(far.stranded_at(0), i64::MAX, "saturates");
        // A game's fields land in those 64 bytes, in order.
        let hook = Pubkey::new_unique();
        let g = Companion {
            game_hook: hook,
            pot_bps: 7_000,
            pending_pot: u64::MAX,
            round_secs: 3_600,
            stranded_burned_at: -2,
            ..c.clone()
        };
        let mut data2 = Vec::new();
        g.try_serialize(&mut data2).unwrap();
        assert_eq!(data2.len(), Companion::LEN);
        assert_eq!(data[..Companion::LEN - 64], data2[..Companion::LEN - 64]);
        let tail = &data2[Companion::LEN - 64..];
        assert_eq!(&tail[..32], hook.as_ref());
        assert_eq!(&tail[32..34], &7_000u16.to_le_bytes());
        assert_eq!(&tail[34..42], &u64::MAX.to_le_bytes());
        assert_eq!(&tail[42..46], &3_600u32.to_le_bytes());
        assert_eq!(&tail[46..54], &(-2i64).to_le_bytes());
        assert_eq!(&tail[54..], &[0u8; 10][..]);
        // Phase 2's fields take what was left: the kind (0, the lottery, in every companion made
        // before), the locked part of the pot, one byte still reserved.
        let k = Companion {
            game_kind: GameKind::Streak,
            pot_locked: u64::MAX - 1,
            ..g.clone()
        };
        let mut data3 = Vec::new();
        k.try_serialize(&mut data3).unwrap();
        assert_eq!(data3.len(), Companion::LEN);
        assert_eq!(data2[..Companion::LEN - 10], data3[..Companion::LEN - 10]);
        let tail = &data3[Companion::LEN - 10..];
        assert_eq!(tail[0], 2);
        assert_eq!(&tail[1..9], &(u64::MAX - 1).to_le_bytes());
        assert_eq!(tail[9], 0);
        let zeros = Companion::try_deserialize(&mut &data2[..]).unwrap();
        assert_eq!((zeros.game_kind, zeros.pot_locked), (GameKind::Lottery, 0));
    }

    #[test]
    fn a_claim_restarts_the_stranded_wait_only_when_it_credits_what_the_buyback_held() {
        // The buyback after a claim at `now` that found it holding `held` and credited `credit`.
        let after = |held: u64, credit: u64, burned_at: i64, now: i64| {
            let mut c = Companion {
                pending_buyback: held + credit,
                stranded_burned_at: burned_at,
                ..companion()
            };
            c.restart_stranded_wait(held, now);
            c.stranded_burned_at
        };
        // An empty buyback (a working hook's buybacks spent it all): any credit restarts the wait.
        assert_eq!(after(0, 1, 0, 1_000), 1_000);
        // Dust left in it: a credit at least as large restarts it.
        assert_eq!(after(500, 500, 0, 1_000), 1_000);
        assert_eq!(after(500, 499, 0, 1_000), 0);
        // A refusing hook's buyback holds everything credited since its last burn: a credit
        // smaller than that restarts nothing; one at least as large does, and the next must be
        // at least twice it.
        let held = 5_000_000_000;
        assert_eq!(after(held, held - 1, 7, 1_000), 7);
        assert_eq!(after(held, held, 7, 1_000), 1_000);
        assert_eq!(after(2 * held, held, 7, 1_000), 7);
        // No credit, no restart; and the wait never moves back.
        assert_eq!(after(0, 0, 0, 1_000), 0);
        assert_eq!(after(0, 1, 2_000, 1_000), 2_000);
        // No overflow at the extremes.
        assert_eq!(after(u64::MAX / 2, u64::MAX / 2, 0, i64::MAX), i64::MAX);
    }

    #[test]
    fn approvals_and_scripts_have_fixed_sizes_and_modes() {
        // The author (and a script's author's running total) came from the reserved bytes: the
        // sizes stay.
        assert_eq!(
            Approval::LEN,
            8 + 1 + 1 + 32 + 1 + 32 + 8 + 1 + 32 + 8 + 32 + 24
        );
        assert_eq!(Approval::LEN, 180);
        assert_eq!(
            Script::LEN,
            8 + 1 + 1 + 32 + 32 + 8 + 8 + 8 + 8 + 32 + 8 + 4 + 20
        );
        assert_eq!(Script::LEN, 170);
        let mut c = companion();
        assert!(c.is_classic() && !c.is_script() && !c.has_own_pot());
        c.mode = MODE_SCRIPT;
        assert!(c.is_script() && !c.is_classic() && !c.is_snowball() && c.has_own_pot());
        c.mode = MODE_SNOWBALL;
        assert!(c.is_snowball() && !c.is_script() && c.has_own_pot());
        // The mode is the companion's last byte.
        let mut data = Vec::new();
        Companion {
            mode: MODE_SCRIPT,
            ..companion()
        }
        .try_serialize(&mut data)
        .unwrap();
        assert_eq!(data.len(), Companion::LEN);
        assert_eq!(data[Companion::LEN - 1], MODE_SCRIPT);
    }

    #[test]
    fn splits_with_a_pot() {
        let s = Split {
            buyback_bps: 3_000,
            holders_bps: 0,
            beneficiary_bps: 0,
        };
        assert!(!s.valid());
        assert!(s.valid_with_pot(7_000));
        assert!(!s.valid_with_pot(6_999));
        let full = Split {
            buyback_bps: 10_000,
            holders_bps: 0,
            beneficiary_bps: 0,
        };
        assert!(full.valid() && full.valid_with_pot(0) && !full.valid_with_pot(1));
        // No overflow at the extremes.
        let max = Split {
            buyback_bps: u16::MAX,
            holders_bps: u16::MAX,
            beneficiary_bps: u16::MAX,
        };
        assert!(!max.valid_with_pot(u16::MAX));
    }

    #[test]
    fn terms_and_caps() {
        assert_eq!(HookTerms::DEFAULT.cap(), Some(DEFAULT_POT_CAP));
        let audited = HookTerms {
            audited: true,
            ..HookTerms::DEFAULT
        };
        assert_eq!(audited.cap(), None);
        // A cap is never above the unaudited ceiling, whatever a status holds.
        let high = HookTerms {
            pot_cap: u64::MAX,
            ..HookTerms::DEFAULT
        };
        assert_eq!(high.cap(), Some(DEFAULT_POT_CAP));
        // A pot full at a cap below the minimum is drawn at the cap.
        let low = HookTerms {
            pot_cap: MIN_POT_CAP,
            ..HookTerms::DEFAULT
        };
        assert_eq!(low.draw_threshold(5 * MIN_POT_CAP), MIN_POT_CAP);
        assert_eq!(
            HookTerms::DEFAULT.draw_threshold(MAX_MIN_POT),
            DEFAULT_POT_CAP
        );
        assert_eq!(HookTerms::DEFAULT.draw_threshold(MIN_MIN_POT), MIN_MIN_POT);
        assert_eq!(audited.draw_threshold(MAX_MIN_POT), MAX_MIN_POT);
        let g = Game {
            version: 1,
            bump: 0,
            kind: GameKind::Lottery,
            mint: Pubkey::default(),
            hook: Pubkey::default(),
            state_bump: 0,
            status_bump: 0,
            oracle_bump: 0,
            round_secs: 3_600,
            min_pot: 0,
            prize_bps: 0,
            claim_window_secs: 600,
            max_attempts: 8,
            created_at: 0,
            status: DrawStatus::Idle,
            next_round: 0,
            round: 0,
            total: 0,
            n: 0,
            seed: [0; 32],
            request: Pubkey::default(),
            committed_at: 0,
            requested_at: 0,
            randomness: [0; 64],
            revealed_at: 1_000,
            prize: 0,
            draws: 0,
            prizes_paid: 0,
            prizes_total: 0,
            rollovers: 0,
            oracle_total: 0,
            last_winner: Pubkey::default(),
            settled_at: 0,
            paid_seed: [0; 32],
            paid_round: 0,
            paid_streak: 0,
            timer_secs: 0,
            min_tokens: 0,
            paid_buys: 0,
            min_streak_secs: 0,
            min_weight: 0,
            epoch_paid: 0,
            hook_audit_slot: 0,
            hook_audit_ok: false,
            reserved: [0; 34],
        };
        assert_eq!(g.attempt_opens(0), Some(1_000));
        assert_eq!(g.attempt_opens(8), Some(1_000 + 8 * 600));
        let mut far = g.clone();
        far.revealed_at = i64::MAX;
        assert_eq!(far.attempt_opens(1), None);
        assert_eq!(far.attempt_closes(1), None);
        // Claims end with the round after the drawn one; the last attempts are cut there.
        let mut late = g.clone();
        late.round = 500_000;
        assert_eq!(late.claims_end(), 500_002 * 3_600);
        late.revealed_at = 500_002 * 3_600 - 1_000;
        assert_eq!(late.attempt_closes(0), Some(late.revealed_at + 600));
        assert_eq!(late.attempt_closes(1), Some(late.claims_end()));
        assert_eq!(
            late.last_draw(500_000),
            late.claims_end() - 600 - REVEAL_SECS
        );
        assert_eq!(claims_end(u32::MAX, u32::MAX), i64::MAX, "saturates");
        let mut data = Vec::new();
        g.try_serialize(&mut data).unwrap();
        assert_eq!(data.len(), Game::LEN);
        assert_eq!(Game::LEN, 8 + 478, "the account keeps its size");
        // Phase 2's fields take the first 40 of what were the last 83 reserved bytes, in order;
        // a game made before reads them as zeros (as `create_game` wrote them).
        let k = Game {
            timer_secs: 0x0102_0304,
            min_tokens: 0x1112_1314_1516_1718,
            paid_buys: 0x2122_2324_2526_2728,
            min_streak_secs: 0x3132_3334,
            min_weight: 0x4142_4344_4546_4748,
            epoch_paid: 0x5152_5354_5556_5758,
            ..g.clone()
        };
        let mut data2 = Vec::new();
        k.try_serialize(&mut data2).unwrap();
        assert_eq!(data[..Game::LEN - 83], data2[..Game::LEN - 83]);
        let tail = &data2[Game::LEN - 83..];
        assert_eq!(&tail[..4], &0x0102_0304u32.to_le_bytes());
        assert_eq!(&tail[4..12], &k.min_tokens.to_le_bytes());
        assert_eq!(&tail[12..20], &k.paid_buys.to_le_bytes());
        assert_eq!(&tail[20..24], &0x3132_3334u32.to_le_bytes());
        assert_eq!(&tail[24..32], &k.min_weight.to_le_bytes());
        assert_eq!(&tail[32..40], &k.epoch_paid.to_le_bytes());
        assert_eq!(&tail[40..], &[0u8; 43][..]);
        assert!(data[Game::LEN - 83..].iter().all(|b| *b == 0));
        // The kinds' Borsh numbers: a lottery made before stays one.
        assert_eq!(data[10], 0, "Game.kind of a lottery");
        let mut kind = Vec::new();
        GameKind::Streak.serialize(&mut kind).unwrap();
        GameKind::Jackpot.serialize(&mut kind).unwrap();
        assert_eq!(kind, [2, 1]);
        // A receipt is small: its rent is about 0.0015 SOL.
        assert_eq!(ShareReceipt::LEN, 8 + 1 + 1 + 32 + 4 + 32 + 32 + 8 + 8);
    }

    /// A lottery game with `round_secs` and its claim windows, nothing drawn yet.
    fn timing_game(round_secs: u32, claim_window_secs: u32, max_attempts: u8) -> Game {
        Game {
            version: 1,
            bump: 0,
            kind: GameKind::Lottery,
            mint: Pubkey::default(),
            hook: Pubkey::default(),
            state_bump: 0,
            status_bump: 0,
            oracle_bump: 0,
            round_secs,
            min_pot: MIN_MIN_POT,
            prize_bps: 10_000,
            claim_window_secs,
            max_attempts,
            created_at: 0,
            status: DrawStatus::Idle,
            next_round: 0,
            round: 0,
            total: 0,
            n: 0,
            seed: [0; 32],
            request: Pubkey::default(),
            committed_at: 0,
            requested_at: 0,
            randomness: [0; 64],
            revealed_at: 0,
            prize: 0,
            draws: 0,
            prizes_paid: 0,
            prizes_total: 0,
            rollovers: 0,
            oracle_total: 0,
            last_winner: Pubkey::default(),
            settled_at: 0,
            paid_seed: [0; 32],
            paid_round: 0,
            paid_streak: 0,
            timer_secs: 0,
            min_tokens: 0,
            paid_buys: 0,
            min_streak_secs: 0,
            min_weight: 0,
            epoch_paid: 0,
            hook_audit_slot: 0,
            hook_audit_ok: false,
            reserved: [0; 34],
        }
    }

    #[test]
    fn last_draw_keeps_ten_minutes_from_an_hour_and_a_quarter_of_a_shorter_round() {
        // An hourly game, as every game before five-minute rounds: exactly as before, ten minutes
        // and a window before the claims end with the round after the drawn one.
        let hourly = timing_game(3_600, 600, 3);
        assert_eq!(hourly.last_draw(500_000), 1_800_006_000);
        assert_eq!(
            hourly.last_draw(500_000),
            claims_end(500_000, 3_600) - 600 - REVEAL_SECS
        );
        // The SDK vectors' sample game (`companion-games.json`, `clocks.lastDraw`).
        let six_hours = timing_game(21_600, 600, 8);
        assert_eq!(six_hours.last_draw(82_870), 1_790_034_000);
        // Every round of an hour or more keeps 600 s, whatever its window.
        for (secs, window) in [
            (3_600u32, 60u32),
            (3_600, 300),
            (7_200, 3_600),
            (86_400, 86_400),
            (30 * 86_400, 600),
        ] {
            let g = timing_game(secs, window, 1);
            assert_eq!(
                g.last_draw(1_000),
                claims_end(1_000, secs) - i64::from(window) - 600,
                "{secs}"
            );
        }
        // A five-minute round with one-minute windows: the claims of round 6,000,000 (it ends at
        // 1,800,000,300) end with the next one, at 1,800,000,600; the draw leaves a minute and the
        // reveal, 75 s, before that: 165 s into the round after its own.
        let short = timing_game(300, 60, 2);
        assert_eq!(reveal_secs(300), 75);
        assert_eq!(claims_end(6_000_000, 300), 1_800_000_600);
        assert_eq!(short.last_draw(6_000_000), 1_800_000_465);
        assert_eq!(
            short.last_draw(6_000_000) - bordrless_game::round_end(6_000_000, 300),
            165
        );
        // Its longest window (two and a half minutes, one attempt) still leaves 75 s to draw.
        let widest = timing_game(300, 150, 1);
        assert_eq!(widest.last_draw(6_000_000), 1_800_000_375);
        // A draw at the very last moment, revealed when the reveal's margin runs out, keeps a whole
        // window before the claims end.
        let mut drawn = short.clone();
        drawn.round = 6_000_000;
        drawn.revealed_at = short.last_draw(6_000_000) + reveal_secs(300);
        assert_eq!(drawn.attempt_closes(0), Some(drawn.revealed_at + 60));
        assert_eq!(drawn.attempt_closes(0), Some(drawn.claims_end()));
    }

    #[test]
    fn a_pot_that_pays_nothing_goes_dormant_then_retirable() {
        let g = Game {
            version: 1,
            bump: 0,
            kind: GameKind::Lottery,
            mint: Pubkey::default(),
            hook: Pubkey::default(),
            state_bump: 0,
            status_bump: 0,
            oracle_bump: 0,
            round_secs: 3_600,
            min_pot: 2 * MIN_MIN_POT * 10,
            prize_bps: 10_000,
            claim_window_secs: 300,
            max_attempts: 6,
            created_at: 0,
            status: DrawStatus::Idle,
            next_round: 0,
            round: 0,
            total: 0,
            n: 0,
            seed: [0; 32],
            request: Pubkey::default(),
            committed_at: 0,
            requested_at: 0,
            randomness: [0; 64],
            revealed_at: 0,
            prize: 0,
            draws: 0,
            prizes_paid: 0,
            prizes_total: 0,
            rollovers: 0,
            oracle_total: 0,
            last_winner: Pubkey::default(),
            settled_at: 0,
            paid_seed: [0; 32],
            paid_round: 0,
            paid_streak: 0,
            timer_secs: 0,
            min_tokens: 0,
            paid_buys: 0,
            min_streak_secs: 0,
            min_weight: 0,
            epoch_paid: 0,
            hook_audit_slot: 0,
            hook_audit_ok: false,
            reserved: [0; 34],
        };
        let launched = 1_000_000;
        // Short rounds: 30 days; the launch counts until a prize is paid.
        assert_eq!(g.dormant_secs(), DORMANT_SECS);
        assert_eq!(g.idle_since(launched), launched);
        let dormant = launched + DORMANT_SECS;
        assert_eq!(g.min_pot_at(dormant - 1, launched), g.min_pot);
        assert_eq!(g.min_pot_at(dormant, launched), MIN_MIN_POT);
        assert_eq!(g.retirable_at(launched), launched + 2 * DORMANT_SECS);
        // A prize (or a retirement) restarts the clock.
        let paid = Game {
            settled_at: dormant + 5,
            ..g.clone()
        };
        assert_eq!(paid.min_pot_at(dormant + 6, launched), g.min_pot);
        assert_eq!(paid.retirable_at(launched), dormant + 5 + 2 * DORMANT_SECS);
        // Long rounds: four of them.
        let long = Game {
            round_secs: 30 * 86_400,
            ..g.clone()
        };
        assert_eq!(long.dormant_secs(), 4 * 30 * 86_400);
        assert_eq!(long.retirable_at(launched), launched + 8 * 30 * 86_400);
        // Never overflows.
        let far = Game {
            settled_at: i64::MAX - 1,
            round_secs: u32::MAX,
            ..g.clone()
        };
        assert_eq!(far.retirable_at(0), i64::MAX);
        assert_eq!(far.min_pot_at(i64::MAX - 1, 0), g.min_pot);
        // A minimum already at the floor stays there.
        let low = Game {
            min_pot: MIN_MIN_POT,
            ..g.clone()
        };
        assert_eq!(low.min_pot_at(dormant, launched), MIN_MIN_POT);

        // The oracle's breaker: after a paid request ORAO left unanswered, the pot pays for the
        // next one a round later, then 2, 4, 8… rounds, never more than 30 days apart.
        let mut b = g;
        assert!(!b.has_paid_request());
        b.paid_seed = [7; 32];
        b.paid_round = 100;
        let gaps: Vec<u64> = (1..=12)
            .map(|streak| {
                b.paid_streak = streak;
                b.oracle_backoff_rounds()
            })
            .collect();
        assert_eq!(gaps, [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 720, 720]);
        b.paid_streak = u8::MAX;
        assert_eq!(b.oracle_backoff_rounds(), 720, "hourly rounds: 30 days");
        b.paid_streak = 3;
        b.round = 103;
        assert!(!b.oracle_backoff_over());
        b.round = 104;
        assert!(b.oracle_backoff_over());
        // The same for a draw not committed yet.
        assert!(!b.oracle_backoff_over_for(103) && b.oracle_backoff_over_for(104));
        // Rounds of 30 days: one round at most; never zero.
        let long = Game {
            round_secs: 30 * 86_400,
            paid_streak: 9,
            ..b.clone()
        };
        assert_eq!(long.oracle_backoff_rounds(), 1);
        let longer = Game {
            round_secs: u32::MAX,
            ..long
        };
        assert_eq!(longer.oracle_backoff_rounds(), 1);
        // No overflow at the extremes.
        let far = Game {
            paid_round: u32::MAX,
            round: u32::MAX,
            ..b.clone()
        };
        assert!(!far.oracle_backoff_over());
        // A reveal resets it.
        b.oracle_answered();
        assert!(!b.has_paid_request());
        assert_eq!((b.paid_round, b.paid_streak), (0, 0));
    }
}
