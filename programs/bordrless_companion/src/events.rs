use anchor_lang::prelude::*;

use crate::state::{GameKind, Split};

#[event]
pub struct CompanionCreated {
    pub companion: Pubkey,
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub beneficiary: Pubkey,
    pub split: Split,
    pub bounty_bps: u16,
    pub max_buyback: u64,
    pub buyback_interval: i64,
    pub vest_secs: i64,
}

#[event]
pub struct CompanionLaunched {
    pub companion: Pubkey,
    pub mint: Pubkey,
    pub ts: i64,
}

#[event]
pub struct DevBought {
    pub companion: Pubkey,
    pub lamports: u64,
    pub tokens: u64,
    pub dev_tokens: u64,
}

#[event]
pub struct FeesClaimed {
    pub companion: Pubkey,
    pub claimed: u64,
    pub bounty: u64,
    pub to_buyback: u64,
    pub to_holders: u64,
    pub to_beneficiary: u64,
    pub cranker: Pubkey,
    /// Of `claimed` less the bounty, the part that came from the launch's hook SOL cut, credited
    /// whole to the pot's bucket and already counted in that bucket's figure: the pot (`PotFunded`
    /// follows for a game's, a Snowball's or a script's), else `to_buyback`, else `to_holders`
    /// (`to_beneficiary` only for a split of the beneficiary alone). Appended 2026-10-15: absent
    /// from events before (read it as 0).
    pub from_hook: u64,
}

#[event]
pub struct BoughtBack {
    pub companion: Pubkey,
    pub spent: u64,
    pub burned: u64,
    pub bounty: u64,
    pub cranker: Pubkey,
}

/// A buyback that waited: the price was above the reference (which moved toward it, when due).
#[event]
pub struct BuybackWaited {
    pub companion: Pubkey,
    pub price: u128,
    pub reference: u128,
}

#[event]
pub struct SharedWithHolders {
    pub companion: Pubkey,
    pub amount: u64,
    pub bounty: u64,
    pub cranker: Pubkey,
}

#[event]
pub struct BeneficiaryPaid {
    pub companion: Pubkey,
    pub lamports: u64,
}

#[event]
pub struct DevReleased {
    pub companion: Pubkey,
    pub tokens: u64,
    pub released: u64,
}

// ---- Games (v2) ----------------------------------------------------------------------------------

/// A game made for a companion before its launch (`create_game`): its split now includes the pot.
#[event]
pub struct GameCreated {
    pub companion: Pubkey,
    pub game: Pubkey,
    pub mint: Pubkey,
    pub kind: GameKind,
    pub hook: Pubkey,
    pub split: Split,
    pub pot_bps: u16,
    pub round_secs: u32,
    pub min_pot: u64,
    pub prize_bps: u16,
    pub claim_window_secs: u32,
    pub max_attempts: u8,
    /// The first round a draw may be for.
    pub first_round: u32,
}

/// A fee claim's pot share (game companions only, next to `FeesClaimed`): what the pot got, and
/// what went to the buyback instead (above the cap of a hook that is not audited, or all of it
/// for a blocked hook).
#[event]
pub struct PotFunded {
    pub companion: Pubkey,
    pub to_pot: u64,
    pub to_buyback: u64,
    pub pending_pot: u64,
}

/// Pot money moved to the buyback: the pot of a blocked hook's game, or what a pot held above a cap
/// lowered since. Moved by `burn_stranded`, it waits a whole stranded period before any burn.
#[event]
pub struct PotToBuyback {
    pub companion: Pubkey,
    pub lamports: u64,
    pub blocked: bool,
    pub pending_pot: u64,
}

/// A draw committed its seed, made from the hash of one of the last `oracle::SEED_SLOTS` slots, so
/// nobody could know its answer: always with its request (`DrawRequested`, the same instruction).
#[event]
pub struct DrawCommitted {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub round: u32,
    /// The round's ticket total, read from the hook's header.
    pub total: u64,
    /// The seed's index in the round.
    pub n: u32,
    pub seed: [u8; 32],
    /// The oracle's request account for the seed.
    pub request: Pubkey,
    /// The slot whose hash is in the seed (the draw named it).
    pub slot: u64,
    pub cranker: Pubkey,
}

