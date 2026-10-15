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

/// A listed config (`create_listed_config`), after its `LaunchConfigCreated`.
#[event]
pub struct ConfigListed {
    pub config: Pubkey,
    pub author: Pubkey,
    pub author_share_bps: u16,
    pub ts: i64,
}

/// A config author's part of a claim of creator fees (whoever claimed).
#[event]
pub struct AuthorFeesPaid {
    pub launch: Pubkey,
    pub mint: Pubkey,
    pub config: Pubkey,
    pub author: Pubkey,
    pub amount: u64,
    pub paid_total: u64,
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

/// A `LaunchConfig` whose custom hook takes a SOL cut on every trade, decided by the hook's
/// `quote_cut` (`docs/quote-cut.md`), at most these caps per side.
#[event]
pub struct QuoteCutConfigured {
    pub config: Pubkey,
    pub hook: Pubkey,
    pub max_buy_bps: u16,
    pub max_sell_bps: u16,
    pub ts: i64,
}

/// A launch made from such a config: its pool asks the hook on every trade.
#[event]
pub struct LaunchQuoteCut {
    pub launch: Pubkey,
    pub mint: Pubkey,
    pub hook: Pubkey,
    pub max_buy_bps: u16,
    pub max_sell_bps: u16,
    pub ts: i64,
}

/// A trade paid a hook's SOL cut (logged by the pool hook, not by self-CPI: the DEX's callback
/// has no event authority). It goes to the launch's quote holding with the creator fee.
#[event]
pub struct QuoteCutTaken {
    pub launch: Pubkey,
    pub actor: Pubkey,
    /// 0 a sell, 1 a buy.
    pub direction: u8,
    /// What the hook answered (before the cap) and the rate taken.
    pub answered_bps: u16,
    pub bps: u16,
    /// Lamports of bridged SOL.
    pub amount: u64,
}

/// A partner config (`create_partner_config`, `docs/partner-fee.md`), after its
/// `LaunchConfigCreated` (and `ConfigListed` / `QuoteCutConfigured` when it has those too): every
/// launch from it pays `partner` `partner_share_bps` of the creator fee.
#[event]
pub struct PartnerConfigured {
    pub config: Pubkey,
    pub partner: Pubkey,
    pub partner_share_bps: u16,
    pub ts: i64,
}

/// A launch made from a partner config, after its `LaunchCreated`: every claim of its creator fees
/// pays the config's partner `partner_share_bps` of what it takes.
#[event]
pub struct LaunchPartner {
    pub launch: Pubkey,
    pub mint: Pubkey,
    pub config: Pubkey,
    pub partner_share_bps: u16,
    pub ts: i64,
}

/// A partner's part of a claim of creator fees (whoever claimed). `paid_total` counts every launch
/// made from the config.
#[event]
pub struct PartnerFeesPaid {
    pub launch: Pubkey,
    pub mint: Pubkey,
    pub config: Pubkey,
    pub partner: Pubkey,
    pub amount: u64,
    pub paid_total: u64,
    pub slot: u64,
    pub ts: i64,
}

/// A launch moved between bridged SOL and native SOL: its creator fees (`moved`, unwrapped onto
/// the launch or wrapped back into its holding) and its kit's reward vault with it.
#[event]
pub struct LaunchNativeSet {
    pub launch: Pubkey,
    pub mint: Pubkey,
    pub native: bool,
    pub moved: u64,
    pub ts: i64,
}

/// The admin turned native launches on or off for launches made from now on.
#[event]
pub struct NativeLaunchesSet {
    pub on: bool,
    pub ts: i64,
}
