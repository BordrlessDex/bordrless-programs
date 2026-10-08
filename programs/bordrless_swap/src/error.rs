//! Errors of the DEX.

use anchor_lang::prelude::*;

/// Custom errors (6000 + index).
#[error_code]
pub enum SwapError {
    #[msg("the amount is zero")]
    ZeroAmount,
    #[msg("the config was already created")]
    AlreadyInitialized,
    #[msg("not the upgrade authority")]
    NotUpgradeAuthority,
    #[msg("not the admin")]
    NotAdmin,
    #[msg("the DEX is paused")]
    Paused,
    #[msg("base and quote are the same mint")]
    SameMint,
    #[msg("the fee is above the maximum")]
    FeeTooHigh,
    #[msg("invalid hook flags")]
    InvalidHookFlags,
    #[msg("a curve pool needs a hook program and must be created by it")]
    CurveNeedsHook,
    #[msg("the hook caller is not the hook program's hook authority")]
    BadHookCaller,
    #[msg("the hook program passed is not the pool's")]
    WrongHookProgram,
    #[msg("the hook program was not passed")]
    HookProgramMissing,
    #[msg("the hook returned a delta that is too large")]
    DeltaTooLarge,
    #[msg("the hook named an invalid delta account")]
    InvalidDeltaAccount,
    #[msg("the pool is a curve: liquidity cannot be added or removed until it is finalized")]
    CurveLocked,
    #[msg("the pool is not a curve")]
    NotCurve,
    #[msg("insufficient liquidity for this trade")]
    InsufficientLiquidity,
    #[msg("the output is below the minimum")]
    Slippage,
    #[msg("nothing arrived in the vault")]
    NothingReceived,
    #[msg("the fee would consume the whole input")]
    FeeExceedsInput,
    #[msg("the deposit is too small")]
    DepositTooSmall,
    #[msg("the LP minted is below the minimum")]
    LpBelowMinimum,
    #[msg("the withdrawal is below the minimum")]
    WithdrawalBelowMinimum,
    #[msg("the holding passed is not the expected vault")]
    WrongVault,
    #[msg("the holding passed is for another mint or owner")]
    WrongHolding,
    #[msg("invalid direction")]
    InvalidDirection,
    #[msg("too many remaining accounts for the counts given")]
    AccountCounts,
    #[msg("hook data is too long")]
    HookDataTooLong,
    #[msg("math overflow")]
    MathOverflow,
    #[msg("a curve pool opens with base only")]
    CurveQuoteNotZero,
    #[msg("the hook answered something this callback or the pool's flags do not allow")]
    UnsupportedHookReturn,
    #[msg("the hook's answer does not decode")]
    InvalidHookReturn,
    #[msg("the hook answered more than three deltas")]
    TooManyDeltas,
    #[msg("the hook answered a delta of zero")]
    ZeroDelta,
    #[msg("the hook answered a burn of a mint that was not passed writable")]
    MintNotWritable,
    #[msg("the protocol fee would consume the whole output")]
    FeeExceedsOutput,
    #[msg("the hook signer is not this program's signer for the pool's hook program")]
    BadHookSigner,
    #[msg("the pool's quote is not bridged SOL")]
    NotBridgedSol,
}

/// The DEX's error for a swap the fees or the curve refuse.
pub fn swap_failure(e: bordrless_core::SwapFailure) -> Error {
    use bordrless_core::SwapFailure;
    match e {
        SwapFailure::FeesExceedInput => SwapError::FeeExceedsInput,
        SwapFailure::InsufficientLiquidity => SwapError::InsufficientLiquidity,
        SwapFailure::FeeExceedsOutput => SwapError::FeeExceedsOutput,
    }
    .into()
}

/// The DEX's error for an answer the hook protocol refuses.
pub fn answer_error(e: bordrless_hook::AnswerError) -> Error {
    use bordrless_hook::AnswerError;
    match e {
        AnswerError::Malformed => SwapError::InvalidHookReturn,
        AnswerError::TooManyDeltas => SwapError::TooManyDeltas,
        AnswerError::ZeroDelta => SwapError::ZeroDelta,
        AnswerError::DuplicateDeltaAccount => SwapError::InvalidDeltaAccount,
        AnswerError::DeltaTooLarge => SwapError::DeltaTooLarge,
        AnswerError::Unsupported => SwapError::UnsupportedHookReturn,
    }
    .into()
}
