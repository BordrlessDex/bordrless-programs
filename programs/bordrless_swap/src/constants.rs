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
pub const DISCRIMINATOR_LEN: usize = 8;/// The launchpad's launch seed (`bordrless_launch::constants::LAUNCH_SEED`; the test suite checks
/// it): `revert_pool_to_bridged` reads a launch pool's launch.
pub const LAUNCH_SEED: &[u8] = b"launch";
/// Byte offset of `bordrless_launch::state::Launch::custom_hook`'s option tag in a launch account.
/// The fields after it are fixed: the hook (32 bytes when some), `custom_hook_flags` (2),
/// `author_share_bps` (2), `author_fees_paid` (8), then `reserved`, whose byte 17 is the native
/// flag. The test suite checks this against `Launch::is_native` for both tags.
pub const LAUNCH_CUSTOM_HOOK_OFFSET: usize = 498;
/// The one bridged-SOL pool that must never go native (its hook, `AwAvBe…`, is a third party's;
/// audit N-9). The hook rule of `migrate_pool_to_native` already refuses it; named as well.
pub const EXCLUDED_POOL: anchor_lang::prelude::Pubkey =
    anchor_lang::prelude::Pubkey::from_str_const("4PvPnTByfrnhA1nNmsaou7GrdQsqJNrkinqLHp2JHFX7");

/// Name of every LP mint.
pub const LP_NAME: &str = "Bordrless LP";
/// Symbol of every LP mint.
pub const LP_SYMBOL: &str = "BLP";
/// The upgradeable loader.
pub const BPF_LOADER_UPGRADEABLE_ID: anchor_lang::prelude::Pubkey =
    anchor_lang::prelude::Pubkey::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");

/// The bridge (`bordrless_bridge`): `collect_protocol_fees_sol` calls its `unwrap_sol`. Constants
/// rather than a crate dependency; the unit tests check each against the bridge's own.
pub const BRIDGE_ID: anchor_lang::prelude::Pubkey =
    anchor_lang::prelude::Pubkey::from_str_const("CtLkuFVitoXHTa86Hfp8KmfSDfqJaMYFWr6EGmQVsKb7");
/// Bridged SOL: the bridge's `["wrapped", NATIVE_MINT]`, the quote of every launch pool.
pub const BRIDGED_SOL_MINT: anchor_lang::prelude::Pubkey =
    anchor_lang::prelude::Pubkey::from_str_const("A49oVhX22ExMwTEtFC6Y8nhBdZ4LJDGhdXLDn4c2f59i");
/// The bridge's `unwrap_sol` instruction discriminator.
pub const UNWRAP_SOL_DISCRIMINATOR: [u8; 8] = [99, 40, 14, 105, 45, 107, 172, 201];
/// The bridge's `wrap_sol` instruction discriminator.
pub const WRAP_SOL_DISCRIMINATOR: [u8; 8] = [47, 62, 155, 172, 131, 205, 37, 201];

/// `Pool.reserved[NATIVE_FLAG_OFFSET]`: 1 when the pool holds its quote (bridged SOL by its mint
/// and seeds) as lamports on the pool account itself (a native pool, `crate::native`).
pub const NATIVE_FLAG_OFFSET: usize = 0;
/// `Pool.reserved[BUFFER_OFFSET..+8]` (u64 LE): a native pool's buffer, the bridged SOL in its
/// quote vault that its books count (the rest of the quote is lamports on the pool).
pub const BUFFER_OFFSET: usize = 1;
/// `Pool.reserved[BUFFER_TARGET_OFFSET..+8]` (u64 LE): the buffer `rebalance` moves toward.
pub const BUFFER_TARGET_OFFSET: usize = 9;

#[cfg(test)]
mod bridge_tests {
    use anchor_lang::Discriminator;

    #[test]
    fn the_bridge_constants_are_the_bridges() {
        let native = anchor_lang::prelude::Pubkey::from_str_const(
            "So11111111111111111111111111111111111111112",
        );
        assert_eq!(super::BRIDGE_ID, bordrless_bridge::ID);
        assert_eq!(
            super::BRIDGED_SOL_MINT,
            bordrless_bridge::client::wrapped_mint_address(&native)
        );
        assert_eq!(
            super::UNWRAP_SOL_DISCRIMINATOR,
            bordrless_bridge::instruction::UnwrapSol::DISCRIMINATOR
        );
        assert_eq!(
            super::WRAP_SOL_DISCRIMINATOR,
            bordrless_bridge::instruction::WrapSol::DISCRIMINATOR
        );
    }
}
