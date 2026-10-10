//! The Bordrless strategy interface (`docs/phase3a.md` §4): a strategy coin's payouts decided by a
//! builder's program, paid by the audited companion.
//!
//! The companion keeps custody of the pot and asks a strategy program two questions, each by CPI
//! with every account read-only and no signer:
//!
//! 1. **`plan`** ([`PLAN`], [`PlanArgs`] → [`PlanDecision`]): once a period, how much of the pot it
//!    pays (the budget, at most [`PlanArgs::budget_max`]).
//! 2. **`entitle`** ([`ENTITLE`], [`EntitleArgs`] → [`Entitlement`]): once per candidate holding,
//!    how much it gets (at most [`EntitleArgs::max_amount`]).
//!
//! The answer is the return data, set by the strategy itself (an Anchor instruction returning
//! `Result<PlanDecision>` or `Result<Entitlement>`), exactly 8 bytes. The companion refuses any
//! other answer, any answer out of bounds (never clamped) and any call that used more compute than
//! the strategy's terms allow: a period planned so is closed with nothing, a candidate entitled so
//! is skipped. A strategy that fails makes the whole transaction fail.
//!
//! A strategy sees one candidate at a time, so a holder's amount depends only on its own holding
//! and shared state, never on who else a transaction carries: size entitlements so that their sum
//! over every holder stays within the budget (`budget * weight / total`, for one).
//!
//! **Accounts** (all read-only): `plan` gets [`PLAN_PREFIX`] (`game`, `companion`, the ticket
//! hook's state, the launch, the pool), `entitle` gets [`ENTITLE_PREFIX`] (`game`, `companion`,
//! the ticket hook's state, the launch, the candidate's holding), then up to [`MAX_EXTRAS`] extra
//! accounts the strategy names in its registry ([`registry_address`], a
//! `bordrless_hook::HookAccountList`), each owned by the strategy program. The registry is read
//! once, when the strategy game is made, against [`REGISTRY_PREFIX`] (`mint`, `game`).
//!
//! Tickets come from the coin's lottery hook (the game ticket standard, `bordrless-game`): a
//! holding's weight for period `p` is its ticket range for round `p` (what it has held since the
//! period began), its `since` the last time it sent anything. [`slots_of`] reads them.

#![forbid(unsafe_code)]

use anchor_lang::prelude::*;

pub use bordrless_game::{
    parse_holding, round_end, round_of, round_start, GameHeader, HoldingView, Range, Slots,
};
pub use bordrless_hook::{AccountSource, ExtraAccount, HookAccountList, Seed};

/// `sha256("global:plan")[..8]`: the `plan` instruction's discriminator.
pub const PLAN: [u8; 8] = [0x0f, 0xb9, 0x2d, 0x20, 0xa3, 0x06, 0xd8, 0x4d];
/// `sha256("global:entitle")[..8]`: the `entitle` instruction's discriminator.
pub const ENTITLE: [u8; 8] = [0x1f, 0xc5, 0x5b, 0x2d, 0xda, 0x60, 0x0d, 0xaf];
/// Seed of a strategy's registry: `PDA(["bordrless-strategy-accounts", mint], strategy)`.
pub const REGISTRY_SEED: &[u8] = b"bordrless-strategy-accounts";
/// The most extra accounts a strategy's registry may list (transaction size, §4.7).
pub const MAX_EXTRAS: usize = 2;
/// The layout version of [`PlanArgs`] and [`EntitleArgs`].
pub const ARGS_VERSION: u8 = 1;
/// The length of an answer's return data: one u64.
pub const ANSWER_LEN: usize = 8;

/// The accounts `plan` gets before the extras, by index.
pub mod plan_prefix {
    /// The companion's `Game`.
    pub const GAME: usize = 0;
    /// The companion.
    pub const COMPANION: usize = 1;
    /// The ticket hook's state (the game header).
    pub const HOOK_STATE: usize = 2;
    /// The launch.
    pub const LAUNCH: usize = 3;
    /// The launch's pool.
    pub const POOL: usize = 4;
}
/// How many accounts `plan` gets before the extras.
pub const PLAN_PREFIX: usize = 5;

