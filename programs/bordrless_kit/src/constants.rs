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
pub const LAUNCH_ID: Pubkey = Pubkey::from_str_const("1jcBymHxBjniZDhNPy51Vgm5Nz7pLUdxa9UBHc4TavC");
/// The companion program (`bordrless_companion`, docs/companions.md): a launch whose creator is
/// `PDA(["creator", mint], COMPANION_ID)` has its creator excluded like the pool. A constant: the
/// companion depends on the kit, not the other way round (its tests check the two agree).
pub const COMPANION_ID: Pubkey =
    Pubkey::from_str_const("6ZUM1gWBH9hBBNoJoaVAGwSftyZ6CUda6vUZTW9MsJuo");
pub const COMPANION_CREATOR_SEED: &[u8] = b"creator";
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

// ---- Coins quoted in tokens (`docs/token-quotes.md` §6.1): the launchpad's `Legs` at
// `["legs", mint]`, read in place. A constant layout: the kit has no crate dependency on the
// launch, whose unit tests check every number below against its own `Legs`. ----

/// `KitConfig.reserved[QUOTE_KIND_OFFSET]`: [`QUOTE_KIND_TOKENS`] for the kit of a coin quoted in
/// tokens (`init_legs`), whose callbacks take the coin's `Legs` after the two extras; 0 for every
/// other kit.
pub const QUOTE_KIND_OFFSET: usize = 9;
/// See [`QUOTE_KIND_OFFSET`] (the launchpad's `Launch.reserved[18]` takes the same value).
pub const QUOTE_KIND_TOKENS: u8 = 2;
/// `["legs", mint]` under [`LAUNCH_ID`].
pub const LEGS_SEED: &[u8] = b"legs";
/// Anchor's discriminator of the launchpad's `Legs`: `sha256("account:Legs")[..8]`.
pub const LEGS_DISCRIMINATOR: [u8; 8] = [0x16, 0xbe, 0x5c, 0xdd, 0xc3, 0x3b, 0xab, 0xbd];
/// Size of a `Legs` account.
pub const LEGS_LEN: usize = 974;
/// Byte offset of `Legs.mint`.
pub const LEGS_MINT_OFFSET: usize = 10;
/// Byte offset of `Legs.count`.
pub const LEGS_COUNT_OFFSET: usize = 74;
/// Byte offset of `Legs.opened` (trading opens when it reaches `count`).
pub const LEGS_OPENED_OFFSET: usize = 75;
/// Byte offset of `Legs.legs[0]`.
pub const LEGS_FIRST_LEG_OFFSET: usize = 76;
/// Bytes of one leg.
pub const LEGS_LEG_LEN: usize = 139;
/// Offset of a leg's `pool` within the leg.
pub const LEGS_LEG_POOL_OFFSET: usize = 51;
/// Offset of a leg's `opened_at` (i64, 0 until `open_leg`) within the leg.
pub const LEGS_LEG_OPENED_AT_OFFSET: usize = 131;
/// The most legs a coin has.
pub const LEGS_MAX: usize = 6;
/// Byte offset of `Legs.reserved[8]`: 1 once the legs together have sold the point a SOL coin
/// would graduate at, which lifts max wallet.
pub const LEGS_LIFTED_OFFSET: usize = 918;
/// Offset of a leg's `quote_mint` within the leg.
pub const LEGS_LEG_QUOTE_MINT_OFFSET: usize = 0;
/// Offset of a leg's `quote_program` (0 SPL Token, 1 Token-2022) within the leg.
pub const LEGS_LEG_QUOTE_PROGRAM_OFFSET: usize = 32;
/// Offset of a leg's `decimals` within the leg.
pub const LEGS_LEG_DECIMALS_OFFSET: usize = 33;

// ---- Holder rewards on coins quoted in tokens (`docs/token-quotes.md` §7.5.1): the kind of reward a
// kit installed by `init_legs` pays, kept in `KitConfig.reserved`, zero on every kit made before. ----

/// `KitConfig.reserved[REWARD_KIND_OFFSET]` (byte 386): the reward kind of a coin quoted in tokens,
/// 0 for every other kit (and for a token coin without holder rewards).
pub const REWARD_KIND_OFFSET: usize = 10;
/// `KitConfig.reserved[11..15]` (bytes 387..391): a rounds kit's round length in seconds (u32).
pub const ROUND_SECS_OFFSET: usize = 11;
/// `KitConfig.reserved[15]` (byte 391): a token kit's reward program, 0 SPL Token, 1 Token-2022.
pub const REWARD_PROGRAM_OFFSET: usize = 15;
/// `KitConfig.reserved[16]` (byte 392): a token kit's reward mint's decimals.
pub const REWARD_DECIMALS_OFFSET: usize = 16;
/// Kind 1: a one-leg coin's rewards in its leg's token, through the kit's own accounting, the vault
/// being the config's associated token account of that token (continuous, claimable any time).
pub const REWARD_KIND_TOKEN: u8 = 1;
/// Kind 2: a coin of two to six legs: every leg's token in rounds (`KitRounds`), each holding paid
/// its share of each leg's vault on the least it held since the round before began.
pub const REWARD_KIND_ROUNDS: u8 = 2;
/// Kind 3: rewards in SOL (the launch's SOL switch): a native kit from birth, fed by
/// `swap_rewards_to_sol`, which sells the leg vaults' tokens through the launchpad's `swap_to_sol`.
pub const REWARD_KIND_SOL: u8 = 3;
/// The shortest round a rounds kit takes (10 minutes).
pub const MIN_ROUND_SECS: u32 = 600;
/// The longest round (a day).
pub const MAX_ROUND_SECS: u32 = 86_400;
/// The round length when the launch names none (an hour).
pub const DEFAULT_ROUND_SECS: u32 = 3_600;
/// `["kit-rounds", mint]`: a rounds kit's [`crate::state::KitRounds`].
pub const KIT_ROUNDS_SEED: &[u8] = b"kit-rounds";
/// The launchpad's `swap_to_sol` discriminator (`sha256("global:swap_to_sol")[..8]`), which
/// `swap_rewards_to_sol` calls by CPI (the kit has no crate dependency on the launchpad; the
/// launchpad's unit tests check the value against its own).
pub const SWAP_TO_SOL_DISCRIMINATOR: [u8; 8] = [0x12, 0x3c, 0xff, 0x27, 0x66, 0x80, 0x3e, 0x8c];

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
