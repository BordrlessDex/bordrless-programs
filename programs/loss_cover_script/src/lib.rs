//! `loss_cover_script`: the companion script of a Loss cover coin (`docs/loss-cover.md` in the
//! monorepo, `docs/companions.md` "Loss cover" here). The companion calls it with the creator
//! address signing (`companion_init` once, `companion_step` every interval); it reads the Loss cover
//! hook's records and answers which holders are under water and what each gets, as a
//! `PayoutAnswer` in its return data. It moves nothing itself: the companion checks the answer
//! against the payout terms, pays each owner SOL from the pot and has the hook record it.
//!
//! - **The terms** (`companion_init`'s `init_data`, the Borsh of [`CoverTerms`], 34 bytes): the
//!   part of a loss the pot covers when it can (`cover_bps`), the least holding, loss and age that
//!   count, the round length, and the least pot a round starts with (`min_pot_lamports`). Out of
//!   bounds, the init fails and so does the launch's setup.
//! - **Rounds** are `floor(now / round_secs)`, the game standard's arithmetic: every step agrees
//!   on them with no start time. A step's first act is the round: when a new one has begun and the
//!   pot holds at least `min_pot_lamports`, the loss measured over the previous round becomes
//!   `l_prev` (0 after a gap, or before the first step), the counters reset, the budget `b` is the
//!   companion's pot as the step reads it, and `CoverRound` is logged. With the pot below the
//!   minimum, no round starts: every step answers `NothingToDo` (it fails, so it takes no interval
//!   slot, measures nothing and leaves `l_prev` as it was) until the pot reaches it; the round
//!   that then starts after the gap measures only. `b` and `l_prev` are written at the round's
//!   first step and read by every later one: no sender can change them until the round rolls.
//! - **The fraction** every under-water holder is paid of its uncovered loss within a round is
//!   `f = min(1, b / l_prev)`, never stored: each payment is `uncovered * b / l_prev`, capped at
//!   `uncovered`. With `l_prev = 0` the round measures only: the first round, and the round after
//!   a gap or after a round in which nobody was under water.
//! - **A step** reads the pool's spot, rolls the round when due, reads the terms (by address among
//!   its accounts) and recomputes the companion's bounds exactly (an answer out of bounds is
//!   refused, never clamped), then takes every holding of the mint among its accounts in the
//!   order given: a record of version 1 covering its whole balance, not yet measured this round,
//!   above the least holding, under water by at least the least loss (`cover_bps` of its loss,
//!   less what it was paid), old enough. Each gets an entry: `pay = min(uncovered * b / l_prev,
//!   uncovered, what the round's budget has left, what the companion's budget has left, the
//!   holding's cap)`; what it does not pay joins `l_cur`, what the next round must cover. A touch
//!   (0 lamports) when the budget is out or the round measures. Nothing to measure or pay:
//!   `NothingToDo`, so an empty step pays no bounty and takes no interval slot.
//!
//! The accounts a step takes after the companion's six (`[creator, mint, launch, companion, pool,
//! state]`), looked up by key: the `PayoutTerms` (writable), the hook program, the payout
//! authority, the hook's state (writable), its `["hook-authority"]`, the token program and its
//! event authority, the bridge's `unwrap_sol` accounts, then the candidates: a holding (writable:
//! the hook records into it) and, in a paying round, its owner (writable). The order among them
//! does not matter to this program; the answer's indices are the holdings' positions among them.

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use bordrless_companion::constants::{
    CREATOR_SEED, MAX_PAYOUTS_PER_STEP, PAYOUT_ANSWER_VERSION, SCRIPT_STATE_SEED, SWAP_ID, TOKEN_ID,
};
use bordrless_companion::instructions::{bounty_for, step_budget};
use bordrless_companion::state::{bps_of, Companion, PayoutAnswer, PayoutEntry, PayoutTerms};
use bordrless_game::{eligible, round_of};
use bordrless_launch::client::read_launch;
use bordrless_strategy::pool_reserves;
use bordrless_token::client as token_client;
use loss_cover_hook::{spot_of, Record, PRICE_SCALE};

declare_id!("9mRXU9gpWY3B13AyuCCtNjHnKt1pUFBQ6vJDLTwgakhm");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Bordrless Loss cover script",
    project_url: "https://github.com/BordrlessDex/bordrless-programs",
    contacts: "link:https://github.com/BordrlessDex/bordrless-programs/security/advisories/new",
    policy: "https://github.com/BordrlessDex/bordrless-programs/blob/main/SECURITY.md",
    source_code: "https://github.com/BordrlessDex/bordrless-programs"
}

