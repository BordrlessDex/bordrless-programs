//! Game ticket standard v1 (`docs/studio-companions.md` §1, the monorepo's): how a game coin's token
//! hook records who holds tickets, and how the Bordrless companion, which holds the pot, reads them.
//!
//! A game coin's hook writes; the companion only reads. The hook never sees SOL and the companion
//! never calls the hook, so the code that holds the pot never runs code nobody audited.
//!
//! # The header
//!
//! The hook's state lives at `PDA(["state", mint], hook)` ([`state_address`]). After Anchor's 8-byte
//! discriminator it starts with a [`GameHeader`] at fixed offsets ([`header_offsets`]), which a
//! reader parses from raw bytes without knowing the hook's own account type ([`GameHeader::parse`],
//! [`read_state`]). A hook declares it as the first field of its state account, so Borsh lays it out
//! exactly there.
//!
//! Rounds are `floor(unix_time / round_secs)` ([`round_of`]). Both programs compute them from the
//! clock, so neither needs a start time. The first write of a new round rolls the header
//! ([`GameHeader::roll`]): `(round, total)` moves to `(prev_round, prev_total)`, and the new round
//! starts with no tickets. [`GameHeader::total_of`] answers a finished round's ticket total from the
//! header, or `None` once the header has forgotten it.
//!
//! # Tickets
//!
//! Each round has a ticket space `[0, total)`. A holding's tickets are a range `[start, start +
//! weight)` of it, kept in the holding's 64 bytes of hook data ([`Slots`], [`slot_offsets`]): the
//! current round's range, the previous round's (so a winner who trades in the next round can still
//! claim), and `since`. The current slot's `round` is the round the holding was last written in,
//! with or without tickets that round.
//!
//! **A round's tickets are the tokens held since it began.** A holding gets its range of a round
//! at its first write that round (a send, a receive or an `enter`), and only then
//! ([`written_in`]): for what it has held since the round began, as far as that write keeps it.
//! Tokens that arrive during a round count from the next one. Every change of an eligible
//! holding's balance is a write (the hook sees every transfer and burn), so a holding not written
//! yet this round holds exactly what it held when the round began.
//!
//! - **Weight is tokens held through the round.** Splitting a balance across wallets, or moving it
//!   between them, changes nobody's odds but the mover's (a send cuts the sender's range; the
//!   receiver gets nothing for it until the next round).
//! - **Ranges only shrink.** A send cuts both slots to the balance left ([`Slots::shrink`]); what is
//!   cut is dead for ever. A range always starts at the round's total ([`register`]) and never
//!   grows. So no ticket number is ever given to two ranges, or given again once dead, and a
//!   round's total never decreases.
//! - **Dead tickets are bounded by the round's opening holdings.** Each holding registers at most
//!   once a round, at most what it held when the round began, so a round's total is at most the
//!   tokens its eligible holders held at its start; moving tokens around (self-transfers, dust,
//!   `enter`s) can only cut the mover's own range, never add tickets.
//! - **No ticket ever exceeds a balance**: weights are set to a balance and only cut after.
//!
//! The rules a hook applies are [`on_send`], [`on_receive`] and [`on_enter`] (built on [`register`]
//! and [`Slots::shrink`]); `lottery_hook` is the reference implementation.
//!
//! **A round's claims end with the round after it.** A holding remembers two rounds: the one it was
//! last written in and the one before. A draw of round `r` happens in round `r + 1`, and as long as
//! its claims are made in `r + 1` the winner's range of `r` is still there, whatever was written to
//! the holding meanwhile. In `r + 2` a holding written in both `r + 1` and `r + 2` has forgotten
//! `r`, so the companion ends every draw of `r` at [`round_end`]`(r + 1)`.
//!
//! # Draws and claims
//!
//! A draw picks `x = draw_index(R, k, total_r)`: the first 8 bytes of `sha256(R ‖ k)` as a
//! little-endian u64, modulo the round's total ([`draw_index`]). The holding whose range for that
//! round contains `x` wins if the range's weight is at most its balance ([`wins`]) and its owner may
//! hold tickets ([`eligible`]): never the launch pool, the launch, the companion's creator address,
//! or any address off the ed25519 curve.

