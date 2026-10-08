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
        u64::from(self.buyback_bps) + u64::from(self.holders_bps) + u64::from(self.beneficiary_bps)
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
    pub reserved: [u8; 64],
}

impl Companion {
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    pub fn address(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[COMPANION_SEED, mint.as_ref()], &crate::ID)
    }

    pub fn creator(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[CREATOR_SEED, mint.as_ref()], &crate::ID)
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
