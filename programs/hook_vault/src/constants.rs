//! Seeds, bounds, the guards ported from the companion's buyback, and the programs a vault calls
//! (and nothing else).

use anchor_lang::prelude::Pubkey;

/// `PDA(["vault", mint])`: a coin's vault.
pub const VAULT_SEED: &[u8] = b"vault";
/// `PDA(["slot", mint, [i]])`: slot `i`'s owner, a system-owned address with no data that only this
/// program signs for, and only for its own holdings. Its coin holding is where the coin's hook
/// sends the slot's cut (the delta target).
pub const SLOT_SEED: &[u8] = b"slot";

pub const VERSION: u8 = 1;
pub const BPS: u64 = 10_000;

/// The most slots a vault has: a token hook answers at most this many deltas a transfer.
pub const MAX_SLOTS: usize = bordrless_hook::MAX_DELTAS;

/// Slot policies, fixed at `create_vault`.
pub mod policy {
    /// No slot.
    pub const UNUSED: u8 = 0;
    /// Burns the slot's coin.
    pub const BURN: u8 = 1;
    /// Sells the slot's coin for SOL, paid to a wallet fixed before the launch.
    pub const SELL_FOR_SOL: u8 = 2;
    /// Sells the slot's coin, then buys another launchpad token with the SOL and burns it.
    pub const SELL_BUY_BURN: u8 = 3;
}

/// The most a crank is paid: 1% of the SOL a step moves (the companion's bound).
pub const MAX_BOUNTY_BPS: u16 = 100;
/// One sell takes at most this share of the pool's quote side (basis points), and never more than
/// the launch's fee-based `pool_share_bps`: 1%, the companion's `BUYBACK_POOL_SHARE_BPS`.
pub const MAX_SELL_BPS: u16 = 100;
/// Sells (and buys) are at least a minute apart, and at most 30 days. A slot's next attempt after a
/// guard wait is `MIN_INTERVAL` later.
pub const MIN_INTERVAL: i64 = 60;
pub const MAX_INTERVAL: i64 = 30 * 86_400;
/// The most the coin's own hook may be declared to cut from the vault's sell: half of it. The same
/// bound holds for the cut a `SellBuyBurn` slot declares for the bought token's own hook.
pub const MAX_HOOK_CUT_BPS: u16 = 5_000;

// ---- The guards, ported from the companion (`bordrless_companion::constants`) ------------------

/// A sell's slice of the pool's quote side is at most 1%, as a buyback's
/// (`BUYBACK_POOL_SHARE_BPS`).
pub const POOL_SHARE_BPS: u64 = 100;
/// A buy waits while the price is more than 3% above its reference (`MAX_PREMIUM_BPS`); a sell, its
/// mirror, while the price is more than 3% below its own.
pub const MAX_PREMIUM_BPS: u64 = 300;
pub const MAX_DISCOUNT_BPS: u64 = 300;
/// A wait moves the reference toward the price by 5% for each interval since it last moved.
pub const REFERENCE_STEP_BPS: u64 = 500;
/// The most reference steps one wait catches up (1.05^64, about 23x).
pub const MAX_REFERENCE_STEPS: i64 = 64;
/// Prices are quote per base unit, times this.
pub const PRICE_SCALE: u128 = 1_000_000_000_000;
/// Nothing is sold or bought in a launch's first minute (the sniper fee's window, 30 s, with room).
pub const AFTER_LAUNCH: i64 = 60;
/// A sell or a buy takes no less than the pool's own quote at that moment, less the fees and this.
pub const SLIPPAGE_BPS: u64 = 200;

/// A slot nobody has run for this long (counted from the latest of the vault's opening, its last
/// sell, wait, burn or retirement, and its last buy or buy wait) may be retired by anyone: what it
/// holds is burned, paying nobody. A wait counts as a run: a slot that is cranked while its guard
/// waits is live, and a waiting reference reaches the price at 5% an interval.
pub const RETIRE_SECS: i64 = 60 * 86_400;

/// The programs a vault invokes.
pub const LAUNCH_ID: Pubkey = bordrless_launch::ID;
pub const SWAP_ID: Pubkey = bordrless_swap::ID;
pub const TOKEN_ID: Pubkey = bordrless_token::ID;
pub const BRIDGE_ID: Pubkey = bordrless_bridge::ID;
pub const KIT_ID: Pubkey = bordrless_launch::constants::KIT_ID;
/// Bridged SOL, every launch's quote.
pub const BRIDGED_SOL_MINT: Pubkey = bordrless_swap::constants::BRIDGED_SOL_MINT;
/// The incinerator: lamports sent to it are burned when the block ends. Where `retire` sends a
/// slot's stranded SOL (as the companion's `burn_stranded` does).
pub const INCINERATOR: Pubkey =
    Pubkey::from_str_const("1nc1nerator11111111111111111111111111111111");

