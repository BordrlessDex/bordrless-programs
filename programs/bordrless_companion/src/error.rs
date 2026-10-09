use anchor_lang::prelude::*;

#[error_code]
pub enum CompanionError {
    #[msg("the split must add up to 10,000 basis points")]
    BadSplit,
    #[msg("the bounty is at most 1% of what a step moves")]
    BountyTooHigh,
    #[msg("buybacks need a cap above zero and at least a minute between them")]
    BadBuybackLimits,
    #[msg("a dev bag vests over a year at most")]
    VestTooLong,
    #[msg("an account the step needs was not passed")]
    MissingAccount,
    #[msg("the launch's creator must be this companion's creator address")]
    WrongCreator,
    #[msg("the launch's mint must be this companion's mint")]
    WrongMint,
    #[msg("this companion has launched already")]
    AlreadyLaunched,
    #[msg("this companion has not launched yet")]
    NotLaunched,
    #[msg("a companion launch can't use the creator wallet lock: the companion's own vesting replaces it")]
    CreatorLockUnsupported,
    #[msg("a share for holders needs holder rewards on")]
    HolderRewardsOff,
    #[msg("a companion launch can't use a custom token hook unless it runs that hook's game")]
    CustomHookUnsupported,
    #[msg("nothing to do yet")]
    NothingToDo,
    #[msg("too early: the next buyback is not due yet")]
    BuybackNotDue,
    #[msg("arithmetic overflow")]
    MathOverflow,
    #[msg("the creator address must keep its rent-exempt minimum")]
    Underfunded,
    #[msg("a companion launch can't come from a listed config that pays an author")]
    AuthorShareUnsupported,
    #[msg(
        "with holder rewards on, the beneficiary must be a wallet (the dev bag can only go to one)"
    )]
    BeneficiaryNotAWallet,
    #[msg("the dev's buy is made within ten minutes of the launch")]
    DevBuyWindowClosed,
    #[msg("the dev bag would hold more than max wallet allows a wallet before graduation")]
    DevBagOverMaxWallet,
    #[msg("tokens bought in the early-buyer window stay until it unlocks")]
    EarlyLocked,
    #[msg("the pool can't quote a buyback right now")]
    NoQuote,
    // ---- v2: games. Appended, so every code above keeps its number. ----
    #[msg("an account of the randomness oracle is not the one expected")]
    OracleAccount,
    #[msg("the randomness oracle's fee is above the most a draw pays")]
    OracleFeeTooHigh,
    #[msg("the randomness oracle has not answered yet")]
    OracleNotFulfilled,
    #[msg("the randomness oracle has answered: reveal the draw")]
    OracleFulfilled,
    #[msg("a game's settings are out of bounds")]
    BadGame,
    #[msg("this companion runs no game")]
    NotAGame,
    #[msg("a game coin's launch must use the game's hook, with the callbacks a lottery needs")]
    GameHookMismatch,
    #[msg("the game hook's state is not the one expected (owner, address, magic, mint or round length)")]
    HookState,
    #[msg("the game hook's registry is not the one expected")]
    HookRegistry,
    #[msg("the hook status account is not the one expected")]
    HookStatusAccount,
    #[msg("only the protocol's upgrade authority sets a hook's status")]
    NotProtocolAuthority,
    #[msg("an audit is final, an audited hook can't be blocked, a blocked hook is unblocked only by an audit, and a hook not audited is capped at 0.1 to 10 SOL")]
    BadHookStatus,
    #[msg("a draw is in progress")]
    DrawPending,
    #[msg("no draw is at this step")]
    NoDraw,
    #[msg("the round is not over, or has been drawn already")]
    RoundNotOver,
    #[msg("the pot is below the game's minimum")]
    PotTooSmall,
    #[msg("this attempt's claim window is not open")]
    AttemptClosed,
    #[msg("this holding does not hold the drawn ticket")]
    NotTheWinner,
    #[msg("this owner can't win: the launch, its pool, the creator address, or an address off the curve")]
    NotEligible,
    #[msg("the holding is not the one expected")]
    WrongHolding,
    #[msg("too early for this step")]
    NotDue,
    #[msg("too late: a draw's claims end with the round after the drawn one")]
    DrawLate,
    #[msg("a game hook's registry lists more extra accounts than a launch transaction can carry")]
    TooManyHookExtras,
    /// No longer returned: while the breaker holds, a draw rolls its round over
    /// (`RolloverReason::OracleUnpaid`). Kept so every code after it keeps its number.
    #[msg("the oracle has not answered the last request the pot paid for: the pot pays for a new one only after a backoff")]
    OracleUnanswered,
    #[msg("a game's hook must be Bordrless's lottery hook, or one the protocol has given a status, and not blocked")]
    GameHookNotAccepted,
    #[msg("only a blocked game hook's stranded buyback can be burned")]
    HookNotBlocked,
    #[msg("a draw's seed comes from one of the last 3 slots, which ORAO has not answered: build the draw again from a newer slot")]
    StaleSeed,
    // ---- v2.1: the jackpot and the streak. Appended, so every code above keeps its number. ----
    #[msg("this step is for another kind of game")]
    WrongGameKind,
    #[msg("this holding has no share of the epoch: no weight registered, below the minimum, above its balance, or forfeited by a send")]
    NoShare,
    #[msg("the launch holds creator fees nobody has claimed that could fund this prize: claim the fees first, in the same transaction")]
    FeesUnclaimed,
}
