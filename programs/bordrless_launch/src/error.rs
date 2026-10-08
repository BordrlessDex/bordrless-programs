//! Errors of the launchpad.

use anchor_lang::prelude::*;

/// Custom errors (6000 + index).
#[error_code]
pub enum LaunchError {
    #[msg("not the upgrade authority")]
    NotUpgradeAuthority,
    #[msg("not the admin")]
    NotAdmin,
    #[msg("launches are paused")]
    Paused,
    #[msg("invalid config")]
    InvalidConfig,
    #[msg("invalid metadata")]
    InvalidMetadata,
    #[msg("the creator fee is above the maximum")]
    CreatorFeeTooHigh,
    #[msg("the virtual quote reserve is out of bounds")]
    VirtualQuoteOutOfBounds,
    #[msg("the curve parameters could not be derived")]
    BadCurve,
    #[msg("not the DEX's signer for this program's pool callbacks")]
    BadHookSigner,
    #[msg("the launch does not belong to this pool")]
    WrongPool,
    #[msg("the holding passed is not the launch's")]
    WrongHolding,
    #[msg("pools with this hook are created by the launchpad only")]
    OnlyLaunchpadCreatesPools,
    #[msg("the launch has already graduated")]
    AlreadyGraduated,
    #[msg("the pool has not raised the graduation threshold yet")]
    NotReady,
    #[msg("nothing to claim")]
    NothingToClaim,
    #[msg("not the creator")]
    NotCreator,
    #[msg("wrong program account")]
    WrongProgram,
    #[msg("math overflow")]
    MathOverflow,
    #[msg("a holder fee is above the maximum")]
    HolderFeeTooHigh,
    #[msg("a burn is above the maximum")]
    BurnTooHigh,
    #[msg("creator fee, holder fee and burn add up to more than the maximum on one side")]
    RulesFeeTooHigh,
    #[msg("max wallet must be off or within the bounds")]
    MaxWalletOutOfBounds,
    #[msg("the creator wallet lock is longer than the maximum")]
    CreatorLockTooLong,
    #[msg("the early-buyer lock needs a window within the maximum and an unlock after it, within the maximum")]
    InvalidEarlyLock,
    #[msg("a launch with kit rules needs the kit's accounts")]
    KitAccountsMissing,
    #[msg("a kit account was passed for a launch without that rule")]
    UnexpectedKitAccounts,
    #[msg("a kit account is not at its address")]
    WrongKitAccount,
    #[msg("the holder vault passed is not the launch's")]
    WrongHolderVault,
    #[msg(
        "a config with a custom hook can have no kit rule (holder rewards, max wallet, the locks)"
    )]
    CustomHookWithKitRules,
    #[msg("the custom hook must be an executable program that is none of the protocol's")]
    InvalidCustomHook,
    #[msg("the custom hook's flags must name at least one callback and no unknown bit")]
    InvalidCustomHookFlags,
    #[msg("the custom hook has no registry for this mint: prepare the hook for the mint first")]
    HookRegistryMissing,
    #[msg("the custom hook's extra accounts do not match its registry")]
    HookExtrasMismatch,
    #[msg("a launch with a custom hook needs the hook's accounts")]
    CustomHookAccountsMissing,
    #[msg("custom hook accounts were passed for a launch without one")]
    UnexpectedCustomHookAccounts,
    #[msg("the arguments do not match the launch config")]
    ConfigMismatch,
    #[msg("the hook signer passed is not the token program's signer for the custom hook")]
    WrongHookSigner,
    #[msg("the label is longer than 32 bytes")]
    InvalidLabel,
    #[msg("a listed config's author share is between 1 and 5,000 basis points of the creator fee")]
    InvalidAuthorShare,
    #[msg("this launch pays no config author")]
    NoAuthorShare,
    #[msg("a launch that pays its config's author needs the config and the author's holding")]
    AuthorAccountsMissing,
    #[msg("the signer is not the config's author")]
    NotAuthor,
}
