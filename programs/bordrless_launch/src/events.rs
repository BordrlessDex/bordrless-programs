//! Events of the launchpad, emitted by self-CPI.

#![allow(missing_docs)]

use anchor_lang::prelude::*;

use crate::state::{LaunchRules, RuleBounds};

#[event]
pub struct ConfigSet {
    pub admin: Pubkey,
    pub treasury: Pubkey,
    pub quote_mint: Pubkey,
    pub launch_fee_lamports: u64,
    pub lp_fee_bps: u16,
    pub max_creator_fee_bps: u16,
    pub sniper_window_secs: i64,
    pub sniper_start_bps: u16,
    pub curve_bps: u16,
    pub supply: u64,
    pub decimals: u8,
    pub min_virtual_quote: u64,
    pub max_virtual_quote: u64,
    pub paused: bool,
    pub rule_bounds: RuleBounds,
    pub ts: i64,
}

#[event]
pub struct LaunchCreated {
    pub launch: Pubkey,
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub pool: Pubkey,
    pub quote_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub supply: u64,
    pub decimals: u8,
    pub creator_fee_bps: u16,
    pub lp_fee_bps: u16,
    pub sniper_window_secs: i64,
    pub sniper_start_bps: u16,
    pub virtual_quote: u64,
    pub virtual_base: u64,
    pub graduation_quote: u64,
    pub curve_tokens: u64,
    pub reserve_tokens: u64,
    pub launch_fee_lamports: u64,
    /// The token rules, fixed at launch.
    pub rules: LaunchRules,
    /// The kit modules they installed (0: no kit, no token hook).
    pub modules: u8,
    /// The token's `KitConfig` when it has a kit.
    pub kit_config: Option<Pubkey>,
    /// Where holder fees go (the kit's reward vault) when holder rewards are on.
    pub holder_vault: Option<Pubkey>,
    /// The creator wallet unlocks at this time; 0 when off.
    pub creator_unlock_at: i64,
    /// Buys from the pool before this time are locked; 0 when off.
    pub early_window_end: i64,
    /// ... until this time; 0 when off.
    pub early_unlock_at: i64,
    /// The `LaunchConfig` the launch was made from, when it was.
    pub config: Option<Pubkey>,
    /// The creator's own token hook, when the token has one.
    pub custom_hook: Option<Pubkey>,
    /// Its flags; 0 without one.
    pub custom_hook_flags: u16,
    pub slot: u64,
    pub ts: i64,
}

/// A `LaunchConfig` was made (by anyone, with the SDK).
#[event]
pub struct LaunchConfigCreated {
    pub config: Pubkey,
    pub creator: Pubkey,
    pub rules: LaunchRules,
    pub creator_fee_bps: u16,
    pub custom_hook: Option<Pubkey>,
    pub custom_hook_flags: u16,
    pub label: String,
    pub ts: i64,
}

#[event]
pub struct Graduated {
    pub launch: Pubkey,
    pub mint: Pubkey,
    pub pool: Pubkey,
    pub cranker: Pubkey,
    pub topup: u64,
    pub burned: u64,
    pub base_reserve: u64,
    pub quote_reserve: u64,
    pub lp_minted: u64,
    pub supply: u64,
    pub slot: u64,
    pub ts: i64,
}

#[event]
pub struct CreatorFeesClaimed {
    pub launch: Pubkey,
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub amount: u64,
    pub claimed_total: u64,
    pub slot: u64,
    pub ts: i64,
}
