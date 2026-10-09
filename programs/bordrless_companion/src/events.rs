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
