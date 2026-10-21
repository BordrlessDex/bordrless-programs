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
    #[msg("the custom hook can be upgraded by someone other than Bordrless: make it immutable or deploy it with Studio")]
    HookUpgradeable,
    #[msg("the custom hook's program data account is missing or wrong")]
    HookProgramDataMissing,
    // ---- Phase 3a. Appended, so every code above keeps its number. ----
    #[msg("the custom hook's upgrade authority is its timelock: pass its Timelock account (owned by hook_timelock, for this program, a delay of at least 3 days)")]
    HookTimelockInvalid,
    #[msg("the custom hook's timelock holds a proposal of new code: a coin is made on a timelocked hook only while nothing is pending (its author cancels it, or it lands first)")]
    HookTimelockPending,
    // ---- The SOL cut (docs/quote-cut.md). Appended, so every code above keeps its number. ----
    #[msg("a SOL cut needs the config's custom hook and at most 1,000 basis points on each side, buys above 0 (the hook is always asked on buys; it may answer 0)")]
    InvalidQuoteCut,
    #[msg("this coin's hook takes a SOL cut: pass the hook program, the trader's holding of the token, the hook's state and the launchpad's signer for the hook after the pool's four extras (the pool's registry lists them)")]
    QuoteCutAccountsMissing,
    #[msg("a SOL cut account is not the one this trade needs (the custom hook, the trader's holding of the token, the hook's writable [\"state\", mint], the launchpad's [\"quote-cut\", hook])")]
    QuoteCutWrongAccount,
    #[msg("the hook's quote_cut answered something other than its cut in basis points (a u16)")]
    QuoteCutBadAnswer,
    // ---- The partner fee (docs/partner-fee.md). Appended, so every code above keeps its number. ----
    #[msg("a partner's share is 1 to 10,000 basis points of the creator fee, and with the author's share at most 10,000 in all")]
    InvalidPartnerShare,
    #[msg("the partner is the default key")]
    InvalidPartner,
    #[msg("a launch that pays a partner needs the partner's account (writable [\"partner\", config]) and the partner's holding of the quote, after any author accounts")]
    PartnerAccountsMissing,
    #[msg("this launch pays no partner")]
    NoPartnerShare,
    #[msg("a companion coin can't launch from a partner config: its creator fees are the companion's to split (share them with payees instead)")]
    PartnerOnCompanion,
    // ---- Native SOL. Appended, so every code above keeps its number. ----
    #[msg("this launch is not native: its fees are claimed in bridged SOL")]
    NotNativeLaunch,
    #[msg("the DEX must be paused for this")]
    DexNotPaused,
    #[msg("the launch's pool is not native yet: migrate the pool first")]
    PoolNotNative,
    #[msg("a recipient would be left below the rent-exempt minimum")]
    RecipientBelowRent,
    /// Unused since phase 3 (companion coins are native from launch); kept so the codes after it
    /// stay put.
    #[msg("a companion coin stays on bridged SOL until its companion is paid in SOL")]
    CompanionLaunch,
    #[msg("revert the coin's companion to bridged first (pass the companion)")]
    CompanionStillNative,
    #[msg("revert the coin's hook vault to bridged first (pass the vault)")]
    VaultStillNative,
    // ---- Quote tokens (docs/token-quotes.md §2.2). Appended, so every code above keeps its number. ----
    #[msg("this launch is quoted in tokens: it never graduates, and its creator fees are claimed per leg (claim_creator_fees_token)")]
    TokenQuotedLaunch,
    #[msg("this launch is not quoted in tokens")]
    NotTokenLaunch,
    #[msg("a launch quoted in tokens has 1 to 6 quote mints (the remaining accounts) and one virtual quote for each")]
    InvalidLegCount,
    #[msg("a quote token must be an initialized SPL Token or Token-2022 mint")]
    QuoteMintInvalid,
    #[msg("the quote token charges a transfer fee")]
    QuoteMintTransferFee,
    #[msg("the quote token is non-transferable")]
    QuoteMintNonTransferable,
    #[msg("the quote token's new accounts start frozen")]
    QuoteMintFrozenByDefault,
    #[msg("the quote token runs a transfer hook program")]
    QuoteMintTransferHook,
    #[msg("the quote token is paused")]
    QuoteMintPaused,
    #[msg("the quote token mints and burns confidentially")]
    QuoteMintConfidential,
    #[msg("the quote token has an extension pools do not support")]
    QuoteMintUnknownExtension,
    #[msg("SOL is quoted by a SOL launch: wrapped SOL can't be a quote token")]
    SolQuoteToken,
    #[msg("the same quote token is named twice")]
    DuplicateQuoteToken,
    #[msg("a leg's virtual quote is below the minimum (1,000 raw units)")]
    LegVirtualQuoteTooSmall,
    #[msg("unused since 21 Oct 2026: a companion launches a coin quoted in tokens through its launch_token")]
    CompanionTokenLaunch,
    #[msg("the launch has no such leg")]
    NoSuchLeg,
    #[msg("the leg is already open")]
    LegAlreadyOpen,
    #[msg("trading starts once every leg is open")]
    LegsNotOpen,
    #[msg(
        "an account is not the leg's (its quote mint, its program, its pool or its fee account)"
    )]
    WrongLegAccount,
    #[msg("a leg pool's callbacks take the launch's Legs (writable) where a launch pool takes its holder vault")]
    LegsAccountInvalid,
    #[msg("unused since docs/token-quotes.md §7.5: a coin quoted in tokens takes a holder fee")]
    HolderFeeOnTokenLaunch,
    // ---- Workshop items on coins quoted in tokens (docs/token-quotes.md §7). Appended. ----
    #[msg("a custom hook's SOL cut has nothing to be paid in on a coin quoted in tokens: a config with a quote cut can't launch one")]
    QuoteCutOnTokenLaunch,
    #[msg("a config that pays its author or a partner a share of the creator fee can't launch a coin quoted in tokens: those shares are paid by SOL claims")]
    ConfigSharesOnTokenLaunch,
    #[msg("a custom hook on a coin quoted in tokens must list the coin's Legs in its registry: the launchpad's PDA of legs and the mint (Account 1)")]
    HookMissingLegs,
    // ---- Holder rewards in the quote token and the SOL switch (docs/token-quotes.md §7.5). Appended. ----
    #[msg(
        "payouts: flags other than the SOL switch, or a holder round outside 10 minutes to a day"
    )]
    PayoutsInvalid,
    #[msg("a leg trade's holder fee goes to the kit's vault of the leg's token (the kit config's associated token account of it), writable, after the leg's four extras")]
    WrongKitVault,
    #[msg(
        "with the SOL switch on, only the creator takes the creator fee in tokens: it must sign"
    )]
    CreatorSignsTokenClaim,
    #[msg("the quote token has no reference pool: register one (register_quote_pool)")]
    QuoteReferenceMissing,
    #[msg("the pool is not a reference: a Raydium AMM v4, CPMM, CLMM or Pump AMM pool of the token against wrapped SOL or USDC, swapping, its vaults its own, its fee within 0.01% and 3%")]
    ReferencePoolInvalid,
    #[msg("the reference pool holds less than 100 SOL on its SOL (or USDC) side")]
    ReferencePoolShallow,
    #[msg("a registered reference is replaced only by a pool at least 25% deeper")]
    ReferenceNotDeeper,
    #[msg("a reference is replaced at most once an hour")]
    ReferenceReplacedRecently,
    #[msg("the quote token's reference is paused")]
    ReferencePaused,
    #[msg("the reference pool's price history is too short or too old")]
    ReferenceStale,
    #[msg("a swap to SOL is worth at least 0.01 SOL")]
    SwapTooSmall,
    #[msg("only a coin's launch, kit config or companion creator address sells its tokens to SOL here")]
    NotACustodian,
    #[msg("the coin's SOL switch is off")]
    SolSwitchOff,
    #[msg("a companion's creator fee is swapped by the companion (swap_bucket_to_sol)")]
    CompanionCreator,
    #[msg("the reference's swaps for this window are used up: wait for the next 8 slots")]
    SwapWindowFull,
    #[msg("the swap brought less SOL than the reference's minimum")]
    SwapBelowMinimum,
    #[msg("the venue's accounts are not the reference pool's")]
    WrongVenueAccounts,
}

impl From<bordrless_swap::spl::MintRefusal> for LaunchError {
    fn from(r: bordrless_swap::spl::MintRefusal) -> Self {
        use bordrless_swap::spl::MintRefusal;
        match r {
            MintRefusal::NotAMint => LaunchError::QuoteMintInvalid,
            MintRefusal::TransferFee => LaunchError::QuoteMintTransferFee,
            MintRefusal::NonTransferable => LaunchError::QuoteMintNonTransferable,
            MintRefusal::FrozenByDefault => LaunchError::QuoteMintFrozenByDefault,
            MintRefusal::TransferHook => LaunchError::QuoteMintTransferHook,
            MintRefusal::Paused => LaunchError::QuoteMintPaused,
            MintRefusal::Confidential => LaunchError::QuoteMintConfidential,
            MintRefusal::UnknownExtension => LaunchError::QuoteMintUnknownExtension,
        }
    }
}