/// A draw's seed requested from the oracle, in the instruction that committed it (or a pending
/// request someone had already made for it adopted: then nothing is paid).
#[event]
pub struct DrawRequested {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub round: u32,
    pub total: u64,
    /// The seed's index in the round.
    pub n: u32,
    pub seed: [u8; 32],
    /// The oracle's request account, where the randomness will be.
    pub request: Pubkey,
    /// Whether the companion made the request (else it adopted one ORAO already held).
    pub made: bool,
    /// The oracle's fee, and what the pot sent the oracle payer for this request.
    pub fee: u64,
    pub top_up: u64,
    pub bounty: u64,
    /// Requests the pot has paid for since ORAO last answered one of this game's, this one
    /// included (`Game.paid_streak`): above 1, the last one was still unanswered.
    pub paid_streak: u8,
    /// What the draw will pay.
    pub prize: u64,
    pub pending_pot: u64,
    pub cranker: Pubkey,
}

/// The oracle answered: the draw's randomness, from which every attempt's ticket follows.
#[event]
pub struct DrawRevealed {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub round: u32,
    pub seed: [u8; 32],
    pub randomness: [u8; 64],
    pub cranker: Pubkey,
}

/// A draw paid its winner.
#[event]
pub struct PrizePaid {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub round: u32,
    pub attempt: u8,
    pub ticket: u64,
    pub holding: Pubkey,
    pub winner: Pubkey,
    /// Paid to the winner as SOL, and to the sender.
    pub prize: u64,
    pub bounty: u64,
    pub pending_pot: u64,
    pub cranker: Pubkey,
}

/// Why a round paid no prize.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RolloverReason {
    /// The round had no tickets.
    NoTickets,
    /// The hook's header no longer knows the round's total.
    RoundForgotten,
    /// No attempt's ticket was claimed in its window.
    NoClaim,
    /// The oracle had not answered the round's seed by the end of the draw's claims.
    OracleSilent,
    /// The hook was blocked: the pot went to the buyback.
    Blocked,
    /// The draw could not finish within the round after the drawn one, when its claims end (a
    /// holding remembers two rounds of tickets): drawn too late (after `Game::last_draw`, which
    /// leaves the reveal its margin and a whole claim window), answered or claimed too late.
    Late,
    /// The oracle answered in a form the companion can't read: no new seed is bought for the
    /// round (it would be answered the same way).
    OracleUnreadable,
    /// The pot could not pay for the oracle's request: its breaker held (the pot's last paid
    /// request unanswered, within the backoff), ORAO's fee was above `MAX_REQUEST_FEE`, its
    /// network state unreadable, or the pot short of the request's cost. The draw rolls the round
    /// over without committing a seed (a seed is only ever committed with its request).
    OracleUnpaid,
}

/// A round rolled over: the pot stays for the next draw (or, blocked, went to the buyback).
#[event]
pub struct RolledOver {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub round: u32,
    pub reason: RolloverReason,
    pub pending_pot: u64,
}

/// A pot that had paid no prize for two dormant periods (`retire`) sent to the buyback: nobody is
/// paid it. The game goes on.
#[event]
pub struct PotRetired {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub lamports: u64,
    /// Since when the pot had paid nothing (its last prize or retirement, or the launch).
    pub idle_since: i64,
    pub pending_buyback: u64,
    pub cranker: Pubkey,
}

/// A blocked game hook's buyback that no buy had spent for `STRANDED_SECS` (or `STRANDED_INTERVALS`
/// intervals) burned as SOL: unwrapped and sent to the incinerator. Nobody is paid, the sender
/// included. Never a pot the same call moved there: that is kept a whole wait (`PotToBuyback`).
#[event]
pub struct StrandedBurned {
    pub companion: Pubkey,
    /// Burned: the pending buyback (pots moved there by earlier steps included).
    pub lamports: u64,
    /// Since when no buyback had bought or waited (the latest of the launch, the last buyback,
    /// the reference price's last move, the block, the last burn and the pot's move into the
    /// buyback).
    pub since: i64,
    pub cranker: Pubkey,
}

