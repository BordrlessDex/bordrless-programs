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
    #[msg("this kit pays rewards in SOL: use claim_sol and share_sol")]
    NativeKit,
    #[msg("this kit pays rewards in bridged SOL: use claim and share")]
    NotNativeKit,
    #[msg("the recipient would be left below the rent-exempt minimum")]
    RecipientBelowRent,
    #[msg("the bridge accounts are missing or wrong")]
    BridgeAccountsMissing,
    #[msg("a coin quoted in tokens passes its Legs after the kit's two extras")]
    LegsMissing,
    #[msg("the Legs passed is not this coin's")]
    WrongLegs,
    #[msg("holder rewards on a coin quoted in tokens need a reward kind (a launchpad from before quote-token rewards)")]
    RewardsOnTokenQuote,
    #[msg("this kit pays holder rewards another way (a leg's token, rounds, or SOL)")]
    WrongRewardKind,
    #[msg("the owner's token account of the reward token is missing or can't receive it")]
    NoTokenAccount,
    #[msg("this round has not been started: start_round first")]
    RoundNotStarted,
    #[msg("this round has already been started")]
    RoundAlreadyStarted,
    #[msg("the holding has not held through the round before")]
    NotHeldThroughRound,
    #[msg("the holding has already been paid this round")]
    AlreadyPaidThisRound,
    #[msg("a round is 10 minutes to a day")]
    InvalidRoundSecs,
    #[msg("the swap to SOL is not the launchpad's swap_to_sol of this leg's vault")]
    WrongSwap,
}