/// The companion program, whose creator address signs every call and which makes the state.
pub const COMPANION_ID: Pubkey = bordrless_companion::ID;
/// Layout version of [`CoverState`].
pub const VERSION: u8 = 1;
/// The state's size: what `create_script_v2`'s `state_len` must be.
pub const STATE_LEN: u32 = 128;
/// The part of a loss the pot covers when it can: 10% to all of it.
pub const MIN_COVER_BPS: u16 = 1_000;
pub const MAX_COVER_BPS: u16 = 10_000;
/// A round: a minute to a day.
pub const MIN_ROUND_SECS: u32 = 60;
pub const MAX_ROUND_SECS: u32 = 86_400;
/// The longest wait after a buy before its holding is covered: 30 days.
pub const MAX_MIN_AGE_SECS: u32 = 30 * 86_400;
/// The most a round may be asked to start with: 100 SOL (0: no minimum).
pub const MAX_MIN_POT_LAMPORTS: u64 = 100_000_000_000;
pub const BPS: u64 = 10_000;

/// The terms of a Loss cover coin: `companion_init`'s `init_data`, Borsh, 34 bytes.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoverTerms {
    /// The part of a loss the pot covers when it can (`MIN_COVER_BPS` to `MAX_COVER_BPS`).
    pub cover_bps: u16,
    /// The least balance that counts, base units (at least 1).
    pub min_holding: u64,
    /// The least uncovered loss that counts, lamports (at least 1).
    pub min_loss: u64,
    /// How long after its last buy a holding waits before it counts (at most `MAX_MIN_AGE_SECS`).
    pub min_age_secs: u32,
    /// The round (`MIN_ROUND_SECS` to `MAX_ROUND_SECS`): the companion's payout window must be the
    /// same.
    pub round_secs: u32,
    /// The least the pot holds for a round to start (0 to `MAX_MIN_POT_LAMPORTS`; 0: none).
    pub min_pot_lamports: u64,
}

impl CoverTerms {
    /// Borsh bytes of the terms.
    pub const LEN: usize = 2 + 8 + 8 + 4 + 4 + 8;

    /// Whether the terms are within their bounds.
    pub fn valid(&self) -> bool {
        (MIN_COVER_BPS..=MAX_COVER_BPS).contains(&self.cover_bps)
            && self.min_holding >= 1
            && self.min_loss >= 1
            && self.min_age_secs <= MAX_MIN_AGE_SECS
            && (MIN_ROUND_SECS..=MAX_ROUND_SECS).contains(&self.round_secs)
            && self.min_pot_lamports <= MAX_MIN_POT_LAMPORTS
    }
}

/// The script's state, `PDA(["script-state", mint])` under the companion, 128 bytes: the terms,
/// written once by `companion_init`, then the rounds.
#[account]
#[derive(InitSpace)]
pub struct CoverState {
    pub version: u8,
    pub mint: Pubkey,
    pub cover_bps: u16,
    pub min_holding: u64,
    pub min_loss: u64,
    pub min_age_secs: u32,
    pub round_secs: u32,
    pub min_pot_lamports: u64,
    /// The round of the last step (0 before the first).
    pub round: u32,
    /// The round's budget: the pot at its first step.
    pub b: u64,
    /// The uncovered loss measured over the previous round.
    pub l_prev: u64,
    /// Measured so far this round.
    pub l_cur: u64,
    /// Paid so far this round.
    pub round_paid: u64,
    /// Holdings measured this round.
    pub measured: u32,
    /// Rounds stepped so far.
    pub rounds: u32,
    /// Paid all time.
    pub paid_total: u64,
    pub reserved: [u8; 1],
}

impl CoverState {
    /// Account size.
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    /// The terms as the state keeps them.
    pub fn terms(&self) -> CoverTerms {
        CoverTerms {
            cover_bps: self.cover_bps,
            min_holding: self.min_holding,
            min_loss: self.min_loss,
            min_age_secs: self.min_age_secs,
            round_secs: self.round_secs,
            min_pot_lamports: self.min_pot_lamports,
        }
    }
}