//!
//! # Phase 2: the jackpot and the streak
//!
//! Two more kinds of game use the same header and slots, each adding a header of its own right
//! after the base header (offset 120, where a lottery hook's own fields start: the companion reads
//! it only for a game of that kind, after its magic): [`jackpot`] (`BRJ1`, the last-buyer jackpot)
//! and [`streak`] (`BRS1`, the diamond-hands streak). Every phase-1 offset, rule and helper above is
//! unchanged. [`launch`] reads what a hook needs of its launch (the pool, and whether it is on its
//! bonding curve), [`holding`] a holding (for `enter`); [`cpi`] is the one call a game hook makes
//! (`enter`'s `write_hook_data`).

#![forbid(unsafe_code)]

use anchor_lang::prelude::*;

pub mod cpi;
pub mod holding;
pub mod jackpot;
pub mod launch;
pub mod streak;

pub use cpi::{write_own_hook_data, TOKEN_EVENT_AUTHORITY, TOKEN_PROGRAM_ID};
pub use holding::{parse_holding, read_holding, HoldingView};
pub use jackpot::{
    jackpot_mark, jackpot_on_buy, jackpot_on_receive, jackpot_on_send, jackpot_winner_holds,
    qualifying_buy, set_jackpot_mark, settle_round, timer_over, valid_timer_secs, EndedRound,
    JackpotHeader, JackpotRound, JACKPOT_ENDED_ROUNDS, JACKPOT_MAGIC,
};
pub use launch::{parse_launch, read_launch, LaunchView};
pub use streak::{
    share_of, streak_on_enter, streak_on_receive, streak_on_send, streak_qualifies, streak_weight,
    StreakHeader, STREAK_MAGIC,
};

/// The first four bytes of a game hook's header: `"BRG1"`, the standard's layout version 1.
pub const MAGIC: [u8; 4] = *b"BRG1";
/// Seed of a game hook's state: `PDA(["state", mint], hook)`.
pub const STATE_SEED: &[u8] = b"state";
/// Anchor's account discriminator, which precedes the header.
pub const DISCRIMINATOR_LEN: usize = 8;
/// Bytes of the header: magic 4, mint 32, round_secs 4, round 4, total 8, prev_round 4,
/// prev_total 8, last_buyer 32, last_amount 8, last_buy_at 8.
pub const HEADER_LEN: usize = 112;
/// Bytes of hook data per holding (the token standard's `Holding.hook_data`).
pub const HOOK_DATA_LEN: usize = 64;
/// The shortest round: an hour.
pub const MIN_ROUND_SECS: u32 = 3_600;
/// The longest round: 30 days.
pub const MAX_ROUND_SECS: u32 = 30 * 86_400;

/// The launchpad (`bordrless_launch`): its `["launch", mint]` holds a launch's supply and never
/// holds tickets.
pub const LAUNCH_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("1jcBymHxBjniZDhNPy51Vgm5Nz7pLUdxa9UBHc4TavC");
/// The launchpad's `["launch", mint]` seed.
pub const LAUNCH_SEED: &[u8] = b"launch";
/// The companion (`bordrless_companion`): its `["creator", mint]` is a companion launch's creator,
/// which holds the fees, the pot and the dev bag, and never holds tickets.
pub const COMPANION_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("6ZUM1gWBH9hBBNoJoaVAGwSftyZ6CUda6vUZTW9MsJuo");
/// The companion's `["creator", mint]` seed.
pub const COMPANION_CREATOR_SEED: &[u8] = b"creator";

