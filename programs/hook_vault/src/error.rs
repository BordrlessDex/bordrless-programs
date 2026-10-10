use anchor_lang::prelude::*;

#[error_code]
pub enum VaultError {
    #[msg("the bounty is at most 1% of what a step moves")]
    BountyTooHigh,
    #[msg("one sell takes from 1 to 100 basis points of the pool's quote side")]
    BadMaxSell,
    #[msg("the interval is from a minute to 30 days")]
    BadInterval,
    #[msg("the hook's declared cut of the vault's sell is at most 50%")]
    BadHookCut,
    #[msg("a vault has one to three slots, each with a policy")]
    BadSlots,
    #[msg("a slot's target does not fit its policy")]
    BadTarget,
    #[msg("the pool to buy from must be another launchpad token's pool quoted in bridged SOL, without holder rewards")]
    BadPool,
    #[msg("the hook must be a program of the coin's own, not a protocol program")]
    BadHook,
    #[msg("an account the step needs was not passed")]
    MissingAccount,
    #[msg("the vault is open already")]
    AlreadyOpen,
    #[msg("the vault is not open yet")]
    NotOpen,
    #[msg("the coin has no launch yet")]
    LaunchMissing,
    #[msg("the coin's launch or mint runs another hook than the vault's")]
    WrongHook,
    #[msg("no such slot")]
    WrongSlot,
    #[msg("the slot's owner is not that slot's address")]
    WrongSlotOwner,
    #[msg("not yet: the launch's first minute, or the slot's interval, has not passed")]
    NotDue,
    #[msg("nothing to do yet")]
    NothingToDo,
    #[msg("the pool gives no quote for this amount")]
    NoQuote,
    #[msg("math overflow")]
    MathOverflow,
    #[msg("only a sell-and-buy-and-burn slot buys")]
    NotSellBuyBurn,
    #[msg("a slot is retired only after 60 days with no execution")]
    NotRetirable,
    #[msg("the hook's registry is missing, not the hook's, or out of bounds")]
    HookRegistry,
    #[msg("a vault is made before its coin: the mint must not exist yet")]
    MintExists,
    #[msg("two buy slots of one vault can't buy on the same pool")]
    DuplicatePool,
    #[msg("the wallet paid can't take SOL: a program, a reserved key, or an address of the vault's own")]
    BadWallet,
    #[msg("the bought token's wallet cap leaves no room for a buy")]
    NoRoom,
    #[msg("only the vault's creator (who paid for it) or the coin's mint keypair opens a vault")]
    NotOpener,
}