/// What a holding under water is owed, as a step computes it: `loss = (entry - spot) * balance /
/// 10^12` lamports, `covered = loss * cover_bps / 10,000`, `uncovered = covered - paid` (0 when
/// nothing, or the holding is not under water).
pub fn uncovered_loss(entry: u64, spot: u64, balance: u64, cover_bps: u16, paid: u64) -> u64 {
    if entry <= spot {
        return 0;
    }
    let loss = u128::from(entry - spot) * u128::from(balance) / PRICE_SCALE;
    let covered = loss * u128::from(cover_bps) / u128::from(BPS);
    u64::try_from(covered.saturating_sub(u128::from(paid))).unwrap_or(u64::MAX)
}

/// What a holding owed `uncovered` is paid in a round of budget `b` that measured `l_prev` the
/// round before: `min(uncovered * b / l_prev, uncovered)`, 0 in a round that measures only.
pub fn fraction_of(uncovered: u64, b: u64, l_prev: u64) -> u64 {
    if l_prev == 0 {
        return 0;
    }
    let pay = u128::from(uncovered) * u128::from(b) / u128::from(l_prev);
    u64::try_from(pay.min(u128::from(uncovered))).unwrap_or(uncovered)
}

/// A round rolled: its budget and the loss measured over the round before, which fix the round's
/// fraction `min(1, b / l_prev)`.
#[event]
pub struct CoverRound {
    pub mint: Pubkey,
    pub round: u32,
    pub b: u64,
    pub l_prev: u64,
}

#[program]
pub mod loss_cover_script {
    use super::*;

    /// The companion's `create_script_v2` calls it once, the creator address signing: the terms
    /// (`init_data`, a [`CoverTerms`]) are written into the state the companion made.
    pub fn companion_init(
        ctx: Context<CompanionInit>,
        mint: Pubkey,
        init_data: Vec<u8>,
    ) -> Result<()> {
        let terms = CoverTerms::try_from_slice(&init_data)
            .map_err(|_| error!(LossCoverScriptError::BadTerms))?;
        require!(terms.valid(), LossCoverScriptError::BadTerms);
        let s = &mut ctx.accounts.state;
        s.version = VERSION;
        s.mint = mint;
        s.cover_bps = terms.cover_bps;
        s.min_holding = terms.min_holding;
        s.min_loss = terms.min_loss;
        s.min_age_secs = terms.min_age_secs;
        s.round_secs = terms.round_secs;
        s.min_pot_lamports = terms.min_pot_lamports;
        s.round = 0;
        s.b = 0;
        s.l_prev = 0;
        s.l_cur = 0;
        s.round_paid = 0;
        s.measured = 0;
        s.rounds = 0;
        s.paid_total = 0;
        s.reserved = [0; 1];
        Ok(())
    }

