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
/// Index, in a pool callback's account list, of the launch itself (after the prefix of 5): a
/// native launch's creator fees go there, as lamports.
pub const LAUNCH_INDEX: u8 = 5;
/// Index of the holder vault (the kit config's holding of the quote): holder fees go there.
pub const HOLDER_VAULT_INDEX: u8 = 7;
/// Index of the kit config, read for the holder-fee threshold.
pub const KIT_CONFIG_INDEX: u8 = 8;
/// The most a custom hook's SOL cut may be on either side of a trade (`docs/quote-cut.md`): 10%.
pub const QUOTE_CUT_MAX_BPS: u16 = 1_000;
/// The launchpad's signer of a hook's `quote_cut`: `PDA(["quote-cut", hook], LAUNCH_ID)`, one per
/// hook program, so a hook that passes on the signer it was given can only vouch for itself.
pub const QUOTE_CUT_SIGNER_SEED: &[u8] = b"quote-cut";
/// A custom hook's state account the launchpad passes to its `quote_cut`, writable:
/// `PDA(["state", mint], hook)` (it need not exist).
pub const HOOK_STATE_SEED: &[u8] = b"state";
/// Anchor's discriminator of a hook's `quote_cut` instruction: `sha256("global:quote_cut")[..8]`.
pub const QUOTE_CUT_DISCRIMINATOR: [u8; 8] = [250, 55, 215, 202, 76, 189, 220, 83];
/// Index, in a pool callback's account list, of the first SOL-cut account (the custom hook
/// program; then the trader's holding of the token, the hook's state and the launchpad's signer
/// for the hook): after the prefix and the four extras.
pub const QUOTE_CUT_EXTRAS_INDEX: usize = 9;
/// Number of SOL-cut accounts.
pub const QUOTE_CUT_EXTRAS: usize = 4;
/// `hook_vault`: its slots (`PDA(["slot", mint, [i]], HOOK_VAULT_ID)`, `i` below
/// [`HOOK_VAULT_MAX_SLOTS`]) sell and buy a hook's token cut on the token's own pool, and skip
/// its SOL cut like the creator.
pub const HOOK_VAULT_ID: Pubkey =
    Pubkey::from_str_const("5cojoUStG7WFhHJiSncCUEwTqDuhDLKu4a9BqTbF8jzG");
/// `hook_vault`'s slot seed.
pub const HOOK_VAULT_SLOT_SEED: &[u8] = b"slot";
/// `hook_vault`'s number of slots.
pub const HOOK_VAULT_MAX_SLOTS: u8 = 3;
/// `Launch.status`: on the curve.
pub const STATUS_CURVE: u8 = 0;
/// `Launch.status`: graduated.
pub const STATUS_GRADUATED: u8 = 1;
/// Longest `LaunchConfig.label`, in bytes.
pub const LABEL_MAX: usize = 32;

/// Who may hold a custom hook's upgrade authority (§5.8): a hook must be immutable, or upgradeable
/// only by Bordrless Studio's upgrade key (a hook Studio deployed as "managed", upgraded only
/// through Studio's review and rebuild) or by the protocol's own upgrade authority (Half-Life,
/// tax_hook). A hook anyone else can upgrade could be swapped for other code after its token
/// launched, so `create_config` and `create_launch` refuse it.
pub const HOOK_UPGRADE_AUTHORITIES: [Pubkey; 2] = [
    Pubkey::from_str_const("CS1NRyXNCPxEUP4CRoa26cHQSeSJCxXh5SPijwFhDW6W"),
    Pubkey::from_str_const("5xsibKwtiN6ruxsYrEyWVpV3KcwuzSPbQd1n28a7spEd"),
];

/// The BPF loader 2 and loader v4 (whose programs `check_hook_authority` reads; the upgradeable
/// loader is `BPF_LOADER_UPGRADEABLE_ID` above).
pub const BPF_LOADER_2_ID: Pubkey =
    Pubkey::from_str_const("BPFLoader2111111111111111111111111111111111");
pub const LOADER_V4_ID: Pubkey =
    Pubkey::from_str_const("LoaderV411111111111111111111111111111111111");

/// The most a listed config's author may take of a launch's creator fee: half of it.
pub const MAX_AUTHOR_SHARE_BPS: u16 = 5_000;
/// A partner config's partner (`docs/partner-fee.md`): `PDA(["partner", launch_config], LAUNCH_ID)`.
pub const PARTNER_SEED: &[u8] = b"partner";
/// The most a partner config's partner may take of a launch's creator fee: all of it (with the
/// author's share, at most all of it in all).
pub const MAX_PARTNER_SHARE_BPS: u16 = 10_000;
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

/// The bridge (`bordrless_bridge`), whose `wrap_sol` and `unwrap_sol` move a launch's creator fees
/// between bridged SOL and lamports.
pub const BRIDGE_ID: Pubkey = bordrless_kit::constants::BRIDGE_ID;

/// The companion's seed (`bordrless_companion::constants::COMPANION_SEED`) and the byte past
/// `Companion::LEN` that holds its native flag when the account has room for it
/// (`bordrless_companion::state::NativeSol`); the test suite checks both.
pub const COMPANION_SEED: &[u8] = b"companion";
pub const COMPANION_NATIVE_OFFSET: usize = 308;
/// hook_vault's vault seed and the offset of `Vault.reserved[0]`, its native flag (checked by the
/// test suite).
pub const HOOK_VAULT_SEED: &[u8] = b"vault";
pub const HOOK_VAULT_NATIVE_OFFSET: usize = 638;

