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
    #[msg("a companion launch can't use a custom token hook yet")]
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
}