    /// The companion's `script_step` calls it, the creator address signing: the round rolled when
    /// due, every candidate holding measured and its payment named (see the module). Answers the
    /// payouts; `NothingToDo` when there are none.
    pub fn companion_step(ctx: Context<CompanionStep>, mint: Pubkey) -> Result<PayoutAnswer> {
        let clock = Clock::get()?;
        let now = clock.unix_timestamp;
        let launch = read_launch(&ctx.accounts.launch)?;
        require_keys_eq!(launch.mint, mint, LossCoverScriptError::WrongMint);
        require_keys_eq!(
            launch.pool,
            ctx.accounts.pool.key(),
            LossCoverScriptError::WrongAccount
        );
        // 1. The pool's spot.
        let pool = &ctx.accounts.pool;
        require_keys_eq!(*pool.owner, SWAP_ID, LossCoverScriptError::WrongAccount);
        let reserves = {
            let data = pool.try_borrow_data()?;
            pool_reserves(&data).ok_or(LossCoverScriptError::NoPrice)?
        };
        require_keys_eq!(reserves.base_mint, mint, LossCoverScriptError::WrongAccount);
        let spot = spot_of(&reserves).ok_or(LossCoverScriptError::NoPrice)?;
        // 2. The round.
        let c = &ctx.accounts.companion;
        let s = &mut ctx.accounts.state;
        require!(s.version == VERSION, LossCoverScriptError::NotInitialized);
        let r = round_of(now, s.round_secs);
        if r > s.round {
            // No round starts on a pot below the minimum: the step fails (no interval slot, no
            // measuring, `l_prev` as it was) until the pot reaches it.
            require!(
                c.pending_pot >= s.min_pot_lamports,
                LossCoverScriptError::NothingToDo
            );
            s.l_prev = if r == s.round.saturating_add(1) {
                s.l_cur
            } else {
                0
            };
            s.l_cur = 0;
            s.round_paid = 0;
            s.measured = 0;
            s.b = c.pending_pot;
            s.round = r;
            s.rounds = s.rounds.saturating_add(1);
            emit!(CoverRound {
                mint,
                round: r,
                b: s.b,
                l_prev: s.l_prev,
            });
        }
        // The companion's bounds, exactly as it computes them after this step: its terms, the
        // window (rolled when this step is its first), the bounty at most the full one this
        // step could earn (so the budget here is never above the companion's).
        let remaining = ctx.remaining_accounts;
        let terms_key = PayoutTerms::address(&mint).0;
        let terms_info = remaining
            .iter()
            .find(|a| *a.key == terms_key)
            .ok_or(LossCoverScriptError::NoTerms)?;
        require_keys_eq!(
            *terms_info.owner,
            COMPANION_ID,
            LossCoverScriptError::NoTerms
        );
        let t = {
            let data = terms_info.try_borrow_data()?;
            PayoutTerms::try_deserialize(&mut &data[..])
                .map_err(|_| error!(LossCoverScriptError::NoTerms))?
        };
        require_keys_eq!(t.mint, mint, LossCoverScriptError::NoTerms);
        let pot = c.pending_pot;
        let window_start = t.window_start_at(now);
        let (window_pot, window_paid) = if t.window_start == window_start {
            (t.window_pot, t.window_paid)
        } else {
            (pot, 0)
        };
        let holdings = remaining
            .iter()
            .filter(|a| *a.owner == TOKEN_ID)
            .count()
            .min(MAX_PAYOUTS_PER_STEP);
        let wanted = bounty_for(Some(holdings));
        let bounty = if pot >= wanted { wanted } else { 0 };
        let budget = step_budget(
            pot,
            pot,
            bounty,
            window_pot,
            window_paid,
            t.step_bps,
            t.window_bps,
        );
        let holding_cap = bps_of(budget, u64::from(t.holding_bps));
        let rent_min = Rent::get()?.minimum_balance(0);
        // 3. The candidates, in the order given.
        let hook = launch.custom_hook.unwrap_or_default();
        let excluded = [
            ctx.accounts.launch.key(),
            launch.pool,
            ctx.accounts.creator.key(),
            c.key(),
            crate::ID,
            s.key(),
            terms_key,
            hook,
        ];
        let mut budget_left = budget;
        let mut entries: Vec<PayoutEntry> = Vec::with_capacity(MAX_PAYOUTS_PER_STEP);
        for (i, info) in remaining.iter().enumerate() {
            if entries.len() == MAX_PAYOUTS_PER_STEP {
                break;
            }
            if *info.owner != TOKEN_ID {
                continue;
            }
            let Ok(h) = token_client::read_holding(info) else {
                continue;
            };
            if h.mint != mint || !eligible(&h.owner, &excluded) || h.owner == c.beneficiary {
                continue;
            }
            let rec = Record::decode(&h.hook_data);
            if !rec.is_set()
                || rec.amount != h.amount
                || rec.round == r
                || h.amount < s.min_holding
                || now.saturating_sub(rec.basis_at) < i64::from(s.min_age_secs)
            {
                continue;
            }
            let uncovered = uncovered_loss(rec.entry, spot, h.amount, s.cover_bps, rec.paid);
            if uncovered == 0 || uncovered < s.min_loss {
                continue;
            }
            let pay = fraction_of(uncovered, s.b, s.l_prev)
                .min(s.b.saturating_sub(s.round_paid))
                .min(budget_left)
                .min(holding_cap);
            if pay > 0 {
                // Paid only into an account the companion will take: its owner's, passed writable,
                // no program's, and not left below its rent-exempt minimum. Otherwise the holding
                // waits for a step that carries it right (not measured, not touched).
                let Some(owner) = remaining.iter().find(|a| *a.key == h.owner) else {
                    continue;
                };
                let funded = owner
                    .lamports()
                    .checked_add(pay)
                    .is_some_and(|l| l >= rent_min);
                if !owner.is_writable || owner.executable || !funded {
                    continue;
                }
            }
            s.l_cur = s.l_cur.saturating_add(uncovered - pay);
            s.measured = s.measured.saturating_add(1);
            s.round_paid = s.round_paid.saturating_add(pay);
            s.paid_total = s.paid_total.saturating_add(pay);
            budget_left -= pay;
            entries.push(PayoutEntry {
                account: i as u8,
                lamports: pay,
                tag: r,
            });
        }
        require!(!entries.is_empty(), LossCoverScriptError::NothingToDo);
        Ok(PayoutAnswer {
            version: PAYOUT_ANSWER_VERSION,
            entries,
        })
    }
}

