//! Errors of the bridge.

use anchor_lang::prelude::*;

/// Custom errors (6000 + index).
#[error_code]
pub enum BridgeError {
    #[msg("the amount is zero")]
    ZeroAmount,
    #[msg("not the upgrade authority")]
    NotUpgradeAuthority,
    #[msg("not the admin")]
    NotAdmin,
    #[msg("the bridge is paused")]
    Paused,
    #[msg("the underlying is not an SPL Token or Token-2022 mint")]
    NotAMint,
    #[msg("the token program passed is not the mint's")]
    WrongTokenProgram,
    #[msg("the vault passed is not the wrapper's")]
    WrongVault,
    #[msg("the token account passed is for another mint")]
    WrongTokenAccount,
    #[msg("nothing arrived in the vault")]
    NothingReceived,
    #[msg("the vault does not hold enough")]
    InsufficientVault,
    #[msg("invalid metadata")]
    InvalidMetadata,
    #[msg("this wrapper is native SOL; use wrap_sol and unwrap_sol")]
    NativeWrapper,
    #[msg("this wrapper is not native SOL")]
    NotNativeWrapper,
    #[msg("math overflow")]
    MathOverflow,
}
