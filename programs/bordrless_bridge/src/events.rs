//! Events of the bridge, emitted by self-CPI.

#![allow(missing_docs)]

use anchor_lang::prelude::*;

#[event]
pub struct ConfigSet {
    pub admin: Pubkey,
    pub paused: bool,
    pub ts: i64,
}

#[event]
pub struct WrapperRegistered {
    pub wrapper: Pubkey,
    pub underlying_mint: Pubkey,
    pub underlying_program: Pubkey,
    pub wrapped_mint: Pubkey,
    pub vault: Pubkey,
    pub decimals: u8,
    pub native: bool,
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub registrar: Pubkey,
    pub ts: i64,
}

#[event]
pub struct Wrapped {
    pub wrapper: Pubkey,
    pub underlying_mint: Pubkey,
    pub wrapped_mint: Pubkey,
    pub user: Pubkey,
    pub amount_sent: u64,
    pub amount_minted: u64,
    pub total_wrapped: u64,
    pub slot: u64,
    pub ts: i64,
}

#[event]
pub struct Unwrapped {
    pub wrapper: Pubkey,
    pub underlying_mint: Pubkey,
    pub wrapped_mint: Pubkey,
    pub user: Pubkey,
    pub amount_burned: u64,
    pub total_wrapped: u64,
    pub slot: u64,
    pub ts: i64,
}