/// Accounts of `companion_init`, as the companion passes them.
#[derive(Accounts)]
#[instruction(mint: Pubkey)]
pub struct CompanionInit<'info> {
    /// The companion's creator address for the mint, which only the companion signs for.
    #[account(seeds = [CREATOR_SEED, mint.as_ref()], bump, seeds::program = COMPANION_ID)]
    pub creator: Signer<'info>,
    /// CHECK: the mint (not created yet).
    pub mint_account: UncheckedAccount<'info>,
    /// The state the companion made for the mint: `PDA(["script-state", mint])` under the
    /// companion, owned by this program, all zeros.
    #[account(zero, seeds = [SCRIPT_STATE_SEED, mint.as_ref()], bump, seeds::program = COMPANION_ID)]
    pub state: Box<Account<'info, CoverState>>,
    pub system_program: Program<'info, System>,
}

/// Accounts of `companion_step`, as the companion passes them (then the remaining accounts, see
/// the module).
#[derive(Accounts)]
#[instruction(mint: Pubkey)]
pub struct CompanionStep<'info> {
    /// The companion's creator address for the mint, which only the companion signs for.
    #[account(seeds = [CREATOR_SEED, mint.as_ref()], bump, seeds::program = COMPANION_ID)]
    pub creator: Signer<'info>,
    /// CHECK: the mint.
    pub mint_account: UncheckedAccount<'info>,
    /// CHECK: the launch (read in the handler: owner-checked, of the mint).
    pub launch: UncheckedAccount<'info>,
    /// The companion of the mint: its pot and its beneficiary.
    #[account(constraint = companion.mint == mint @ LossCoverScriptError::WrongMint)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the launch pool (the launch's, owner-checked in the handler): the price.
    pub pool: UncheckedAccount<'info>,
    #[account(mut, constraint = state.mint == mint @ LossCoverScriptError::WrongMint)]
    pub state: Box<Account<'info, CoverState>>,
}

/// Errors.
#[error_code]
pub enum LossCoverScriptError {
    #[msg("the terms are out of bounds: cover 10% to 100%, a holding and a loss of at least 1, a wait of 30 days at most, rounds of a minute to a day, a minimum pot of 100 SOL at most")]
    BadTerms,
    #[msg("the state was never initialized")]
    NotInitialized,
    #[msg("an account is of another mint")]
    WrongMint,
    #[msg("an account is not the one expected")]
    WrongAccount,
    #[msg("the pool has no price")]
    NoPrice,
    #[msg("the companion's payout terms for the mint were not passed")]
    NoTerms,
    #[msg("nothing to measure or pay in this batch")]
    NothingToDo,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_is_128_bytes_at_its_offsets() {
        let s = CoverState {
            version: VERSION,
            mint: Pubkey::new_unique(),
            cover_bps: 1,
            min_holding: 2,
            min_loss: 3,
            min_age_secs: 4,
            round_secs: 5,
            min_pot_lamports: 14,
            round: 6,
            b: 7,
            l_prev: 8,
            l_cur: 9,
            round_paid: 10,
            measured: 11,
            rounds: 12,
            paid_total: 13,
            reserved: [0; 1],
        };
        let mut data = Vec::new();
        s.try_serialize(&mut data).unwrap();
        assert_eq!(data.len(), CoverState::LEN);
        assert_eq!(CoverState::LEN, STATE_LEN as usize);
        assert_eq!(data[8], 1);
        assert_eq!(&data[9..41], s.mint.as_ref());
        assert_eq!(&data[41..43], &1u16.to_le_bytes());
        assert_eq!(&data[43..51], &2u64.to_le_bytes());
        assert_eq!(&data[51..59], &3u64.to_le_bytes());
        assert_eq!(&data[59..63], &4u32.to_le_bytes());
        assert_eq!(&data[63..67], &5u32.to_le_bytes());
        assert_eq!(&data[67..75], &14u64.to_le_bytes());
        assert_eq!(&data[75..79], &6u32.to_le_bytes());
        assert_eq!(&data[79..87], &7u64.to_le_bytes());
        assert_eq!(&data[87..95], &8u64.to_le_bytes());
        assert_eq!(&data[95..103], &9u64.to_le_bytes());
        assert_eq!(&data[103..111], &10u64.to_le_bytes());
        assert_eq!(&data[111..115], &11u32.to_le_bytes());
        assert_eq!(&data[115..119], &12u32.to_le_bytes());
        assert_eq!(&data[119..127], &13u64.to_le_bytes());
        assert_eq!(&data[127..], &[0u8; 1][..]);
        let mut terms = Vec::new();
        s.terms().serialize(&mut terms).unwrap();
        assert_eq!(terms.len(), CoverTerms::LEN);
        assert_eq!(CoverTerms::LEN, 34);
        assert_eq!(&terms[26..34], &14u64.to_le_bytes());
    }