/// The protocol set what it says of a game hook.
#[event]
pub struct HookStatusSet {
    pub hook: Pubkey,
    pub audited: bool,
    pub pot_cap: u64,
    pub blocked: bool,
    pub authority: Pubkey,
}

// ---- v2.1: the jackpot and the streak ----------------------------------------------------------

/// `create_game_v2`: the game's kind settings (with `GameCreated`), and the terms its hook had
/// when it was made (a hook without a status is not audited: capped).
#[event]
pub struct GameKindSet {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub kind: GameKind,
    pub timer_secs: u32,
    pub min_tokens: u64,
    pub min_streak_secs: u32,
    pub min_weight: u64,
    pub audited: bool,
    /// The pot's cap (0: none, audited).
    pub pot_cap: u64,
}

/// A jackpot round paid its last qualifying buyer.
#[event]
pub struct JackpotPaid {
    pub game: Pubkey,
    pub mint: Pubkey,
    /// The round's number (the count of qualifying buys at its last one).
    pub round: u64,
    pub holding: Pubkey,
    pub winner: Pubkey,
    /// What they bought, and when.
    pub amount: u64,
    pub bought_at: i64,
    /// Paid to the winner as SOL, and to the sender.
    pub prize: u64,
    pub bounty: u64,
    pub pending_pot: u64,
    pub cranker: Pubkey,
}

/// Why a jackpot round was forfeited.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForfeitReason {
    /// Its buyer no longer held what they bought (or sent anything since).
    NotHeld,
    /// Its buyer's address can't be paid (a program's, a sysvar's, a reserved key).
    Unpayable,
    /// It was left unsettled `SETTLE_GRACE_SECS` after its timer ran out.
    Stale,
}

/// A jackpot round closed unpaid: its buyer no longer held what they bought (or sent anything
/// since), can't be paid, or the round was left unsettled too long. The pot stays.
#[event]
pub struct JackpotForfeited {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub round: u64,
    pub reason: ForfeitReason,
    pub buyer: Pubkey,
    pub amount: u64,
    pub bought_at: i64,
    pub pending_pot: u64,
    pub cranker: Pubkey,
}

/// A jackpot round closed paying nothing: its buyer held, but the pot was below its minimum when it
/// was settled (or an empty wallet could not have taken so small a prize). The pot stays.
#[event]
pub struct JackpotUnfunded {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub round: u64,
    pub winner: Pubkey,
    pub amount: u64,
    pub pending_pot: u64,
    pub cranker: Pubkey,
}

/// A streak epoch closed: its pot and total fixed, its claims open until `claims_end`.
#[event]
pub struct EpochClosed {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub epoch: u32,
    /// The epoch's weight total (the hook's header).
    pub total: u64,
    /// What its holders share (locked for them until `claims_end`).
    pub epoch_pot: u64,
    pub claims_end: i64,
    pub pending_pot: u64,
    pub cranker: Pubkey,
}

/// A streak epoch's claims ended: what it did not pay rolls over in the pot.
#[event]
pub struct EpochEnded {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub epoch: u32,
    pub unclaimed: u64,
    pub pending_pot: u64,
}

/// A holding's share of a streak epoch paid to its owner.
#[event]
pub struct ShareClaimed {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub epoch: u32,
    pub holding: Pubkey,
    pub owner: Pubkey,
    /// Its weight, of the epoch's total.
    pub weight: u64,
    pub total: u64,
    /// Paid to the owner as SOL, and to the sender.
    pub share: u64,
    pub bounty: u64,
    /// What the epoch's pot has paid so far.
    pub epoch_paid: u64,
    pub pending_pot: u64,
    pub cranker: Pubkey,
}

