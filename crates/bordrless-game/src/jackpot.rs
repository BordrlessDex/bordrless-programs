//! The last-buyer jackpot (game ticket standard, phase 2): every qualifying buy restarts a timer;
//! once it runs out, the companion pays the last qualifying buyer a share of the pot, if they still
//! hold what they bought.
//!
//! # What the hook keeps
//!
//! - **The base header's jackpot fields** ([`crate::GameHeader`]: `last_buyer`, `last_amount`,
//!   `last_buy_at`): the current round's last qualifying buy. A jackpot hook's header has no
//!   rounds: `round_secs` is 0.
//! - **The jackpot header** ([`JackpotHeader`], right after the base header at offset 120,
//!   [`jackpot_offsets`]): the timer and the minimum buy the hook was made with, the count of
//!   qualifying buys (`buys`: the current round's number), and the last [`JACKPOT_ENDED_ROUNDS`]
//!   rounds that ended (each its buyer, amount, time and number; the newest in the `ended_*`
//!   fields, the older ones in `earlier`, newest first). A round ends when the timer runs out; the
//!   next qualifying buy moves it to the `ended_*` fields (the older ones shifting down `earlier`,
//!   the oldest dropped), so a buy that lands after the timer ran out, but before anyone settled,
//!   never takes the round's winner's place, and an ended round stays payable for at least
//!   [`JACKPOT_ENDED_ROUNDS`] timers after it ended (8 more rounds must end, each a whole timer
//!   without a qualifying buy, before it is dropped).
//! - **Each holding's mark** (the first 8 of the slot's free bytes, [`jackpot_mark`]): the number of
//!   its first qualifying buy since it last sent anything; zero once it sends or burns. So a holding
//!   that holds a mark `m` with `0 < m <= n` has held everything it had at buy `m` ever since, and
//!   in particular what it bought in round `n` if it was that round's buyer.
//!
//! # What counts as a qualifying buy ([`qualifying_buy`])
//!
//! A transfer out of the launch's own pool, while the launch is on its bonding curve, of at least
//! `min_tokens`, to an owner who may hold tickets ([`crate::eligible`]: a wallet, not the launch,
//! its pool or the companion's creator address). Not a buy through any other pool (anyone can open
//! one), not the companion's own buybacks (to its creator address), and nothing once the launch has
//! graduated: from then on liquidity can be added and taken out again, and a removal is a transfer
//! out of the pool that a token hook can't tell from a buy ([`crate::launch`]).
//!
//! # Settling ([`settle_round`], [`jackpot_winner_holds`])
//!
//! The companion pays one round at a time, the oldest open first: the oldest remembered ended
//! round later than the last one settled, else the current round once its timer has run out. A round's
//! buyer wins if their holding still holds a mark `m` with `0 < m <= n` (no send since the buy) and
//! at least the amount they bought; otherwise the round is forfeited and the pot stays.

use anchor_lang::prelude::*;

use crate::{eligible, GameHeader, LaunchView, Slots, HOOK_DATA_LEN};

