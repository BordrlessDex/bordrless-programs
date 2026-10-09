//! What a game hook reads of its mint's launch: the pool, and whether the launch is still on its
//! bonding curve. Read from the launchpad's `Launch` account at fixed offsets (its fields before
//! `status` are all fixed-size), so a hook needs no dependency on the launchpad's crate.
//!
//! The curve matters to a jackpot. On the curve the DEX refuses liquidity on the pool
//! (`add_liquidity` and `remove_liquidity` fail with `CurveLocked`), so the only way the token
//! leaves the pool's vault for a wallet is a buy. Once graduated, anyone can add liquidity and take
//! it out again, and that removal is a transfer from the pool to a wallet that no token hook can
//! tell from a buy: it would be a qualifying "buy" that costs no fee.

use anchor_lang::prelude::*;

/// Anchor's discriminator of the launchpad's `Launch` account: `sha256("account:Launch")[..8]`.
pub const LAUNCH_DISCRIMINATOR: [u8; 8] = [144, 51, 51, 163, 206, 85, 213, 38];
/// `Launch.status` while the launch is on its bonding curve (the launchpad's `STATUS_CURVE`).
pub const LAUNCH_STATUS_CURVE: u8 = 0;

/// Where the fields a game hook reads sit in the launchpad's `Launch` account (absolute offsets,
/// the discriminator first).
pub mod launch_offsets {
    /// `mint: Pubkey`.
    pub const MINT: usize = 10;
    /// `pool: Pubkey`: the launch's pool on the DEX.
    pub const POOL: usize = 74;
    /// `status: u8`: 0 on the curve, 1 graduated.
    pub const STATUS: usize = 138;
    /// The first byte after `status`.
    pub const END: usize = 139;
}

/// A launch as a game hook sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaunchView {
    /// The launch's pool (never the default key once the launch exists).
    pub pool: Pubkey,
    /// Whether it is still on its bonding curve (no liquidity can be added or removed).
    pub on_curve: bool,
}

/// Reads the launch of `mint` from its account data: the launchpad's discriminator, this mint, a
/// pool. `None` for anything else (an account not written yet, as during the launch's own
/// transaction, or another account).
pub fn parse_launch(data: &[u8], mint: &Pubkey) -> Option<LaunchView> {
    use launch_offsets as o;
    if data.len() < o::END || data[..8] != LAUNCH_DISCRIMINATOR {
        return None;
    }
    if data[o::MINT..o::MINT + 32] != mint.to_bytes() {
        return None;
    }
    let mut pool = [0u8; 32];
    pool.copy_from_slice(&data[o::POOL..o::POOL + 32]);
    let pool = Pubkey::new_from_array(pool);
    if pool == Pubkey::default() {
        return None;
    }
    Some(LaunchView {
        pool,
        on_curve: data[o::STATUS] == LAUNCH_STATUS_CURVE,
    })
}

/// [`parse_launch`] from the account: owned by the launchpad, its data readable. Never panics.
pub fn read_launch(info: &AccountInfo, mint: &Pubkey) -> Option<LaunchView> {
    if *info.owner != crate::LAUNCH_PROGRAM_ID {
        return None;
    }
    let data = info.try_borrow_data().ok()?;
    parse_launch(&data, mint)
}