/// Where each header field sits in a game hook's state account (absolute offsets, the 8-byte
/// discriminator first). Integers are little-endian.
pub mod header_offsets {
    /// `magic: [u8; 4]`, [`super::MAGIC`].
    pub const MAGIC: usize = 8;
    /// `mint: Pubkey`.
    pub const MINT: usize = 12;
    /// `round_secs: u32`.
    pub const ROUND_SECS: usize = 44;
    /// `round: u32`: the round of the last write.
    pub const ROUND: usize = 48;
    /// `total: u64`: that round's ticket total.
    pub const TOTAL: usize = 52;
    /// `prev_round: u32`: the round the header held before its last roll (the last earlier round
    /// with a write, or the round the hook was prepared in); 0 until the first roll.
    pub const PREV_ROUND: usize = 60;
    /// `prev_total: u64`: that round's ticket total.
    pub const PREV_TOTAL: usize = 64;
    /// `last_buyer: Pubkey`: the owner of the last qualifying buy (jackpot games).
    pub const LAST_BUYER: usize = 72;
    /// `last_amount: u64`: the tokens it bought.
    pub const LAST_AMOUNT: usize = 104;
    /// `last_buy_at: i64`: when.
    pub const LAST_BUY_AT: usize = 112;
    /// The first byte after the header: the hook's own fields start here.
    pub const END: usize = 120;
}

/// Where each field sits in a holding's 64 bytes of hook data. Integers are little-endian. A slot
/// whose weight is 0 holds no tickets: the previous slot is then written as zeros, the current one
/// as its round only (the round the holding was last written in).
pub mod slot_offsets {
    /// `round: u32` of the current slot: the round the holding was last written in (its range's
    /// round when it has tickets that round).
    pub const ROUND: usize = 0;
    /// `start: u64` of the current slot.
    pub const START: usize = 4;
    /// `weight: u64` of the current slot.
    pub const WEIGHT: usize = 12;
    /// `round: u32` of the previous round's slot.
    pub const PREV_ROUND: usize = 20;
    /// `start: u64` of the previous round's slot.
    pub const PREV_START: usize = 24;
    /// `weight: u64` of the previous round's slot.
    pub const PREV_WEIGHT: usize = 32;
    /// `since: i64`: when the holding last sent (or burned) anything, or first received.
    pub const SINCE: usize = 40;
    /// 16 bytes the hook may use for itself.
    pub const FREE: usize = 48;
    /// Length of the free bytes.
    pub const FREE_LEN: usize = 16;
}

/// Why a game hook's state account was not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameError {
    /// The account is not owned by the game's hook program.
    WrongOwner,
    /// The account is not the hook's `PDA(["state", mint])`.
    WrongAddress,
    /// The account is shorter than the discriminator and the header.
    TooShort,
    /// The header does not start with [`MAGIC`].
    BadMagic,
    /// The header is for another mint.
    WrongMint,
    /// The account's data is borrowed mutably elsewhere.
    Unreadable,
}

// ------------------------------------------------------------------------------------- rounds

/// The round `now` falls in: `floor(now / round_secs)`. 0 before 1970 or for a `round_secs` of 0
/// (never a valid header), and at most `u32::MAX`: it never panics, so a hook callback can always
/// compute it.
pub fn round_of(now: i64, round_secs: u32) -> u32 {
    if round_secs == 0 || now <= 0 {
        return 0;
    }
    let round = (now as u64) / u64::from(round_secs);
    u32::try_from(round).unwrap_or(u32::MAX)
}

/// When `round` starts: `round * round_secs` (saturating).
pub fn round_start(round: u32, round_secs: u32) -> i64 {
    i64::from(round).saturating_mul(i64::from(round_secs))
}

/// When `round` ends (the next round's start). A round's tickets are final once `now >=
/// round_end(round, round_secs)`: every write after that goes to a later round.
pub fn round_end(round: u32, round_secs: u32) -> i64 {
    round_start(round, round_secs).saturating_add(i64::from(round_secs))
}

