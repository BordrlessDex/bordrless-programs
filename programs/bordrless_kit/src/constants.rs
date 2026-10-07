//! Constants of the kit. Addresses the kit compares against are constants rather than derived on
//! chain (`docs/hooks-v2.md` §4.12); the unit tests check each against its derivation.

use anchor_lang::prelude::Pubkey;
use bordrless_hook::token_flags;

/// `["kit", mint]`: a token's [`crate::state::KitConfig`]. Its holding of the reward mint is the
/// reward vault.
pub const KIT_SEED: &[u8] = b"kit";
/// `["kit-caller", mint]` under [`LAUNCH_ID`]: the only signer `init` and `graduate` accept.
pub const KIT_CALLER_SEED: &[u8] = b"kit-caller";
/// The launchpad (`bordrless_launch`). A constant: the kit has no crate dependency on the launch.
pub const LAUNCH_ID: Pubkey =
    Pubkey::from_str_const("1jcBymHxBjniZDhNPy51Vgm5Nz7pLUdxa9UBHc4TavC");
/// The DEX (`bordrless_swap`).
pub const SWAP_ID: Pubkey = Pubkey::from_str_const("GyzKSnnEu2uN5bBRecE4XYY2enbfR2D2MtxnbJPGy7hk");
/// The bridge (`bordrless_bridge`).
pub const BRIDGE_ID: Pubkey =
    Pubkey::from_str_const("CtLkuFVitoXHTa86Hfp8KmfSDfqJaMYFWr6EGmQVsKb7");
/// The token program (`bordrless_token`).
pub const TOKEN_ID: Pubkey = bordrless_token::ID;
/// The token program's signer of every callback to the kit: `["hook-authority", KIT_ID]` under
/// the token program. The token program signs each hook's callbacks with a PDA of that hook's
/// id, so a signer another hook received and passed on in a CPI of its own is never this one.
pub const TOKEN_HOOK_AUTHORITY: Pubkey =
    Pubkey::from_str_const("C2Y3B3hZTesJQqLYrZ7qoaZUoRmwYWh5Qh3MuFxruouE");
/// The token program's event authority.
pub const TOKEN_EVENT_AUTHORITY: Pubkey =
    Pubkey::from_str_const("69vhpnkYjtiJgdfU7QsA5Ww7V8FWtvPcZ2r4znuyByvq");
/// This program's `["hook-authority"]` PDA: it signs the token program's `write_hook_data` in
/// `claim`.
pub const HOOK_AUTHORITY: Pubkey =
    Pubkey::from_str_const("2repKA1JgDkBo4AffscVee342dcBcH6c2yfpUTRrAiEN");
/// Bump of [`HOOK_AUTHORITY`].
pub const HOOK_AUTHORITY_BUMP: u8 = 255;
/// Layout version written into new accounts.
pub const VERSION: u8 = 1;
/// Discriminator length.
pub const DISCRIMINATOR_LEN: usize = 8;

/// The kit's modules (`KitConfig.modules`, `docs/hooks-v2.md` §4.2).
pub mod modules {
    /// Holder rewards: a share of each trade paid to holders in SOL, and shares.
    pub const HOLDER_REWARDS: u8 = 1;
    /// Max wallet: no holder above `max_wallet_amount` until graduation.
    pub const MAX_WALLET: u8 = 2;
    /// Creator wallet lock: the creator's wallet sends nothing until `creator_unlock_at`.
    pub const CREATOR_WALLET_LOCK: u8 = 4;
    /// Early-buyer lock: tokens bought from the pool in the early window stay until
    /// `early_unlock_at`.
    pub const EARLY_BUYER_LOCK: u8 = 8;
    /// Every module.
    pub const ALL: u8 = 15;
}

/// Rewards per eligible base unit are kept scaled by this (`acc_per_share`, `rem`, `snapshot`).
pub const SCALE: u128 = 1_000_000_000_000;
/// A share reaches holders linearly over this many seconds.
pub const SHARE_STREAM_SECS: i64 = 3_600;
/// The smallest share: 0.001 SOL.
pub const MIN_SHARE_LAMPORTS: u64 = 1_000_000;
/// The smallest supply the kit installs on (so `min_eligible` is at least 1).
pub const MIN_SUPPLY: u64 = 1_000;
/// The largest supply the kit installs on (1e16 base units), which bounds the reward math.
pub const MAX_SUPPLY: u64 = 10_000_000_000_000_000;
/// `min_eligible = supply / MIN_ELIGIBLE_DIVISOR`: rewards are divided only once holders hold at
/// least a thousandth of the supply.
pub const MIN_ELIGIBLE_DIVISOR: u64 = 1_000;
/// Basis points in one.
pub const BPS: u64 = 10_000;
/// The largest max wallet, in basis points of the supply.
pub const MAX_MAX_WALLET_BPS: u16 = 9_999;
/// The latest creator wallet unlock, counted from `init`.
pub const MAX_CREATOR_LOCK_SECS: i64 = 365 * 86_400;
/// The latest early-buyer unlock, counted from `init`.
pub const MAX_EARLY_LOCK_SECS: i64 = 30 * 86_400;

/// The token hook flags a kit mint carries for `modules` (§4.2): `BEFORE_TRANSFER`, plus
/// `BEFORE_BURN | WRITES_HOOK_DATA` when holder rewards or the early-buyer lock is on. Zero (no
/// hook at all) without any module.
pub const fn mint_flags(modules: u8) -> u16 {
    if modules == 0 {
        return 0;
    }
    let mut flags = token_flags::BEFORE_TRANSFER;
    if modules & (modules::HOLDER_REWARDS | modules::EARLY_BUYER_LOCK) != 0 {
        flags |= token_flags::BEFORE_BURN | token_flags::WRITES_HOOK_DATA;
    }
    flags
}
