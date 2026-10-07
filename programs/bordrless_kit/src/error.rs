//! Errors of the kit.

use anchor_lang::prelude::*;

/// Custom errors (6000 + index).
#[error_code]
pub enum KitError {
    #[msg("the callback was not signed by the token program's signer for the kit")]
    BadHookSigner,
    #[msg("the kit config or the mint passed is not this token's")]
    WrongMint,
    #[msg("the kit answers before_transfer and before_burn only")]
    UnsupportedOperation,
    #[msg("holder rewards are on but the reward vault was not passed")]
    MissingRewardVault,
    #[msg("the reward vault passed is not the config's")]
    WrongRewardVault,
    #[msg("this token cannot be sent to that owner")]
    DestinationNotAllowed,
    #[msg("the creator's wallet is locked")]
    CreatorLocked,
    #[msg("tokens bought in the early window are locked")]
    EarlyLocked,
    #[msg("the destination would hold more than the max wallet")]
    MaxWalletExceeded,
    #[msg("not the launch's kit caller for this mint")]
    NotKitCaller,
    #[msg("invalid modules")]
    InvalidModules,
    #[msg("invalid max wallet")]
    InvalidMaxWallet,
    #[msg("invalid creator wallet lock")]
    InvalidCreatorLock,
    #[msg("invalid early-buyer lock")]
    InvalidEarlyLock,
    #[msg("the supply is out of bounds")]
    SupplyOutOfBounds,
    #[msg("the mint is not set up for the kit (hook, authorities, max supply or flags)")]
    WrongMintSetup,
    #[msg("the launch reserve is not the launch's holding of the whole supply")]
    WrongReserve,
    #[msg("the reward mint must have no hook, no hook authority and no freeze authority")]
    BadRewardMint,
    #[msg("the kit has already graduated")]
    AlreadyGraduated,
    #[msg("the pool and the launch earn no holder rewards")]
    NotAHolder,
    #[msg("holder rewards are off for this token")]
    RewardsOff,
    #[msg("the holding is not the owner's holding of this token")]
    WrongHolding,
    #[msg("the reward mint passed is not the config's")]
    WrongRewardMint,
    #[msg("the destination is not the owner's holding of the reward mint")]
    WrongDestination,
    #[msg("nothing to claim")]
    NothingToClaim,
    #[msg("a share must be at least 0.001 SOL")]
    ShareTooSmall,
    #[msg("nobody is eligible for holder rewards yet")]
    NoEligibleHolders,
    #[msg("wrong program account")]
    WrongProgram,
    #[msg("math overflow")]
    MathOverflow,
}