/// The first four bytes of a jackpot hook's [`JackpotHeader`]: `"BRJ1"`.
pub const JACKPOT_MAGIC: [u8; 4] = *b"BRJ1";
/// How many ended rounds a jackpot hook remembers: the newest in the header's `ended_*` fields, the
/// [`JACKPOT_ENDED_ROUNDS`]` - 1` before it in `earlier`. An ended round is dropped only once 8 later
/// rounds have ended, each a whole timer without a qualifying buy: it stays payable for at least 8
/// timers after it ended (40 minutes at the 5-minute floor), whatever a keeper's latency. 56 bytes
/// a round (a header of 472 bytes, a state under 1 KiB); `settle` reads it in place and walks at
/// most 9 rounds, and a hook shifts 7 rounds only on the buy that ends one.
pub const JACKPOT_ENDED_ROUNDS: usize = 8;
/// The ended rounds in [`JackpotHeader::earlier`]: all but the newest.
pub const JACKPOT_EARLIER_ROUNDS: usize = JACKPOT_ENDED_ROUNDS - 1;
/// Bytes of one remembered ended round: buyer 32, amount 8, at 8, number 8.
pub const ENDED_ROUND_LEN: usize = 56;
/// Bytes of the jackpot header: magic 4, timer_secs 4, min_tokens 8, buys 8, ended_buyer 32,
/// ended_amount 8, ended_at 8, ended_buys 8 (80), then `earlier`: 7 ended rounds of 56.
pub const JACKPOT_HEADER_LEN: usize = 80 + JACKPOT_EARLIER_ROUNDS * ENDED_ROUND_LEN;
/// The shortest timer: five minutes (block stuffing the last seconds of a shorter one is cheap).
pub const MIN_TIMER_SECS: u32 = 300;
/// The longest timer: 30 days.
pub const MAX_TIMER_SECS: u32 = 30 * 86_400;

/// Where each jackpot header field sits in a jackpot hook's state account (absolute offsets, right
/// after the base header). Integers are little-endian.
pub mod jackpot_offsets {
    /// `magic: [u8; 4]`, [`super::JACKPOT_MAGIC`].
    pub const MAGIC: usize = 120;
    /// `timer_secs: u32`.
    pub const TIMER_SECS: usize = 124;
    /// `min_tokens: u64`: the least a qualifying buy delivers.
    pub const MIN_TOKENS: usize = 128;
    /// `buys: u64`: qualifying buys so far, the current round's number.
    pub const BUYS: usize = 136;
    /// `ended_buyer: Pubkey`: the buyer of the round that ended last.
    pub const ENDED_BUYER: usize = 144;
    /// `ended_amount: u64`.
    pub const ENDED_AMOUNT: usize = 176;
    /// `ended_at: i64`: when its last buy was made.
    pub const ENDED_AT: usize = 184;
    /// `ended_buys: u64`: its number (0: none).
    pub const ENDED_BUYS: usize = 192;
    /// `earlier: [EndedRound; 7]`: the ended rounds before it, newest first, each 56 bytes
    /// (`buyer` +0, `amount` +32, `at` +40, `number` +48; number 0: none).
    pub const EARLIER: usize = 200;
    /// The first byte after the jackpot header.
    pub const END: usize = EARLIER + super::JACKPOT_EARLIER_ROUNDS * super::ENDED_ROUND_LEN;
}

/// Where a jackpot hook keeps a holding's mark: the first 8 of the slot's free bytes.
pub const MARK_AT: usize = crate::slot_offsets::FREE;

/// A round that ended, as a jackpot hook remembers it ([`JackpotHeader::earlier`]).
#[derive(
    AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, Debug, Default, PartialEq, Eq,
)]
pub struct EndedRound {
    /// Its last qualifying buyer (the default key: none).
    pub buyer: Pubkey,
    /// What they bought.
    pub amount: u64,
    /// When (its last qualifying buy).
    pub at: i64,
    /// Its number (0: none).
    pub number: u64,
}

/// The jackpot header, right after the base header in a jackpot hook's state. A hook declares it
/// as the second field of its state account (the base header first).
#[derive(
    AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, Debug, Default, PartialEq, Eq,
)]
pub struct JackpotHeader {
    /// [`JACKPOT_MAGIC`].
    pub magic: [u8; 4],
    /// A round ends this long after its last qualifying buy.
    pub timer_secs: u32,
    /// The least a qualifying buy delivers, in the token's base units.
    pub min_tokens: u64,
    /// Qualifying buys so far: the current round's number (0: none yet).
    pub buys: u64,
    /// The round that ended last: its buyer (the default key: none).
    pub ended_buyer: Pubkey,
    /// What they bought.
    pub ended_amount: u64,
    /// When (its last qualifying buy).
    pub ended_at: i64,
    /// Its number (0: none).
    pub ended_buys: u64,
    /// The ended rounds before it, newest first ([`JACKPOT_ENDED_ROUNDS`]` - 1`; number 0: none).
    pub earlier: [EndedRound; JACKPOT_EARLIER_ROUNDS],
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

impl JackpotHeader {
    /// A fresh jackpot: no buy yet.
    pub fn new(timer_secs: u32, min_tokens: u64) -> Self {
        Self {
            magic: JACKPOT_MAGIC,
            timer_secs,
            min_tokens,
            ..Self::default()
        }
    }