// ---- Phase 3a: attestations, audits tied to code, strategies --------------------------------------

/// Studio's attester attested a program's code (`attest`).
#[event]
pub struct HookAttested {
    pub program: Pubkey,
    pub build_hash: [u8; 32],
    pub source_hash: [u8; 32],
    pub template_commit: [u8; 20],
    pub sim_version: u16,
    pub cut_max_bps: u16,
    pub cap_bps: u16,
    pub review: u8,
    pub kind: u8,
    pub programdata_slot: u64,
    pub attester: Pubkey,
}

/// An attestation revoked (by the attester or the protocol's upgrade authority).
#[event]
pub struct AttestationRevoked {
    pub program: Pubkey,
    pub build_hash: [u8; 32],
    pub by: Pubkey,
}

/// `set_hook_status_v2`: the status, with the audited code's hash (zeros when not audited).
#[event]
pub struct HookAuditRecorded {
    pub hook: Pubkey,
    pub audited: bool,
    pub audited_hash: [u8; 32],
    pub authority: Pubkey,
}

/// What kind of program a strategy is, when its game was made.
pub mod strategy_class {
    /// Nobody can change its code.
    pub const IMMUTABLE: u8 = 0;
    /// Its `hook_timelock` can, after a public delay.
    pub const TIMELOCKED: u8 = 1;
    /// Bordrless's keys can (Studio's or the protocol's).
    pub const MANAGED: u8 = 2;
    /// The protocol wrote a status for it.
    pub const STATUS: u8 = 3;
}

/// `create_strategy_game`: a strategy game's terms (with `GameCreated`).
#[event]
pub struct StrategySet {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub strategy: Pubkey,
    /// `strategy_class`.
    pub class: u8,
    pub budget_bps: u16,
    pub max_share_bps: u16,
    pub max_per_tx: u8,
    pub plan_cu_max: u32,
    pub entitle_cu_max: u32,
    pub min_weight: u64,
    pub extras: Vec<Pubkey>,
    /// The terms then: audited only if both the ticket hook and the strategy are, the lower cap
    /// (0: none).
    pub audited: bool,
    pub pot_cap: u64,
}

/// A strategy period planned: its budget is locked for its holders until `claims_end`.
#[event]
pub struct PeriodPlanned {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub period: u32,
    pub total: u64,
    pub budget: u64,
    pub budget_max: u64,
    pub claims_end: i64,
    pub pending_pot: u64,
    pub cranker: Pubkey,
}

/// A strategy period whose plan answered 0: nothing is paid, the pot stays.
#[event]
pub struct PeriodSkipped {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub period: u32,
    pub total: u64,
    pub cranker: Pubkey,
}

/// Why a strategy's answer was refused (never clamped).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnswerFault {
    /// No return data.
    NoAnswer,
    /// Return data set by another program than the strategy.
    WrongProgram,
    /// Return data not exactly 8 bytes.
    BadLength,
    /// More than the bound (`budget_max`, `max_amount`).
    OverBound,
    /// An extra account is no longer the strategy's own.
    Accounts,
}

/// A strategy period closed with nothing: the strategy's plan was refused. The pot stays.
#[event]
pub struct PeriodRejected {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub period: u32,
    pub reason: AnswerFault,
    pub cranker: Pubkey,
}

/// Why a candidate of `pay_strategy` was not paid (the others go on).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateReason {
    /// Not the token program's holding of the mint at its owner's address.
    WrongHolding,
    /// Its owner can't be paid: off the curve, a program, a sysvar, a reserved key, or one of the
    /// game's own accounts.
    NotEligible,
    /// Its weight for the period is below the minimum or above its balance.
    NoWeight,
    /// It has a receipt for the period already.
    AlreadyPaid,
    /// The strategy's answer was refused.
    Answer(AnswerFault),
    /// The strategy answered 0.
    Zero,
    /// The owner's account would stay below its rent-exempt minimum (an empty wallet paid too
    /// little, or a legacy rent-paying account).
    BelowRent,
}