/// The accounts `entitle` gets before the extras, by index.
pub mod entitle_prefix {
    /// The companion's `Game`.
    pub const GAME: usize = 0;
    /// The companion.
    pub const COMPANION: usize = 1;
    /// The ticket hook's state (the game header).
    pub const HOOK_STATE: usize = 2;
    /// The launch.
    pub const LAUNCH: usize = 3;
    /// The candidate's holding of the coin.
    pub const HOLDING: usize = 4;
}
/// How many accounts `entitle` gets before the extras.
pub const ENTITLE_PREFIX: usize = 5;

/// The keys a strategy's registry resolves its seeds against (`Seed::Account(i)`): `[mint, game]`.
pub mod registry_prefix {
    /// The coin's mint.
    pub const MINT: u8 = 0;
    /// The companion's `Game` of the mint.
    pub const GAME: u8 = 1;
}
/// How many keys [`registry_prefix`] has.
pub const REGISTRY_PREFIX: usize = 2;

/// `plan`'s arguments: the period that just ended and what the pot can pay.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanArgs {
    /// [`ARGS_VERSION`].
    pub version: u8,
    /// The coin.
    pub mint: Pubkey,
    /// The period (the ticket hook's round) that just ended.
    pub period: u32,
    /// When it began.
    pub period_start: i64,
    /// When it ended.
    pub period_end: i64,
    /// Its ticket total: the weight held through it.
    pub total: u64,
    /// The pot now (bridged SOL lamports).
    pub pot: u64,
    /// The most the budget may be: the strategy's `budget_bps` of the pot, within its cap.
    pub budget_max: u64,
    /// Periods planned before this one.
    pub periods_planned: u32,
    /// What the strategy's periods have paid in all.
    pub paid_total: u64,
    /// The clock.
    pub now: i64,
}

/// `plan`'s answer: what this period pays in all (0: nothing, the period is skipped).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlanDecision {
    /// At most [`PlanArgs::budget_max`].
    pub budget: u64,
}

/// `entitle`'s arguments: one candidate.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct EntitleArgs {
    /// [`ARGS_VERSION`].
    pub version: u8,
    /// The coin.
    pub mint: Pubkey,
    /// The period being paid.
    pub period: u32,
    /// The holding's owner (a wallet, eligible).
    pub owner: Pubkey,
    /// The holding's balance now.
    pub balance: u64,
    /// Its weight for the period (its ticket range for the round: held since the period began).
    pub weight: u64,
    /// When it last sent anything (or first received).
    pub since: i64,
    /// The period's ticket total.
    pub total: u64,
    /// The period's budget.
    pub budget: u64,
    /// What it has paid so far.
    pub paid: u64,
    /// The most this candidate may get: the strategy's `max_share_bps` of the budget, and no more
    /// than what is left of it.
    pub max_amount: u64,
    /// The clock.
    pub now: i64,
}

/// `entitle`'s answer: what this holding gets (0: nothing, and no receipt).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entitlement {
    /// At most [`EntitleArgs::max_amount`].
    pub amount: u64,
}

/// A strategy's registry for `mint`: `PDA(["bordrless-strategy-accounts", mint], strategy)`.
pub fn registry_address(strategy: &Pubkey, mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[REGISTRY_SEED, mint.as_ref()], strategy)
}

/// A holding's ticket slots, from its 64 bytes of hook data (the game ticket standard).
pub fn slots_of(hook_data: &[u8; 64]) -> Slots {
    Slots::decode(hook_data)
}

/// A holding's weight for `period`: its live range of that round (current slot, else previous),
/// 0 when it has none.
pub fn weight_in(hook_data: &[u8; 64], period: u32) -> u64 {
    Slots::decode(hook_data)
        .range_in(period)
        .map_or(0, |r| r.weight)
}

/// `amount * part / whole`, rounded down (0 for a `whole` of 0): a pro-rata share.
pub fn pro_rata(amount: u64, part: u64, whole: u64) -> u64 {
    if whole == 0 {
        return 0;
    }
    (u128::from(amount) * u128::from(part) / u128::from(whole)) as u64
}