    /// Parses it from a state account's data (the discriminator and the base header first),
    /// checking the length and [`JACKPOT_MAGIC`]. `None` otherwise; never panics.
    pub fn parse(data: &[u8]) -> Option<Self> {
        use jackpot_offsets as o;
        if data.len() < o::END || data[o::MAGIC..o::MAGIC + 4] != JACKPOT_MAGIC {
            return None;
        }
        let mut earlier = [EndedRound::default(); JACKPOT_EARLIER_ROUNDS];
        for (i, r) in earlier.iter_mut().enumerate() {
            *r = ended_round_at(data, o::EARLIER + i * ENDED_ROUND_LEN);
        }
        let last = ended_round_at(data, o::ENDED_BUYER);
        Some(Self {
            magic: JACKPOT_MAGIC,
            timer_secs: u32_at(data, o::TIMER_SECS),
            min_tokens: u64_at(data, o::MIN_TOKENS),
            buys: u64_at(data, o::BUYS),
            ended_buyer: last.buyer,
            ended_amount: last.amount,
            ended_at: last.at,
            ended_buys: last.number,
            earlier,
        })
    }

    /// The `i`-th newest ended round it remembers (`0`: the `ended_*` fields; up to
    /// [`JACKPOT_ENDED_ROUNDS`]` - 1`); none past the end.
    pub fn ended(&self, i: usize) -> Option<EndedRound> {
        if i == 0 {
            return Some(EndedRound {
                buyer: self.ended_buyer,
                amount: self.ended_amount,
                at: self.ended_at,
                number: self.ended_buys,
            });
        }
        self.earlier.get(i - 1).copied()
    }

    /// Remembers `r` as the newest ended round: the others shift one older, the oldest is dropped.
    pub fn push_ended(&mut self, r: EndedRound) {
        self.earlier.copy_within(..JACKPOT_EARLIER_ROUNDS - 1, 1);
        self.earlier[0] = EndedRound {
            buyer: self.ended_buyer,
            amount: self.ended_amount,
            at: self.ended_at,
            number: self.ended_buys,
        };
        self.ended_buyer = r.buyer;
        self.ended_amount = r.amount;
        self.ended_at = r.at;
        self.ended_buys = r.number;
    }

