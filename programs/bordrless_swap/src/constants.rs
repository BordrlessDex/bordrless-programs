//! Constants of the DEX.

/// `["config"]`.
pub const CONFIG_SEED: &[u8] = b"config";
/// `["pool", base_mint, quote_mint, lp_fee_bps (u16 LE), hook_program or default]`.
pub const POOL_SEED: &[u8] = b"pool";
/// `["lp", pool]`.
pub const LP_SEED: &[u8] = b"lp";
/// Bump of `["config"]` (`2XLvgczuVACmvvjfRLLAcLwMzFHutbAAro61zZKP8fsx`).
pub const CONFIG_BUMP: u8 = 254;
/// Layout version.
pub const VERSION: u8 = 1;
/// Largest LP fee a pool or a hook may set.
pub const MAX_LP_FEE_BPS: u16 = 9_000;
/// Largest flat protocol fee the config may set (ordinary pools).
pub const MAX_PROTOCOL_FEE_BPS: u16 = 1_000;
/// Largest share of the hooks' cuts the config may set for launch pools: the whole cut.
pub const MAX_PROTOCOL_SHARE_BPS: u16 = 10_000;
/// `Pool.fee_model`: a flat rate of the quote (`protocol_fee_bps`): the pools anyone opens.
pub const FEE_MODEL_FLAT: u8 = 0;
/// `Pool.fee_model`: a share of what the pool's hooks cut (`protocol_share_bps`), in the quote:
/// the pools the launchpad opens (a curve created by `LAUNCHPAD_ID` as its own hook). A launch
/// whose rules collect nothing pays nothing. A curve any other hook program creates is an
/// ordinary pool for the fee: the flat rate, so nobody can open a fee-free pool by deploying a
/// program that signs with its own `["hook-authority"]`.
pub const FEE_MODEL_SHARE: u8 = 1;
/// The launchpad (`bordrless_launch`): the one hook program whose curves get the share model.
/// The test suite checks it equals `bordrless_launch::ID` (the DEX cannot depend on the launch
/// program, which depends on the DEX).
pub const LAUNCHPAD_ID: anchor_lang::prelude::Pubkey =
    anchor_lang::prelude::Pubkey::from_str_const("1jcBymHxBjniZDhNPy51Vgm5Nz7pLUdxa9UBHc4TavC");
/// Discriminator length.
pub const DISCRIMINATOR_LEN: usize = 8;
/// Name of every LP mint.
pub const LP_NAME: &str = "Bordrless LP";
/// Symbol of every LP mint.
pub const LP_SYMBOL: &str = "BLP";
/// The upgradeable loader.
pub const BPF_LOADER_UPGRADEABLE_ID: anchor_lang::prelude::Pubkey =
    anchor_lang::prelude::Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");