    #[test]
    fn terms_within_their_bounds() {
        let ok = CoverTerms {
            cover_bps: 10_000,
            min_holding: 100_000_000_000,
            min_loss: 1_000_000,
            min_age_secs: 1_800,
            round_secs: 300,
            min_pot_lamports: 1_000_000_000,
        };
        assert!(ok.valid());
        assert!(CoverTerms {
            min_pot_lamports: 0,
            ..ok
        }
        .valid());
        assert!(CoverTerms {
            min_pot_lamports: MAX_MIN_POT_LAMPORTS,
            ..ok
        }
        .valid());
        for bad in [
            CoverTerms {
                cover_bps: 999,
                ..ok
            },
            CoverTerms {
                cover_bps: 10_001,
                ..ok
            },
            CoverTerms {
                min_holding: 0,
                ..ok
            },
            CoverTerms { min_loss: 0, ..ok },
            CoverTerms {
                min_age_secs: MAX_MIN_AGE_SECS + 1,
                ..ok
            },
            CoverTerms {
                round_secs: 59,
                ..ok
            },
            CoverTerms {
                round_secs: 86_401,
                ..ok
            },
            CoverTerms {
                min_pot_lamports: MAX_MIN_POT_LAMPORTS + 1,
                ..ok
            },
        ] {
            assert!(!bad.valid(), "{bad:?}");
        }
    }

    #[test]
    fn the_loss_and_the_fraction() {
        // Entry 2e7, spot 1e7 (lamports per base unit times 1e12), 1e12 base units: 1e7 * 1e12 /
        // 1e12 = 1e7 lamports lost; half covered; 1e6 paid before.
        assert_eq!(
            uncovered_loss(20_000_000, 10_000_000, 1_000_000_000_000, 5_000, 1_000_000),
            4_000_000
        );
        assert_eq!(
            uncovered_loss(
                20_000_000,
                10_000_000,
                1_000_000_000_000,
                10_000,
                20_000_000
            ),
            0
        );
        assert_eq!(uncovered_loss(10_000_000, 10_000_000, 1, 10_000, 0), 0);
        assert_eq!(uncovered_loss(9, 10, 1, 10_000, 0), 0);
        assert_eq!(uncovered_loss(u64::MAX, 0, u64::MAX, 10_000, 0), u64::MAX);
        // The fraction: a tenth of the loss when the pot is a tenth of what was measured; all of
        // it when the pot covers it; nothing in a measuring round.
        assert_eq!(fraction_of(1_000, 100, 1_000), 100);
        assert_eq!(fraction_of(1_000, 3_000, 1_000), 1_000);
        assert_eq!(fraction_of(1_000, 0, 1_000), 0);
        assert_eq!(fraction_of(1_000, 500, 0), 0);
        assert_eq!(fraction_of(u64::MAX, u64::MAX, 1), u64::MAX);
        assert_eq!(fraction_of(300, 100, 1_000), 30);
    }

    #[test]
    fn the_companion_names_this_scripts_calls_as_anchor_does() {
        use anchor_lang::Discriminator;
        assert_eq!(
            crate::instruction::CompanionInit::DISCRIMINATOR,
            &bordrless_companion::constants::COMPANION_INIT_DISCRIMINATOR[..]
        );
        assert_eq!(
            crate::instruction::CompanionStep::DISCRIMINATOR,
            &bordrless_companion::constants::COMPANION_STEP_DISCRIMINATOR[..]
        );
        assert_eq!(COMPANION_ID, bordrless_companion::ID);
    }
}