    /// The header's bytes as they sit at [`jackpot_offsets::MAGIC`] (what Borsh writes for it).
    pub fn encode(&self) -> [u8; JACKPOT_HEADER_LEN] {
        let mut out = [0u8; JACKPOT_HEADER_LEN];
        let base = jackpot_offsets::MAGIC;
        let mut put = |at: usize, bytes: &[u8]| {
            out[at - base..at - base + bytes.len()].copy_from_slice(bytes);
        };
        use jackpot_offsets as o;
        put(o::MAGIC, &self.magic);
        put(o::TIMER_SECS, &self.timer_secs.to_le_bytes());
        put(o::MIN_TOKENS, &self.min_tokens.to_le_bytes());
        put(o::BUYS, &self.buys.to_le_bytes());
        put(o::ENDED_BUYER, self.ended_buyer.as_ref());
        put(o::ENDED_AMOUNT, &self.ended_amount.to_le_bytes());
        put(o::ENDED_AT, &self.ended_at.to_le_bytes());
        put(o::ENDED_BUYS, &self.ended_buys.to_le_bytes());
        for (i, r) in self.earlier.iter().enumerate() {
            let at = o::EARLIER + i * ENDED_ROUND_LEN;
            put(at, r.buyer.as_ref());
            put(at + 32, &r.amount.to_le_bytes());
            put(at + 40, &r.at.to_le_bytes());
            put(at + 48, &r.number.to_le_bytes());
        }
        out
    }
}

/// The ended round whose 56 bytes start at `at` (buyer, amount, at, number).
fn ended_round_at(data: &[u8], at: usize) -> EndedRound {
    let mut buyer = [0u8; 32];
    buyer.copy_from_slice(&data[at..at + 32]);
    EndedRound {
        buyer: Pubkey::new_from_array(buyer),
        amount: u64_at(data, at + 32),
        at: u64_at(data, at + 40) as i64,
        number: u64_at(data, at + 48),
    }
}

/// Whether `timer_secs` is within [`MIN_TIMER_SECS`] and [`MAX_TIMER_SECS`].
pub fn valid_timer_secs(timer_secs: u32) -> bool {
    (MIN_TIMER_SECS..=MAX_TIMER_SECS).contains(&timer_secs)
}

/// Whether a round whose last qualifying buy was at `at` is over at `now`: its timer ran out.
pub fn timer_over(at: i64, timer_secs: u32, now: i64) -> bool {
    now >= at.saturating_add(i64::from(timer_secs))
}

/// A holding's mark ([`MARK_AT`]): the number of its first qualifying buy since it last sent; 0
/// for none.
pub fn jackpot_mark(slots: &Slots) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&slots.free[..8]);
    u64::from_le_bytes(b)
}

/// Sets a holding's mark.
pub fn set_jackpot_mark(slots: &mut Slots, mark: u64) {
    slots.free[..8].copy_from_slice(&mark.to_le_bytes());
}

/// Whether a transfer is a qualifying buy: out of the launch's own pool, while the launch is on its
/// bonding curve, of at least `min_tokens` (and at least 1), to an owner who may hold tickets (none
/// of `excluded`, a wallet). `launch` is `None` when the launch can't be read (during its own
/// transaction): nothing qualifies then.
pub fn qualifying_buy(
    launch: Option<&LaunchView>,
    source_owner: &Pubkey,
    destination_owner: &Pubkey,
    amount: u64,
    min_tokens: u64,
    excluded: &[Pubkey],
) -> bool {
    let Some(launch) = launch else {
        return false;
    };
    launch.on_curve
        && launch.pool != Pubkey::default()
        && *source_owner == launch.pool
        && amount >= min_tokens.max(1)
        && *destination_owner != launch.pool
        && eligible(destination_owner, excluded)
}

/// A send (or a burn) from a holding at `now` that left it `balance_left`: its mark cleared (it no
/// longer holds everything it bought), `since = now`; a holding left with nothing is cleared to
/// zeros (so it can be closed).
pub fn jackpot_on_send(slots: &mut Slots, balance_left: u64, now: i64) {
    if balance_left == 0 {
        *slots = Slots::default();
        return;
    }
    set_jackpot_mark(slots, 0);
    slots.since = now;
}

/// A receive into an eligible holding at `now` (any receive, a qualifying buy included): `since`
/// is set if it was not.
pub fn jackpot_on_receive(slots: &mut Slots, balance_after: u64, now: i64) {
    if balance_after > 0 && slots.since == 0 {
        slots.since = now;
    }
}