/// Whether `round_secs` is within [`MIN_ROUND_SECS`] and [`MAX_ROUND_SECS`].
pub fn valid_round_secs(round_secs: u32) -> bool {
    (MIN_ROUND_SECS..=MAX_ROUND_SECS).contains(&round_secs)
}

// ------------------------------------------------------------------------------------- header

/// The header at the start of a game hook's state ([`header_offsets`]). A hook declares it as the
/// first field of its state account; the companion reads it from raw bytes ([`GameHeader::parse`]).
#[derive(
    AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, Debug, Default, PartialEq, Eq,
)]
pub struct GameHeader {
    /// [`MAGIC`].
    pub magic: [u8; 4],
    /// The game's mint.
    pub mint: Pubkey,
    /// Seconds per round, within [`MIN_ROUND_SECS`] and [`MAX_ROUND_SECS`].
    pub round_secs: u32,
    /// The round of the last write.
    pub round: u32,
    /// That round's ticket total: every range of the round lies in `[0, total)`.
    pub total: u64,
    /// The round the header held before its last roll (the last earlier round with a write, or the
    /// round the hook was prepared in); 0 until the first roll.
    pub prev_round: u32,
    /// That round's ticket total.
    pub prev_total: u64,
    /// The owner of the last qualifying buy (jackpot games; the default key when unused).
    pub last_buyer: Pubkey,
    /// The tokens of the last qualifying buy.
    pub last_amount: u64,
    /// When it was made (0 when unused).
    pub last_buy_at: i64,
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&data[at..at + 4]);
    u32::from_le_bytes(b)
}

fn u64_at(data: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[at..at + 8]);
    u64::from_le_bytes(b)
}

fn i64_at(data: &[u8], at: usize) -> i64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[at..at + 8]);
    i64::from_le_bytes(b)
}

fn key_at(data: &[u8], at: usize) -> Pubkey {
    let mut b = [0u8; 32];
    b.copy_from_slice(&data[at..at + 32]);
    Pubkey::new_from_array(b)
}

impl GameHeader {
    /// The header of a game prepared at `now` for `mint`: no tickets yet, the current round and its
    /// predecessor both known to hold none.
    pub fn new(mint: Pubkey, round_secs: u32, now: i64) -> Self {
        Self {
            magic: MAGIC,
            mint,
            round_secs,
            round: round_of(now, round_secs),
            ..Self::default()
        }
    }

    /// Parses the header from a state account's data (the discriminator first), checking its length
    /// and [`MAGIC`]. The discriminator itself is the hook's own and is not checked.
    pub fn parse(data: &[u8]) -> core::result::Result<Self, GameError> {
        use header_offsets as o;
        if data.len() < o::END {
            return Err(GameError::TooShort);
        }
        if data[o::MAGIC..o::MAGIC + 4] != MAGIC {
            return Err(GameError::BadMagic);
        }
        Ok(Self {
            magic: MAGIC,
            mint: key_at(data, o::MINT),
            round_secs: u32_at(data, o::ROUND_SECS),
            round: u32_at(data, o::ROUND),
            total: u64_at(data, o::TOTAL),
            prev_round: u32_at(data, o::PREV_ROUND),
            prev_total: u64_at(data, o::PREV_TOTAL),
            last_buyer: key_at(data, o::LAST_BUYER),
            last_amount: u64_at(data, o::LAST_AMOUNT),
            last_buy_at: i64_at(data, o::LAST_BUY_AT),
        })
    }

    /// [`GameHeader::parse`], and the header is for `mint`.
    pub fn read(data: &[u8], mint: &Pubkey) -> core::result::Result<Self, GameError> {
        let header = Self::parse(data)?;
        if header.mint != *mint {
            return Err(GameError::WrongMint);
        }
        Ok(header)
    }

