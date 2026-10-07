//! Accounts of the bridge.

use anchor_lang::prelude::*;

use crate::constants::*;

/// Program configuration at `["config"]`.
#[account]
#[derive(InitSpace, Debug)]
pub struct Config {
    /// Layout version.
    pub version: u8,
    /// Bump.
    pub bump: u8,
    /// May change the config and fix wrapped mints' metadata.
    pub admin: Pubkey,
    /// Stops wrapping (unwrapping always works).
    pub paused: bool,
    /// Wrappers registered so far.
    pub wrappers: u64,
    /// Reserved.
    pub reserved: [u8; 32],
}

impl Config {
    /// Account size.
    pub const LEN: usize = DISCRIMINATOR_LEN + Self::INIT_SPACE;
}

/// A wrapper at `["wrapper", underlying_mint]`.
#[account]
#[derive(InitSpace, Debug)]
pub struct Wrapper {
    /// Layout version.
    pub version: u8,
    /// Bump.
    pub bump: u8,
    /// Bump of the wrapped mint PDA.
    pub wrapped_mint_bump: u8,
    /// True for native SOL.
    pub native: bool,
    /// The underlying mint (the wSOL mint for native SOL).
    pub underlying_mint: Pubkey,
    /// The underlying's token program (the system program for native SOL).
    pub underlying_program: Pubkey,
    /// The BTS mint.
    pub wrapped_mint: Pubkey,
    /// The vault: the wrapper's associated token account, or the SOL vault PDA.
    pub vault: Pubkey,
    /// Decimals (the same on both sides).
    pub decimals: u8,
    /// Wrapped and not yet unwrapped.
    pub total_wrapped: u64,
    /// Registration time.
    pub registered_at: i64,
    /// Who registered it.
    pub registrar: Pubkey,
    /// Reserved.
    pub reserved: [u8; 32],
}

impl Wrapper {
    /// Account size.
    pub const LEN: usize = DISCRIMINATOR_LEN + Self::INIT_SPACE;

    /// The wrapper address of `underlying`.
    pub fn address(underlying: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[WRAPPER_SEED, underlying.as_ref()], &crate::ID)
    }

    /// The wrapped mint address of `underlying`.
    pub fn wrapped_mint_address(underlying: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[WRAPPED_SEED, underlying.as_ref()], &crate::ID)
    }
}