/// A qualifying buy of `amount` by `buyer` at `now` (after [`jackpot_on_receive`] on the buyer's
/// slots): if the current round's timer had run out, the round is remembered as the newest ended
/// one ([`JackpotHeader::push_ended`]: the `ended_*` fields, the older ones shifting); the
/// buy becomes the current round's (`buys` counts it, the base header's `last_*` name it); and the
/// buyer's mark is set to this buy's number unless it holds one already (an earlier buy it has
/// held through). Answers the buy's number; `None` (nothing changed) only at the counter's end.
pub fn jackpot_on_buy(
    header: &mut GameHeader,
    jackpot: &mut JackpotHeader,
    slots: &mut Slots,
    buyer: &Pubkey,
    amount: u64,
    now: i64,
) -> Option<u64> {
    let n = jackpot.buys.checked_add(1)?;
    if jackpot.buys > 0 && timer_over(header.last_buy_at, jackpot.timer_secs, now) {
        jackpot.push_ended(EndedRound {
            buyer: header.last_buyer,
            amount: header.last_amount,
            at: header.last_buy_at,
            number: jackpot.buys,
        });
    }
    jackpot.buys = n;
    header.last_buyer = *buyer;
    header.last_amount = amount;
    header.last_buy_at = now;
    if jackpot_mark(slots) == 0 {
        set_jackpot_mark(slots, n);
    }
    Some(n)
}

/// A round the companion may settle: its number, its last qualifying buyer, what they bought and
/// when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JackpotRound {
    /// The round's number: the count of qualifying buys at its last one.
    pub number: u64,
    /// Its last qualifying buyer, the owner it pays.
    pub buyer: Pubkey,
    /// What they bought, which their holding must still hold.
    pub amount: u64,
    /// When.
    pub at: i64,
}

/// How many of the remembered ended rounds, newest first, a hook under the standard can have left:
/// each numbered (not 0) below the round after it (the newer ended round, or the current one) and
/// over by the timer before that round's last buy. The rounds past the first that is not are
/// ignored (none is, from a hook that keeps the standard).
fn ended_chain(header: &GameHeader, jackpot: &JackpotHeader, timer_secs: u32) -> usize {
    let (mut next_number, mut next_at) = (jackpot.buys, header.last_buy_at);
    let mut n = 0;
    while let Some(r) = jackpot.ended(n) {
        if r.number == 0 || r.number >= next_number || !timer_over(r.at, timer_secs, next_at) {
            break;
        }
        (next_number, next_at) = (r.number, r.at);
        n += 1;
    }
    n
}

/// The oldest round that is over and not settled (`paid`: the last round the companion settled,
/// paid or forfeited), at `now`, with the game's `timer_secs`:
///
/// - the oldest remembered ended round later than `paid` (each earlier than the round after it,
///   and over by the timer before that round's last buy, as a hook under the standard always
///   leaves them);
/// - else the current round, when later than `paid` and its timer has run out at `now`;
/// - else none.
pub fn settle_round(
    header: &GameHeader,
    jackpot: &JackpotHeader,
    paid: u64,
    timer_secs: u32,
    now: i64,
) -> Option<JackpotRound> {
    for i in (0..ended_chain(header, jackpot, timer_secs)).rev() {
        let Some(r) = jackpot.ended(i) else {
            continue;
        };
        if r.number > paid {
            return Some(JackpotRound {
                number: r.number,
                buyer: r.buyer,
                amount: r.amount,
                at: r.at,
            });
        }
    }
    if jackpot.buys > paid && timer_over(header.last_buy_at, timer_secs, now) {
        return Some(JackpotRound {
            number: jackpot.buys,
            buyer: header.last_buyer,
            amount: header.last_amount,
            at: header.last_buy_at,
        });
    }
    None
}

/// Whether a holding with `hook_data` and `balance` still holds what round `round` bought: its
/// mark is that round's buy or earlier (`0 < mark <= round.number`: it has sent nothing since) and
/// its balance is at least the round's amount. The caller also checks the holding is the round's
/// buyer's, of the game's mint, and that its owner is [`crate::eligible`].
pub fn jackpot_winner_holds(
    hook_data: &[u8; HOOK_DATA_LEN],
    round: &JackpotRound,
    balance: u64,
) -> bool {
    let mark = jackpot_mark(&Slots::decode(hook_data));
    mark != 0 && mark <= round.number && balance >= round.amount && round.amount > 0
}