    /// The header's bytes as they sit after the discriminator (what Borsh writes for it).
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        let base = DISCRIMINATOR_LEN;
        let mut put = |at: usize, bytes: &[u8]| {
            out[at - base..at - base + bytes.len()].copy_from_slice(bytes);
        };
        use header_offsets as o;
        put(o::MAGIC, &self.magic);
        put(o::MINT, self.mint.as_ref());
        put(o::ROUND_SECS, &self.round_secs.to_le_bytes());
        put(o::ROUND, &self.round.to_le_bytes());
        put(o::TOTAL, &self.total.to_le_bytes());
        put(o::PREV_ROUND, &self.prev_round.to_le_bytes());
        put(o::PREV_TOTAL, &self.prev_total.to_le_bytes());
        put(o::LAST_BUYER, self.last_buyer.as_ref());
        put(o::LAST_AMOUNT, &self.last_amount.to_le_bytes());
        put(o::LAST_BUY_AT, &self.last_buy_at.to_le_bytes());
        out
    }

    /// The round a write at `now` goes to: the clock's round, or the header's own if the clock
    /// reads earlier (rounds never go back).
    pub fn round_at(&self, now: i64) -> u32 {
        round_of(now, self.round_secs).max(self.round)
    }

    /// Rolls the header to the clock's round when it is a new one: `(round, total)` moves to
    /// `(prev_round, prev_total)` and the new round starts with no tickets. Answers whether it
    /// rolled. A hook rolls before every write (the `on_*` rules do it), so the header always holds
    /// the latest round with a write and the one before it.
    pub fn roll(&mut self, now: i64) -> bool {
        let round = round_of(now, self.round_secs);
        if round <= self.round {
            return false;
        }
        self.prev_round = self.round;
        self.prev_total = self.total;
        self.round = round;
        self.total = 0;
        true
    }

    /// Round `round`'s ticket total as this header knows it, for a round that has ended (the caller
    /// checks `now >= round_end(round, round_secs)`; until then it may still grow):
    ///
    /// - the header's own round: `total`;
    /// - `prev_round`: `prev_total`;
    /// - a round with no write at all (between `prev_round` and `round`, or after `round`): 0;
    /// - a round before `prev_round`: `None`, the header has forgotten it (two rounds with writes
    ///   came after it), so that round rolls over.
    pub fn total_of(&self, round: u32) -> Option<u64> {
        if round == self.round {
            Some(self.total)
        } else if round > self.round {
            Some(0)
        } else if round == self.prev_round {
            Some(self.prev_total)
        } else if round > self.prev_round {
            Some(0)
        } else {
            None
        }
    }
}

/// A game hook's state address for `mint`: `PDA(["state", mint], hook)` and its bump.
pub fn state_address(hook: &Pubkey, mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[STATE_SEED, mint.as_ref()], hook)
}

/// [`state_address`] at a known bump (no search): `None` when the seeds give no PDA.
pub fn state_address_at(hook: &Pubkey, mint: &Pubkey, bump: u8) -> Option<Pubkey> {
    Pubkey::create_program_address(&[STATE_SEED, mint.as_ref(), &[bump]], hook).ok()
}

fn read_checked(
    info: &AccountInfo,
    hook: &Pubkey,
    mint: &Pubkey,
    expected: Option<Pubkey>,
) -> core::result::Result<GameHeader, GameError> {
    if info.owner != hook {
        return Err(GameError::WrongOwner);
    }
    if expected != Some(*info.key) {
        return Err(GameError::WrongAddress);
    }
    let data = info.try_borrow_data().map_err(|_| GameError::Unreadable)?;
    GameHeader::read(&data[..], mint)
}

/// Reads the header of `hook`'s game for `mint` from its state account: owned by `hook`, at
/// `PDA(["state", mint], hook)` (derived here, a search), starting with [`MAGIC`] and naming `mint`.
/// What the companion calls to read a game's tickets.
pub fn read_state(
    info: &AccountInfo,
    hook: &Pubkey,
    mint: &Pubkey,
) -> core::result::Result<GameHeader, GameError> {
    read_checked(info, hook, mint, Some(state_address(hook, mint).0))
}

