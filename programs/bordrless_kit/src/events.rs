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

/// The kit's reward vault moved between bridged SOL (its holding) and lamports on its config.
#[event]
pub struct KitNativeSet {
    pub mint: Pubkey,
    pub native: bool,
    /// The rewards moved (unwrapped into lamports, or wrapped back into the holding).
    pub moved: u64,
    pub lamports_base: u64,
    pub ts: i64,
}

/// Holder rewards of a coin quoted in tokens (`docs/token-quotes.md` §7.5.1), after its
/// `KitInstalled`: `kind` 1 (a one-leg coin's token through the accumulator, `reward_mint` and
/// `reward_vault` the leg's token and the config's account of it), 2 (every leg's token in rounds
/// of `round_secs`) or 3 (SOL, native from birth).
#[event]
pub struct LegsRewardsInstalled {
    pub mint: Pubkey,
    pub kind: u8,
    pub round_secs: u32,
    pub reward_mint: Pubkey,
    pub reward_vault: Pubkey,
}

/// A round of a rounds kit started: `supply` the eligible supply it divides by, `amounts` each
/// leg's vault (in leg order, raw units of each leg's token).
#[event]
pub struct RoundStarted {
    pub mint: Pubkey,
    pub round: u32,
    pub supply: u64,
    pub amounts: Vec<u64>,
    pub ts: i64,
}

/// A holding was paid its share of a round: `balance` what it was paid on (the least it held since
/// the round before began), `amounts` what each leg paid (0 for a leg skipped: its token account
/// missing or unable to receive).
#[event]
pub struct RoundPaid {
    pub mint: Pubkey,
    pub round: u32,
    pub owner: Pubkey,
    pub balance: u64,
    pub amounts: Vec<u64>,
}

/// A SOL kit sold `amount_in` of leg `index`'s vault for `lamports` through the launchpad's
/// `swap_to_sol`; `shared` whether they stream over the hour as a share (else they are divided at
/// the next sync, or held while nobody is eligible).
#[event]
pub struct RewardsSwappedToSol {
    pub mint: Pubkey,
    pub index: u8,
    pub quote_mint: Pubkey,
    pub amount_in: u64,
    pub lamports: u64,
    pub shared: bool,
}