// ---- Quote tokens (`docs/token-quotes.md` §2.2): a coin launched against 1 to 6 real SPL Token
// or Token-2022 mints, one DEX curve pool (a "leg") per quote. ----

/// `["legs", mint]`: the `Legs` of a launch quoted in tokens.
pub const LEGS_SEED: &[u8] = b"legs";
/// `["leg-fees", mint, quote_mint]`: a leg's creator-fee token account, owned by the launch.
pub const LEG_FEES_SEED: &[u8] = b"leg-fees";
/// The most quote tokens (legs) one coin launches against.
pub const MAX_LEGS: usize = 6;
/// The least virtual quote a leg opens with, raw units of its token (below it the opening price
/// can't be told apart from zero). `packages/shared/src/quotes.ts` `MIN_LEG_VIRTUAL_QUOTE`.
pub const MIN_LEG_VIRTUAL_QUOTE: u64 = 1_000;
/// `Launch.reserved[QUOTE_KIND_OFFSET]`: [`QUOTE_KIND_TOKENS`] for a launch quoted in tokens; 0
/// for every other launch (the native flag is `reserved[17]`).
pub const QUOTE_KIND_OFFSET: usize = 18;
/// See [`QUOTE_KIND_OFFSET`].
pub const QUOTE_KIND_TOKENS: u8 = 2;
/// The callbacks a leg pool subscribes to: a launch pool's, without overriding the LP fee (no
/// sniper window: the pool's own LP fee applies).
pub const LEG_HOOK_FLAGS: u16 = pool_flags::BEFORE_INITIALIZE
    | pool_flags::BEFORE_SWAP
    | pool_flags::AFTER_SWAP
    | pool_flags::BEFORE_SWAP_RETURNS_DELTA
    | pool_flags::AFTER_SWAP_RETURNS_DELTA;
/// Index, in a leg pool's callback account list, of the leg's fee account (where a launch pool
/// has the launch's quote holding): creator fees go there, in the leg's token.
pub const LEG_FEES_INDEX: u8 = 6;
/// Index, in a leg pool's callback account list, of the launch's `Legs` (where a launch pool has
/// the holder vault), writable: the creator fee is counted there.
pub const LEGS_INDEX: u8 = 7;
/// The kit's accounts a launch quoted in tokens with kit rules passes `create_token_launch` after
/// its quote mints (`docs/token-quotes.md` §6.1): the kit, the token program's signer of its
/// callbacks, the kit config, the kit's registry for the mint, the mint's kit-caller PDA and the
/// kit's event authority.
pub const TOKEN_LAUNCH_KIT_ACCOUNTS: usize = 6;
/// The kit's accounts `open_leg` takes, as its remaining accounts, for a launch with kit rules:
/// the kit, the token program's signer of its callbacks and the kit config (writable); with the
/// kit's id again and the `Legs` they make the coin's slice for the leg's deposit.
pub const OPEN_LEG_KIT_ACCOUNTS: usize = 3;

/// Whether `creator` is the companion creator address of `mint` (`PDA(["creator", mint],
/// COMPANION_ID)`): a companion coin. `revert_launch_to_bridged` uses it to know a launch has a
/// companion that must revert first.
pub fn is_companion_creator(creator: &Pubkey, mint: &Pubkey) -> bool {
    Pubkey::find_program_address(
        &[
            bordrless_kit::constants::COMPANION_CREATOR_SEED,
            mint.as_ref(),
        ],
        &bordrless_kit::constants::COMPANION_ID,
    )
    .0 == *creator
}

/// Keys a transaction always demotes to read-only (the runtime's reserved account keys: the
/// builtin programs and the sysvars). Such a key can never be credited lamports, so a partner config
/// naming one would lock every native claim of its launches (audit N-7): `create_partner_config`
/// refuses them.
pub const RESERVED_ACCOUNT_KEYS: [Pubkey; 31] = [
    Pubkey::from_str_const("AddressLookupTab1e1111111111111111111111111"),
    Pubkey::from_str_const("BPFLoader1111111111111111111111111111111111"),
    Pubkey::from_str_const("BPFLoader2111111111111111111111111111111111"),
    Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111"),
    Pubkey::from_str_const("LoaderV411111111111111111111111111111111111"),
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111"),
    Pubkey::from_str_const("Config1111111111111111111111111111111111111"),
    Pubkey::from_str_const("Ed25519SigVerify111111111111111111111111111"),
    Pubkey::from_str_const("Feature111111111111111111111111111111111111"),
    Pubkey::from_str_const("NativeLoader1111111111111111111111111111111"),
    Pubkey::from_str_const("KeccakSecp256k11111111111111111111111111111"),
    Pubkey::from_str_const("Secp256r1SigVerify1111111111111111111111111"),
    Pubkey::from_str_const("Stake11111111111111111111111111111111111111"),
    Pubkey::from_str_const("StakeConfig11111111111111111111111111111111"),
    Pubkey::from_str_const("11111111111111111111111111111111"),
    Pubkey::from_str_const("Vote111111111111111111111111111111111111111"),
    Pubkey::from_str_const("ZkE1Gama1Proof11111111111111111111111111111"),
    Pubkey::from_str_const("ZkTokenProof1111111111111111111111111111111"),
    Pubkey::from_str_const("Sysvar1111111111111111111111111111111111111"),
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
];