/// [`read_state`] with the state's bump known (no search).
pub fn read_state_at(
    info: &AccountInfo,
    hook: &Pubkey,
    mint: &Pubkey,
    bump: u8,
) -> core::result::Result<GameHeader, GameError> {
    read_checked(info, hook, mint, state_address_at(hook, mint, bump))
}

// ------------------------------------------------------------------------------------- slots

/// A holding's tickets in one round: `[start, start + weight)`. No tickets when `weight` is 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Range {
    /// The round.
    pub round: u32,
    /// The first ticket.
    pub start: u64,
    /// How many tickets (tokens).
    pub weight: u64,
}

impl Range {
    /// No tickets, in no round.
    pub const EMPTY: Range = Range {
        round: 0,
        start: 0,
        weight: 0,
    };

    /// No tickets in `round` (a current slot that only marks the round it was written in).
    pub const fn none_in(round: u32) -> Range {
        Range {
            round,
            start: 0,
            weight: 0,
        }
    }

    /// Whether it holds any ticket.
    pub fn is_live(&self) -> bool {
        self.weight > 0
    }

    /// One past its last ticket (`None` on overflow, which a range the standard wrote never has).
    pub fn end(&self) -> Option<u64> {
        self.start.checked_add(self.weight)
    }

    /// Whether ticket `x` is in it (overflow-safe for any bytes).
    pub fn contains(&self, x: u64) -> bool {
        self.weight > 0 && x >= self.start && x - self.start < self.weight
    }

    /// Cuts it to at most `balance` tickets; what is cut is dead for ever. Emptied, it keeps only
    /// its round.
    pub fn shrink_to(&mut self, balance: u64) {
        if self.weight > balance {
            self.weight = balance;
        }
        if self.weight == 0 {
            *self = Self::none_in(self.round);
        }
    }

    fn decode(data: &[u8; HOOK_DATA_LEN], at: usize) -> Self {
        let range = Self {
            round: u32_at(data, at),
            start: u64_at(data, at + 4),
            weight: u64_at(data, at + 12),
        };
        if range.weight == 0 {
            Self::none_in(range.round)
        } else {
            range
        }
    }

    fn encode(&self, out: &mut [u8; HOOK_DATA_LEN], at: usize) {
        let written = if self.weight == 0 {
            Self::none_in(self.round)
        } else {
            *self
        };
        out[at..at + 4].copy_from_slice(&written.round.to_le_bytes());
        out[at + 4..at + 12].copy_from_slice(&written.start.to_le_bytes());
        out[at + 12..at + 20].copy_from_slice(&written.weight.to_le_bytes());
    }
}

/// A holding's 64 bytes of hook data under the standard ([`slot_offsets`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Slots {
    /// The round the holding was last written in, and its range that round (no tickets when it
    /// held nothing through the round so far).
    pub current: Range,
    /// The range of an earlier round, kept so its winner can still claim after trading again.
    /// Without tickets it is all zeros.
    pub previous: Range,
    /// When the holding last sent (or burned) anything, or first received; 0 for none.
    pub since: i64,
    /// Bytes 48..64, the hook's own.
    pub free: [u8; slot_offsets::FREE_LEN],
}

impl Slots {
    /// Reads a holding's hook data. All zeros (a holding the hook never wrote) is no tickets.
    pub fn decode(data: &[u8; HOOK_DATA_LEN]) -> Self {
        let mut free = [0u8; slot_offsets::FREE_LEN];
        free.copy_from_slice(&data[slot_offsets::FREE..]);
        let previous = Range::decode(data, slot_offsets::PREV_ROUND);
        Self {
            current: Range::decode(data, slot_offsets::ROUND),
            previous: if previous.is_live() {
                previous
            } else {
                Range::EMPTY
            },
            since: i64_at(data, slot_offsets::SINCE),
            free,
        }
    }

