//! Events of the kit, emitted by self-CPI from `init`, `graduate`, `claim` and `share`. The
//! callbacks emit none (`docs/hooks-v2.md` §4.12).

#![allow(missing_docs)]

use anchor_lang::prelude::*;

/// The kit was installed on a launch's mint, with every parameter.
#[event]
pub struct KitInstalled {
    pub mint: Pubkey,
    pub kit_config: Pubkey,
    pub launch: Pubkey,
    pub pool: Pubkey,
    pub creator: Pubkey,
    pub reward_mint: Pubkey,
    /// The default key when holder rewards are off.
    pub reward_vault: Pubkey,
    pub modules: u8,
    pub supply: u64,
    pub min_eligible: u64,
    pub max_wallet_bps: u16,
    pub max_wallet_amount: u64,
    pub creator_unlock_at: i64,
    pub early_window_end: i64,
    pub early_unlock_at: i64,
    pub ts: i64,
}

/// The launch graduated: max wallet no longer applies.
#[event]
pub struct KitGraduated {
    pub mint: Pubkey,
    pub ts: i64,
}

/// A holder claimed rewards.
#[event]
pub struct RewardsClaimed {
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    /// What the holder is still owed after this claim (zero unless the vault ran short).
    pub owed_left: u64,
    pub total_claimed: u64,
}

/// Someone shared SOL with the holders; it reaches them over one hour, from now or, while an
/// earlier share still streams, from when that one ends (`KitConfig.stream_end`).
#[event]
pub struct RewardsShared {
    pub mint: Pubkey,
    pub from: Pubkey,
    pub amount: u64,
    pub total_shared: u64,
}