/// A candidate of `pay_strategy` not paid: no receipt, nothing moved.
#[event]
pub struct CandidateRejected {
    pub game: Pubkey,
    pub period: u32,
    pub owner: Pubkey,
    pub reason: CandidateReason,
    /// What the strategy answered, when it was over its bound or too little for an empty wallet.
    pub answered: u64,
}

/// One payment of a strategy period.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrategyPayment {
    pub owner: Pubkey,
    /// Paid to the owner as SOL (the entitlement less the sender's bounty).
    pub amount: u64,
    pub bounty: u64,
}

/// `pay_strategy`: the candidates paid, in one unwrap of the pot.
#[event]
pub struct StrategyPaid {
    pub game: Pubkey,
    pub mint: Pubkey,
    pub period: u32,
    pub payments: Vec<StrategyPayment>,
    /// The sender's bounties, in all.
    pub bounty: u64,
    /// What the period has paid so far (bounties included).
    pub epoch_paid: u64,
    pub pending_pot: u64,
    pub cranker: Pubkey,
}

/// A Snowball made for a companion (`create_snowball`, before the launch).
#[event]
pub struct SnowballCreated {
    pub companion: Pubkey,
    pub mint: Pubkey,
    pub pot_bps: u16,
    pub buy_step_bps: u16,
    pub sell_step_bps: u16,
    pub buy_bps: u16,
    pub sell_bps: u16,
    pub interval: i64,
    pub max_trade: u64,
    pub pool_bps: u16,
    pub flat_trades: bool,
}

/// A Snowball trade on the token's pool: `side` 1 a buy (on a dip), 0 a sell (on a rise).
#[event]
pub struct SnowballTraded {
    pub companion: Pubkey,
    pub side: u8,
    /// A step triggered it (a drop or a rise); false for a flat trade.
    pub on_step: bool,
    /// SOL spent (a buy) or received (a sell), lamports, the bounty not counted.
    pub sol: u64,
    /// Tokens bought or sold.
    pub tokens: u64,
    pub bounty: u64,
    /// The pool's price before and after (the new reference).
    pub price_before: u128,
    pub price_after: u128,
    pub pot_after: u64,
    pub tokens_after: u64,
    pub cranker: Pubkey,
}

/// A Snowball that could not trade on the side the price called for (nothing left to spend or to
/// sell): its reference follows the price, so it stays centred on the market.
#[event]
pub struct SnowballFollowed {
    pub companion: Pubkey,
    pub reference_before: u128,
    pub price: u128,
}

/// Bordrless approved a program's code (`approve`): a new approval, or one rewritten.
#[event]
pub struct ProgramApproved {
    pub program: Pubkey,
    /// `APPROVAL_KIND_TOKEN_HOOK` (0) or `APPROVAL_KIND_SCRIPT` (1).
    pub kind: u8,
    pub code_hash: [u8; 32],
    /// The program's ProgramData slot at the approval.
    pub programdata_slot: u64,
    pub authority: Pubkey,
    /// The program's author (the default key: none).
    pub author: Pubkey,
}

/// Bordrless revoked a program's approval (`revoke_approval`).
#[event]
pub struct ApprovalRevoked {
    pub program: Pubkey,
    pub kind: u8,
    pub code_hash: [u8; 32],
    pub authority: Pubkey,
}

/// A script made for a companion (`create_script`, before the launch), its `companion_init` called.
#[event]
pub struct ScriptCreated {
    pub companion: Pubkey,
    pub mint: Pubkey,
    pub program: Pubkey,
    pub split: Split,
    pub pot_bps: u16,
    pub interval: i64,
    /// The script's author, from its approval (the default key: none, as for an immutable script,
    /// which needs no approval).
    pub author: Pubkey,
}

/// A script companion's fee claim paid the script's author their share (`AUTHOR_SHARE_BPS` of the
/// launcher's part of the creator fee the claim took from the launch, out of that part; 2026-10-15),
/// as SOL.
#[event]
pub struct AuthorPaid {
    pub companion: Pubkey,
    pub author: Pubkey,
    pub amount: u64,
}