    /// The hook data to write: a current slot without tickets as its round alone, a previous slot
    /// without tickets as zeros.
    pub fn encode(&self) -> [u8; HOOK_DATA_LEN] {
        let mut out = [0u8; HOOK_DATA_LEN];
        self.current.encode(&mut out, slot_offsets::ROUND);
        if self.previous.is_live() {
            self.previous.encode(&mut out, slot_offsets::PREV_ROUND);
        }
        out[slot_offsets::SINCE..slot_offsets::SINCE + 8]
            .copy_from_slice(&self.since.to_le_bytes());
        out[slot_offsets::FREE..].copy_from_slice(&self.free);
        out
    }

    /// The holding's live range in `round`: the current slot's, else the previous slot's.
    pub fn range_in(&self, round: u32) -> Option<Range> {
        [self.current, self.previous]
            .into_iter()
            .find(|r| r.is_live() && r.round == round)
    }

    /// Moves a live current range of an earlier round than `round` to the previous slot (replacing
    /// what was there), leaving the current slot empty.
    pub fn roll(&mut self, round: u32) {
        if self.current.is_live() && self.current.round < round {
            self.previous = self.current;
            self.current = Range::EMPTY;
        }
    }

    /// Cuts both slots to at most `balance` tickets (a send or a burn left `balance`).
    pub fn shrink(&mut self, balance: u64) {
        self.current.shrink_to(balance);
        self.previous.shrink_to(balance);
        if !self.previous.is_live() {
            self.previous = Range::EMPTY;
        }
    }
}

// ------------------------------------------------------------------------------------- rules

/// Gives the holding a new range of `weight` tickets at the end of the header's round: `start =
/// total`, `total += weight`. Answers whether it did: not for a weight of 0, nor when the total
/// would overflow (the holding then keeps what it had). Under the standard's rules a holding
/// registers at most once a round ([`on_send`], [`on_receive`], [`on_enter`]).
pub fn register(header: &mut GameHeader, slots: &mut Slots, weight: u64) -> bool {
    if weight == 0 {
        return false;
    }
    let Some(end) = header.total.checked_add(weight) else {
        return false;
    };
    slots.current = Range {
        round: header.round,
        start: header.total,
        weight,
    };
    header.total = end;
    true
}

/// Shrinks both of a holding's slots to `balance` ([`Slots::shrink`]).
pub fn shrink(slots: &mut Slots, balance: u64) {
    slots.shrink(balance);
}

/// Whether ticket `x` is in `range` ([`Range::contains`]).
pub fn contains(range: &Range, x: u64) -> bool {
    range.contains(x)
}

/// Whether the holding has been written in `round` already (its current slot is that round's,
/// with or without tickets): it gets no other range that round. Round 0 (before 1970 and a
/// round's length) never counts as written.
pub fn written_in(slots: &Slots, round: u32) -> bool {
    round != 0 && slots.current.round == round
}

/// The holding's first write of the header's round: the slots rolled (a live range of an earlier
/// round kept in the previous slot) and the current slot marked with the round, with a range of
/// `held` tickets: what it has held since the round began, as far as this write keeps it.
fn open_round(header: &mut GameHeader, slots: &mut Slots, held: u64) {
    slots.roll(header.round);
    slots.current = Range::none_in(header.round);
    register(header, slots, held);
}

/// A send (or a burn) from an eligible holding at `now` that left it `balance_left`: the header
/// rolled; at the holding's first write of the round, its range for what it keeps (all it had
/// since the round began, less what leaves now); both slots cut to the balance left; `since =
/// now`. A holding left with nothing is cleared to all zeros (no tickets, so it can be closed).
pub fn on_send(header: &mut GameHeader, slots: &mut Slots, balance_left: u64, now: i64) {
    header.roll(now);
    if balance_left == 0 {
        *slots = Slots::default();
        return;
    }
    if !written_in(slots, header.round) {
        open_round(header, slots, balance_left);
    }
    slots.shrink(balance_left);
    slots.since = now;
}

