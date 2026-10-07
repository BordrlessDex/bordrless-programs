//! Constants of the bridge.

use anchor_lang::prelude::Pubkey;

/// `["config"]`.
pub const CONFIG_SEED: &[u8] = b"config";
/// `["wrapper", underlying_mint]`.
pub const WRAPPER_SEED: &[u8] = b"wrapper";
/// `["wrapped", underlying_mint]`: the BTS mint.
pub const WRAPPED_SEED: &[u8] = b"wrapped";
/// `["sol-vault"]`: the lamports behind wrapped SOL.
pub const SOL_VAULT_SEED: &[u8] = b"sol-vault";
/// Bump of `["config"]` (`Dwf4C8tTMYwicp8cU5MYcTtMQVVJTN7W3LcXCQ1UEhMs`).
pub const CONFIG_BUMP: u8 = 250;
/// Bump of `["sol-vault"]` (`EAcZR2i8A6BbnKRdWuyMVDTuiNkD6qc4RkFpxUYJ9Qba`).
pub const SOL_VAULT_BUMP: u8 = 255;
/// Layout version.
pub const VERSION: u8 = 1;
/// Discriminator length.
pub const DISCRIMINATOR_LEN: usize = 8;
/// The wSOL mint, standing for native SOL.
pub const NATIVE_MINT: Pubkey =
    Pubkey::from_str_const("So11111111111111111111111111111111111111112");
/// SPL Token.
pub const TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// Token-2022.
pub const TOKEN_2022_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
/// The associated token account program.
pub const ATA_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// The upgradeable loader.
pub const BPF_LOADER_UPGRADEABLE_ID: Pubkey =
    Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");
/// Name of wrapped SOL.
pub const SOL_NAME: &str = "Bordrless SOL";
/// Symbol of wrapped SOL.
pub const SOL_SYMBOL: &str = "SOL";
/// Decimals of SOL.
pub const SOL_DECIMALS: u8 = 9;
