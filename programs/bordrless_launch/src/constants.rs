//! Constants of the launchpad. Addresses this program compares against are constants rather than
//! derived on chain (`docs/hooks-v2.md` §4.12); the unit tests check each against its derivation.

use anchor_lang::prelude::Pubkey;
use bordrless_hook::pool_flags;

/// `["config"]`.
pub const CONFIG_SEED: &[u8] = b"config";
/// `["launch", mint]`.
pub const LAUNCH_SEED: &[u8] = b"launch";
/// `["kit-caller", mint]`: the only signer the kit's `init` and `graduate` accept. It signs nothing
/// else and holds nothing.
pub const KIT_CALLER_SEED: &[u8] = bordrless_kit::constants::KIT_CALLER_SEED;
/// `["kit", mint]` under the kit: a token's `KitConfig`.
pub const KIT_SEED: &[u8] = bordrless_kit::constants::KIT_SEED;
/// Bump of this program's `["hook-authority"]` (`3dfEZLdjRcpTJ4RxPzgkqnqaG2FipRdaHgRW72AL6kqL`).
pub const HOOK_AUTHORITY_BUMP: u8 = 255;
/// Bump of `["config"]` (`5n25iAaXFsjs4UgGQM6fCiRhaQcCVQ1L5BgyRZ7UrmaE`).
pub const CONFIG_BUMP: u8 = 253;
/// This program's `["config"]`.
pub const CONFIG_ADDRESS: Pubkey =
    Pubkey::from_str_const("5n25iAaXFsjs4UgGQM6fCiRhaQcCVQ1L5BgyRZ7UrmaE");
/// This program's `["hook-authority"]`: it creates launch pools and finalizes their curves.
pub const LAUNCH_HOOK_AUTHORITY: Pubkey =
    Pubkey::from_str_const("3dfEZLdjRcpTJ4RxPzgkqnqaG2FipRdaHgRW72AL6kqL");
/// The DEX's `["config"]`.
pub const DEX_CONFIG: Pubkey =
    Pubkey::from_str_const("2XLvgczuVACmvvjfRLLAcLwMzFHutbAAro61zZKP8fsx");
/// Layout version.
pub const VERSION: u8 = 1;
/// Discriminator length.
pub const DISCRIMINATOR_LEN: usize = 8;
/// The upgradeable loader.
pub const BPF_LOADER_UPGRADEABLE_ID: Pubkey =
    Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");
/// The kit (`bordrless_kit`): the token hook a launch with rules installs. A constant; there is no
/// config field for it.
pub const KIT_ID: Pubkey = bordrless_kit::KIT_ID;
/// The kit's event authority.
pub const KIT_EVENT_AUTHORITY: Pubkey = bordrless_kit::EVENT_AUTHORITY_AND_BUMP.0;
/// The token program's signer of the kit's callbacks, `["hook-authority", KIT_ID]` under the
/// token program: what the token instructions of a launch's mint take as their hook signer (the
/// kit is the only hook a launch's mint has).
pub const TOKEN_HOOK_AUTHORITY: Pubkey = bordrless_kit::constants::TOKEN_HOOK_AUTHORITY;
/// The token program's event authority.
pub const TOKEN_EVENT_AUTHORITY: Pubkey = bordrless_token::EVENT_AUTHORITY_AND_BUMP.0;
/// The DEX's signer of every callback to this program, `["hook-authority", LAUNCH_ID]` under the
/// DEX. The DEX signs each pool hook's callbacks with a PDA of that hook's id, so a signer another
/// pool hook received and passes on in a CPI here is refused.
pub const DEX_HOOK_AUTHORITY: Pubkey =
    Pubkey::from_str_const("6Ztfr97cUewdViXDXdZUsQq4pz7MYdygvK1WijALjZ5q");
/// The DEX's event authority.
pub const DEX_EVENT_AUTHORITY: Pubkey = bordrless_swap::EVENT_AUTHORITY_AND_BUMP.0;
/// The callbacks a launch pool subscribes to.
pub const LAUNCH_HOOK_FLAGS: u16 = pool_flags::BEFORE_INITIALIZE
    | pool_flags::BEFORE_SWAP
    | pool_flags::AFTER_SWAP
    | pool_flags::BEFORE_SWAP_RETURNS_DELTA
    | pool_flags::AFTER_SWAP_RETURNS_DELTA
    | pool_flags::BEFORE_SWAP_OVERRIDES_FEE;
/// Index, in a pool callback's account list, of the launch's quote holding (prefix of 5, then the
/// launch, then the holding): creator fees go there.
pub const QUOTE_HOLDING_INDEX: u8 = 6;
/// Index of the holder vault (the kit config's holding of the quote): holder fees go there.
pub const HOLDER_VAULT_INDEX: u8 = 7;
/// Index of the kit config, read for the holder-fee threshold.
pub const KIT_CONFIG_INDEX: u8 = 8;
/// `Launch.status`: on the curve.
pub const STATUS_CURVE: u8 = 0;
/// `Launch.status`: graduated.
pub const STATUS_GRADUATED: u8 = 1;
/// Longest `LaunchConfig.label`, in bytes.
pub const LABEL_MAX: usize = 32;

/// The most a listed config's author may take of a launch's creator fee: half of it.
pub const MAX_AUTHOR_SHARE_BPS: u16 = 5_000;
/// The programs a `LaunchConfig` may not name as a custom token hook: the protocol's own (the
/// kit is the launchpad's hook, installed by the inline rules), the system program and the
/// default key.
pub const PROTOCOL_PROGRAMS: [Pubkey; 7] = [
    bordrless_token::ID,
    bordrless_swap::ID,
    bordrless_kit::constants::BRIDGE_ID,
    crate::ID,
    KIT_ID,
    anchor_lang::system_program::ID,
    Pubkey::new_from_array([0; 32]),
];

/// Hard ceilings `init_config` and `set_config` enforce on the token-rule bounds (§5.2); no config
/// can raise them.
pub mod ceilings {
    /// Holder fee per side.
    pub const HOLDER_FEE_BPS: u16 = 500;
    /// Burn per side.
    pub const BURN_BPS: u16 = 500;
    /// Creator fee + holder fee + burn on one side.
    pub const RULES_FEE_BPS: u16 = 1_000;
    /// The smallest a config may set as its smallest max wallet: a cap must be something.
    pub const MIN_MAX_WALLET_BPS: u16 = 1;
    /// Max wallet stays below the whole supply (the kit's own bound).
    pub const MAX_WALLET_BPS: u16 = bordrless_kit::constants::MAX_MAX_WALLET_BPS;
    /// Creator wallet lock: 365 days (the kit's own bound).
    pub const CREATOR_LOCK_SECS: u32 = 365 * 86_400;
    /// Early-buyer window.
    pub const EARLY_WINDOW_SECS: u32 = 3_600;
    /// Early-buyer unlock, counted from the launch: 30 days (the kit's own bound).
    pub const EARLY_LOCK_SECS: u32 = 30 * 86_400;
    /// The smallest launch supply, so the kit's `min_eligible` (supply / 1,000) is at least 1.
    pub const MIN_SUPPLY: u64 = bordrless_kit::constants::MIN_SUPPLY;
    /// The largest launch supply the kit installs on, which bounds its reward math.
    pub const MAX_SUPPLY: u64 = bordrless_kit::constants::MAX_SUPPLY;
}