/// A receive into an eligible holding at `now`, from `balance_before` to `balance_after`: the
/// header rolled; at the holding's first write of the round, its range for what it held before
/// (all of it held since the round began); what arrives counts from the next round. `since` is
/// set if it was not. A receive of nothing changes nothing.
pub fn on_receive(
    header: &mut GameHeader,
    slots: &mut Slots,
    balance_before: u64,
    balance_after: u64,
    now: i64,
) {
    header.roll(now);
    if balance_after == 0 || balance_after == balance_before {
        return;
    }
    if !written_in(slots, header.round) {
        open_round(header, slots, balance_before.min(balance_after));
    }
    if slots.since == 0 {
        slots.since = now;
    }
}

/// `enter` for an eligible holding of `balance` at `now`: the header rolled and, when the holding
/// has not been written this round (so it has held `balance` since the round began), its range for
/// the round. Answers whether the holding's data changed: nothing to write when it was written this
/// round already (so nobody can grief a holder by entering it) or holds nothing.
pub fn on_enter(header: &mut GameHeader, slots: &mut Slots, balance: u64, now: i64) -> bool {
    header.roll(now);
    if balance == 0 || written_in(slots, header.round) {
        return false;
    }
    open_round(header, slots, balance);
    if slots.since == 0 {
        slots.since = now;
    }
    true
}

// ------------------------------------------------------------------------------------- owners

/// The launch of `mint`: `PDA(["launch", mint], launchpad)`.
pub fn launch_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[LAUNCH_SEED, mint.as_ref()], &LAUNCH_PROGRAM_ID).0
}

/// The companion's creator address for `mint`: `PDA(["creator", mint], companion)`.
pub fn companion_creator_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[COMPANION_CREATOR_SEED, mint.as_ref()],
        &COMPANION_PROGRAM_ID,
    )
    .0
}

/// Whether `owner` may hold tickets: none of `excluded` (a game passes the launch, its pool and the
/// companion's creator address), not the default key, and on the ed25519 curve (a wallet: no
/// program-owned address, so no pool, vault or escrow, ever holds tickets).
pub fn eligible(owner: &Pubkey, excluded: &[Pubkey]) -> bool {
    *owner != Pubkey::default() && !excluded.contains(owner) && owner.is_on_curve()
}

// ------------------------------------------------------------------------------------- draws

/// The `k`-th ticket a draw with randomness `randomness` picks in a round of `total` tickets:
/// `u64_le(sha256(randomness ‖ u32_le(k))[..8]) % total`. `None` for a round with no tickets. The
/// modulo favours the lowest `2^64 mod total` tickets by one in `2^64 / total` (relatively, below
/// `total / 2^64`: about 2^-14 for a whole supply of 10^15 base units).
///
/// The draw's seed is not the standard's: the companion commits it on chain from a slot hash
/// nobody knew before (`bordrless_companion::oracle::draw_seed`).
pub fn draw_index(randomness: &[u8], k: u32, total: u64) -> Option<u64> {
    if total == 0 {
        return None;
    }
    let hash = solana_sha256_hasher::hashv(&[randomness, &k.to_le_bytes()]).to_bytes();
    let mut first = [0u8; 8];
    first.copy_from_slice(&hash[..8]);
    Some(u64::from_le_bytes(first) % total)
}

/// Whether a holding with `hook_data` and `balance` holds ticket `x` of `round`: its range for that
/// round (current slot, else previous) contains `x`, and that range's weight is at most the
/// balance. The caller also checks the holding is of the game's mint and its owner is
/// [`eligible`].
pub fn wins(hook_data: &[u8; HOOK_DATA_LEN], round: u32, x: u64, balance: u64) -> bool {
    match Slots::decode(hook_data).range_in(round) {
        Some(range) => range.weight <= balance && range.contains(x),
        None => false,
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_phase2;