/// A script step (`script_step`): what the creator's holdings held before and after the script ran
/// (bridged SOL and the token), the bounty paid from the pot, and the pot after.
#[event]
pub struct ScriptStepped {
    pub companion: Pubkey,
    pub program: Pubkey,
    /// The creator's bridged SOL, lamports, before and after the script (before the bounty).
    pub sol_before: u64,
    pub sol_after: u64,
    /// The creator's tokens, before and after the script.
    pub tokens_before: u64,
    pub tokens_after: u64,
    /// Paid to the sender from the pot (`SCRIPT_BOUNTY`, or nothing when the pot holds less).
    pub bounty: u64,
    /// The pot after the step, the bounty paid.
    pub pot_after: u64,
    pub cranker: Pubkey,
}

// ---- Payouts (Loss cover, 2026-10-12) ----------------------------------------------------------------

/// `create_script_v2` or `set_payout_terms`: the bounds the script's payouts keep within (`PayoutTerms`).
#[event]
pub struct PayoutTermsSet {
    pub companion: Pubkey,
    pub mint: Pubkey,
    pub terms: Pubkey,
    pub step_bps: u16,
    pub holding_bps: u16,
    pub window_bps: u16,
    pub window_secs: u32,
}

/// One entry of a script's payout answer, paid and recorded (`script_step`): `lamports` paid to the
/// holding's owner as SOL (0: a touch, recorded alone), `tag` the script's, passed to the hook's
/// `record_payout` untouched.
#[event]
pub struct ScriptPayout {
    pub companion: Pubkey,
    pub program: Pubkey,
    pub holding: Pubkey,
    pub owner: Pubkey,
    pub lamports: u64,
    pub tag: u32,
    /// What the window has paid so far, this entry included.
    pub window_paid: u64,
    /// What the terms have paid all time, this entry included.
    pub paid_total: u64,
    /// The pot after the step (every entry and the bounty paid).
    pub pot_after: u64,
}

/// A companion's beneficiary part is shared (`set_payees`, before the launch): each payee's basis
/// points of it; the beneficiary keeps the rest.
#[event]
pub struct PayeesSet {
    pub companion: Pubkey,
    pub payees: Vec<PayeeShare>,
}

/// One payee and its share, in `PayeesSet`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PayeeShare {
    pub wallet: Pubkey,
    pub bps: u16,
}

/// A payee was paid its part of the beneficiary part, as SOL, by `withdraw`; `owed` is what it is
/// still owed (its account could not take a payment yet, or was not passed).
#[event]
pub struct PayeePaid {
    pub companion: Pubkey,
    pub wallet: Pubkey,
    pub lamports: u64,
    pub owed: u64,
}

/// A script paid a wallet as SOL (a wallet payout, `PayoutAnswer` version 2), within its terms.
#[event]
pub struct ScriptWalletPayout {
    pub companion: Pubkey,
    pub program: Pubkey,
    pub to: Pubkey,
    pub lamports: u64,
    pub tag: u32,
    /// What the window has paid so far, this entry included.
    pub window_paid: u64,
    /// What the terms have paid all time, this entry included.
    pub paid_total: u64,
    /// The pot after the step (every entry and the bounty paid).
    pub pot_after: u64,
}

/// A script companion's launcher allowed its script to pay wallets (`allow_wallet_payouts`, before
/// the launch).
#[event]
pub struct WalletPayoutsAllowed {
    pub companion: Pubkey,
    pub program: Pubkey,
}

/// A companion moved between bridged SOL (its creator's holding) and native SOL (its creator
/// address's lamports): `moved` the SOL that changed form, `spare` the creator address's spare
/// lamports booked to the beneficiary's part when it turned native.
#[event]
pub struct CompanionNativeSet {
    pub companion: Pubkey,
    pub native: bool,
    pub moved: u64,
    pub spare: u64,
}