/// What a launch pool's account says of its reserves (the DEX's `Pool`, read at its offsets: the
/// optional hook makes everything after it move by 32 bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolReserves {
    /// The base mint (the coin).
    pub base_mint: Pubkey,
    /// The quote mint (bridged SOL for a launch).
    pub quote_mint: Pubkey,
    /// The LP fee.
    pub lp_fee_bps: u16,
    /// Real base reserve.
    pub base_reserve: u64,
    /// Real quote reserve.
    pub quote_reserve: u64,
    /// Virtual base offset.
    pub virtual_base: u64,
    /// Virtual quote offset.
    pub virtual_quote: u64,
}

/// Reads a DEX pool's reserves from its account data (discriminator first; not checked here: the
/// caller checks the owner is the DEX). `None` for data too short or a bad option tag.
pub fn pool_reserves(data: &[u8]) -> Option<PoolReserves> {
    let key = |at: usize| -> Option<Pubkey> {
        Some(Pubkey::new_from_array(data.get(at..at + 32)?.try_into().ok()?))
    };
    let u16_at = |at: usize| -> Option<u16> {
        Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
    };
    let u64_at = |at: usize| -> Option<u64> {
        Some(u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?))
    };
    // discriminator 8, version, bump, lp_mint_bump, base_mint, quote_mint, lp_mint, base_vault,
    // quote_vault, then the hook's Option<Pubkey>.
    let base_mint = key(11)?;
    let quote_mint = key(43)?;
    let mut at = 171;
    match *data.get(at)? {
        0 => at += 1,
        1 => at += 33,
        _ => return None,
    }
    // hook_flags, lp_fee_bps, protocol_fee_bps, then the reserves.
    let lp_fee_bps = u16_at(at + 2)?;
    at += 6;
    Some(PoolReserves {
        base_mint,
        quote_mint,
        lp_fee_bps,
        base_reserve: u64_at(at)?,
        quote_reserve: u64_at(at + 8)?,
        virtual_base: u64_at(at + 16)?,
        virtual_quote: u64_at(at + 24)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_eight_bytes_and_args_round_trip() {
        let mut out = Vec::new();
        PlanDecision { budget: 7 }.serialize(&mut out).unwrap();
        assert_eq!(out.len(), ANSWER_LEN);
        let mut out = Vec::new();
        Entitlement { amount: u64::MAX }.serialize(&mut out).unwrap();
        assert_eq!(out, u64::MAX.to_le_bytes());
        let a = EntitleArgs {
            version: ARGS_VERSION,
            mint: Pubkey::new_unique(),
            period: 9,
            owner: Pubkey::new_unique(),
            balance: 1,
            weight: 2,
            since: -3,
            total: 4,
            budget: 5,
            paid: 6,
            max_amount: 7,
            now: 8,
        };
        let mut bytes = Vec::new();
        a.serialize(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 1 + 32 + 4 + 32 + 8 * 8);
        assert_eq!(EntitleArgs::try_from_slice(&bytes).unwrap(), a);
        let p = PlanArgs {
            version: ARGS_VERSION,
            period: 1,
            ..PlanArgs::default()
        };
        let mut bytes = Vec::new();
        p.serialize(&mut bytes).unwrap();
        assert_eq!(bytes.len(), 1 + 32 + 4 + 8 + 8 + 8 + 8 + 8 + 4 + 8 + 8);
    }

    #[test]
    fn pro_rata_never_overflows_or_divides_by_zero() {
        assert_eq!(pro_rata(u64::MAX, u64::MAX, u64::MAX), u64::MAX);
        assert_eq!(pro_rata(100, 1, 3), 33);
        assert_eq!(pro_rata(100, 1, 0), 0);
        // The sum over a partition never exceeds the amount.
        let weights = [1u64, 7, 13, 999, 5];
        let total: u64 = weights.iter().sum();
        let sum: u64 = weights.iter().map(|w| pro_rata(1_000_003, *w, total)).sum();
        assert!(sum <= 1_000_003);
    }

    #[test]
    fn weights_come_from_the_ticket_slots() {
        let s = Slots {
            current: Range {
                round: 10,
                start: 5,
                weight: 50,
            },
            previous: Range {
                round: 9,
                start: 0,
                weight: 40,
            },
            since: 77,
            free: [0; 16],
        };
        let data = s.encode();
        assert_eq!(weight_in(&data, 10), 50);
        assert_eq!(weight_in(&data, 9), 40);
        assert_eq!(weight_in(&data, 8), 0);
        assert_eq!(slots_of(&data).since, 77);
    }
}