/// Keys a `SellForSol` wallet can never be: the runtime's reserved account keys (builtin programs and
/// sysvars, `agave-reserved-account-keys` 4.3; a transaction demotes them to read-only, so every
/// payment to one would fail). The tests hold this list to that crate's.
pub const RESERVED_KEYS: [Pubkey; 31] = [
    Pubkey::from_str_const("AddressLookupTab1e1111111111111111111111111"),
    Pubkey::from_str_const("BPFLoader1111111111111111111111111111111111"),
    Pubkey::from_str_const("BPFLoader2111111111111111111111111111111111"),
    Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111"),
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"),
    Pubkey::from_str_const("Config1111111111111111111111111111111111111"),
    Pubkey::from_str_const("Ed25519SigVerify111111111111111111111111111"),
    Pubkey::from_str_const("Feature111111111111111111111111111111111111"),
    Pubkey::from_str_const("LoaderV411111111111111111111111111111111111"),
    Pubkey::from_str_const("KeccakSecp256k11111111111111111111111111111"),
    Pubkey::from_str_const("Secp256r1SigVerify1111111111111111111111111"),
    Pubkey::from_str_const("StakeConfig11111111111111111111111111111111"),
    Pubkey::from_str_const("Stake11111111111111111111111111111111111111"),
    Pubkey::from_str_const("11111111111111111111111111111111"),
    Pubkey::from_str_const("Vote111111111111111111111111111111111111111"),
    Pubkey::from_str_const("ZkE1Gama1Proof11111111111111111111111111111"),
    Pubkey::from_str_const("ZkTokenProof1111111111111111111111111111111"),
    Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111"),
    Pubkey::from_str_const("SysvarEpochRewards1111111111111111111111111"),
    Pubkey::from_str_const("SysvarEpochSchedu1e111111111111111111111111"),
    Pubkey::from_str_const("SysvarFees111111111111111111111111111111111"),
    Pubkey::from_str_const("Sysvar1nstructions1111111111111111111111111"),
    Pubkey::from_str_const("SysvarLastRestartS1ot1111111111111111111111"),
    Pubkey::from_str_const("SysvarRecentB1ockHashes11111111111111111111"),
    Pubkey::from_str_const("SysvarRent111111111111111111111111111111111"),
    Pubkey::from_str_const("SysvarRewards111111111111111111111111111111"),
    Pubkey::from_str_const("SysvarS1otHashes111111111111111111111111111"),
    Pubkey::from_str_const("SysvarS1otHistory11111111111111111111111111"),
    Pubkey::from_str_const("SysvarStakeHistory1111111111111111111111111"),
    Pubkey::from_str_const("NativeLoader1111111111111111111111111111111"),
    Pubkey::from_str_const("Sysvar1111111111111111111111111111111111111"),
];

/// Owners of accounts that can't be a `SellForSol` wallet: the loaders (a program's data account
/// takes lamports nobody can take out again), the native loader and the sysvar program.
pub const LOADER_OWNERS: [Pubkey; 6] = [
    Pubkey::from_str_const("BPFLoader1111111111111111111111111111111111"),
    Pubkey::from_str_const("BPFLoader2111111111111111111111111111111111"),
    Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111"),
    Pubkey::from_str_const("LoaderV411111111111111111111111111111111111"),
    Pubkey::from_str_const("NativeLoader1111111111111111111111111111111"),
    Pubkey::from_str_const("Sysvar1111111111111111111111111111111111111"),
];

/// A hook's registry the vault reads (a hook-owned account, decoded onto the 32 KiB heap) may list
/// at most this many extra accounts, each PDA at most `MAX_REGISTRY_SEEDS` seeds, each literal seed
/// at most `MAX_REGISTRY_SEED_LEN` bytes, in at most `MAX_REGISTRY_LEN` bytes; checked before it is
/// decoded, so no registry can make a step abort out of memory (the companion's bounds).
pub const MAX_REGISTRY_ACCOUNTS: usize = 8;
pub const MAX_REGISTRY_SEEDS: usize = 16;
pub const MAX_REGISTRY_SEED_LEN: usize = 32;
pub const MAX_REGISTRY_LEN: usize = 1_024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_hold_together() {
        const { assert!(MAX_SLOTS == 3) };
        const { assert!(MAX_SELL_BPS as u64 == POOL_SHARE_BPS) };
        // A slot is retired only well after a keeper could have run it at the longest interval.
        const { assert!(RETIRE_SECS >= 2 * MAX_INTERVAL) };
        const { assert!(MIN_INTERVAL >= AFTER_LAUNCH) };
        // The declared hook cut and Bordrless's quarter of it never take the whole sell.
        const { assert!(MAX_HOOK_CUT_BPS as u64 + (MAX_HOOK_CUT_BPS as u64).div_ceil(4) < BPS) };
    }
}
