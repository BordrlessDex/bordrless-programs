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
    #[msg("the hook answered a fill that is zero, above the input, or not for a buy on a curve")]
    BadFill,
    #[msg("the pool is not a native-SOL pool")]
    NotNativePool,
    #[msg("the pool's bridged-SOL buffer cannot cover this; rebalance the pool first")]
    BufferShort,
    #[msg("the pool's lamports cannot cover this; rebalance the pool first")]
    LamportsShort,
    #[msg("a lamport recipient would be left below the rent-exempt minimum")]
    RecipientBelowRent,
    #[msg("the bridge accounts are missing or wrong (needed to wrap or unwrap SOL)")]
    BridgeAccountsMissing,
    #[msg("a native pool's lamports are below its rent-exempt minimum plus its books")]
    NativeInvariant,
    #[msg("the quote of a native pool cannot be burned")]
    NativeBurn,
    #[msg("the account cannot pay or receive SOL here")]
    WrongNativeAccount,
    #[msg("the DEX must be paused for this")]
    NotPaused,
    #[msg("buffer_all needs a later rebalance_pool of the same pool in the same transaction")]
    NoRebalanceAfter,
    #[msg("only a hook-less or launchpad pool may go native")]
    ForeignHookPool,
    #[msg("this pool is excluded from native SOL")]
    ExcludedPool,
    #[msg("revert the pool's launch to bridged first (pass the launch)")]
    LaunchStillNative,
    // ---- Quote tokens (docs/token-quotes.md §2.1). Appended, so every code above keeps its number. ----
    #[msg("the pool is quoted in an SPL token: it trades through swap_spl, its fees go through collect_protocol_fees_spl, and it never takes liquidity or finalizes")]
    SplPool,
    #[msg("the pool is not quoted in an SPL token")]
    NotSplPool,
    #[msg("a pool quoted in an SPL token is a curve its hook creates: a virtual quote above 0, no quote deposit, no quote hook accounts")]
    SplPoolCurveOnly,
    #[msg("the quote is not an initialized SPL Token or Token-2022 mint (or its extensions do not parse)")]
    QuoteMintInvalid,
    #[msg("the quote mint charges a transfer fee")]
    QuoteMintTransferFee,
    #[msg("the quote mint is non-transferable")]
    QuoteMintNonTransferable,
    #[msg("the quote mint's new accounts start frozen")]
    QuoteMintFrozenByDefault,
    #[msg("the quote mint runs a transfer hook program")]
    QuoteMintTransferHook,
    #[msg("the quote mint is paused")]
    QuoteMintPaused,
    #[msg("the quote mint mints and burns confidentially")]
    QuoteMintConfidential,
    #[msg("the quote mint has an extension pools do not support")]
    QuoteMintUnknownExtension,
    #[msg("the token program passed is not the quote mint's")]
    WrongTokenProgram,
    #[msg("the account is not a token account of the quote mint (or not the one required)")]
    WrongTokenAccount,
    #[msg("the quote of a pool quoted in an SPL token cannot be burned")]
    SplBurn,
    #[msg("the pool's quote vault holds less than its books")]
    SplInvariant,
    #[msg("a pool with the launchpad as its hook is created by the launchpad only")]
    LaunchpadPoolNeedsLaunchpad,
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
