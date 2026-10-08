//! Seeds, limits and the programs a companion calls (and nothing else).

use anchor_lang::prelude::Pubkey;

/// `PDA(["companion", mint])`: a launch's companion.
pub const COMPANION_SEED: &[u8] = b"companion";
/// `PDA(["creator", mint])`: the launch's creator, a system-owned address with no data that only
/// this program can sign for. Creator fees land in its bridged-SOL holding; the dev bag in its
/// token holding.
pub const CREATOR_SEED: &[u8] = b"creator";

pub const VERSION: u8 = 1;
pub const BPS: u64 = 10_000;
/// The most a step pays whoever sends it: 1% of what it moves.
pub const MAX_BOUNTY_BPS: u16 = 100;
/// Buybacks are at least a minute apart, and at most 30 days.
pub const MIN_BUYBACK_INTERVAL: i64 = 60;
pub const MAX_BUYBACK_INTERVAL: i64 = 30 * 86_400;
/// One buyback spends at least 0.01 SOL of cap (a smaller cap would leave the pending buyback stuck).
pub const MIN_MAX_BUYBACK: u64 = 10_000_000;
/// One buyback spends at most 1% of the pool's quote side (real and virtual), and less on a pool
/// whose fees are low (`steps::pool_share_bps`): a front-runner can only ever move one small slice,
/// smaller than the fees a sandwich around it pays.
pub const BUYBACK_POOL_SHARE_BPS: u64 = 100;
/// A buyback waits while the pool's price is more than 3% above the reference price. A wait raises
/// the reference toward the price by 5% for each interval since it last moved; a buy only lowers it.
pub const MAX_PREMIUM_BPS: u64 = 300;
pub const REFERENCE_STEP_BPS: u64 = 500;
/// The most reference steps one wait catches up (1.05^64, about 23x).
pub const MAX_REFERENCE_STEPS: i64 = 64;
/// Prices are quote per base unit, times this.
pub const PRICE_SCALE: u128 = 1_000_000_000_000;
/// The dev's buy is a launch-time thing: within ten minutes of the launch.
pub const DEV_BUY_WINDOW: i64 = 600;
/// A dev bag vests over a year at most.
pub const MAX_VEST_SECS: i64 = 365 * 86_400;
/// No buyback in the launch's first minute (the sniper fee's window, 30 s, with room).
pub const BUYBACK_AFTER_LAUNCH: i64 = 60;
/// A buyback takes no less than the pool's own quote at that moment, less this (and the fees).
pub const BUYBACK_SLIPPAGE_BPS: u64 = 200;
/// The kit's id, as the kit names the companion (they must agree).
pub const KIT_COMPANION_ID: Pubkey = bordrless_kit::constants::COMPANION_ID;

/// The programs a companion invokes.
pub const LAUNCH_ID: Pubkey = bordrless_launch::ID;
pub const SWAP_ID: Pubkey = bordrless_swap::ID;
pub const TOKEN_ID: Pubkey = bordrless_token::ID;
pub const KIT_ID: Pubkey = bordrless_kit::ID;
pub const BRIDGE_ID: Pubkey = bordrless_bridge::ID;
/// Bridged SOL, every launch's quote.
pub const BRIDGED_SOL_MINT: Pubkey = bordrless_swap::constants::BRIDGED_SOL_MINT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kit_names_this_program() {
        assert_eq!(KIT_COMPANION_ID, crate::ID);
        assert_eq!(
            bordrless_kit::constants::COMPANION_CREATOR_SEED,
            CREATOR_SEED
        );
    }
}
