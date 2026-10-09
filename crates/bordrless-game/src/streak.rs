//! The diamond-hands streak (game ticket standard, phase 2): each epoch, the companion shares a part
//! of the pot among the holders who held through the whole epoch without sending a single token,
//! in proportion to what they held.
//!
//! Epochs are the base header's rounds (`round_secs` is the epoch's length; [`crate::round_of`]).
//!
//! # What the hook keeps
//!
//! - **The base header** ([`crate::GameHeader`]): `round` and `total` are the current epoch and
//!   its weight total, `prev_round` and `prev_total` the epoch before. Unlike a lottery's ticket
//!   total, a streak's total is exact: it is the sum of the live weights, a send subtracting the
//!   sender's.
//! - **The streak header** ([`StreakHeader`], right after the base header at offset 120,
//!   [`streak_offsets`]): `min_streak_secs` and `min_weight`, fixed when the hook is made.
//! - **Each holding's slots** ([`Slots`]): the current slot is the epoch the holding was last written
//!   in and its weight that epoch (`start` is 0: there are no ranges); the previous slot keeps the
//!   epoch before, so its share can be claimed during the next epoch; `since` is when the holding
//!   last sent or burned anything, or first received.
//!
//! # The rules ([`streak_on_send`], [`streak_on_receive`], [`streak_on_enter`])
//!
//! - **An epoch's weight is what the holding held since the epoch began**, registered at its first
//!   write that epoch (a receive registers what it held before; `enter` its balance), and only if
//!   it qualifies then ([`streak_qualifies`]): at least `min_weight`, and by the epoch's end it will
//!   have gone `min_streak_secs` without sending. Tokens that arrive during an epoch count from the
//!   next one; moving tokens between wallets adds no weight to anyone.
//! - **Any send forfeits**: a send (or a burn) zeroes the sender's weight for the epoch (and the
//!   total loses it) and its weight for the epoch before (whose total is final: what it forfeits
//!   stays in the pot). So a holder must claim an ended epoch's share before sending anything.
//! - **The test does not depend on when the holding is written**: `since` can't change without a
//!   send, which forfeits, so a holding that qualifies at its first write of an epoch qualifies all
//!   through it. Nobody can lock a holder out by entering it early: `enter` writes only a weight
//!   that registers.
//! - **No weight ever exceeds a balance**: it is set to a balance held since the epoch began, and
//!   a balance only falls with a send, which zeroes it.
//!
//! # Claims ([`streak_weight`], [`share_of`])
//!
//! Once epoch `e` is over, the companion snapshots its pot share and `total_of(e)`; during epoch
//! `e + 1` a holding claims `pot * weight / total` once (a receipt per owner and epoch). A holding
//! remembers two epochs, so in `e + 2` one written in both `e + 1` and `e + 2` has forgotten `e`:
//! the claims of `e` end with `e + 1`.

use anchor_lang::prelude::*;

use crate::{round_end, written_in, GameHeader, Range, Slots, HOOK_DATA_LEN};

/// The first four bytes of a streak hook's [`StreakHeader`]: `"BRS1"`.
pub const STREAK_MAGIC: [u8; 4] = *b"BRS1";
/// Bytes of the streak header: magic 4, min_streak_secs 4, min_weight 8.
pub const STREAK_HEADER_LEN: usize = 16;
/// The longest streak a game may require: a year.
pub const MAX_MIN_STREAK_SECS: u32 = 365 * 86_400;

/// Where each streak header field sits in a streak hook's state account (absolute offsets, right
/// after the base header). Integers are little-endian.
pub mod streak_offsets {
    /// `magic: [u8; 4]`, [`super::STREAK_MAGIC`].
    pub const MAGIC: usize = 120;
    /// `min_streak_secs: u32`.
    pub const MIN_STREAK_SECS: usize = 124;
    /// `min_weight: u64`.
    pub const MIN_WEIGHT: usize = 128;
    /// The first byte after the streak header.
    pub const END: usize = 136;
}

/// The streak header, right after the base header in a streak hook's state. A hook declares it as
/// the second field of its state account (the base header first).
#[derive(
    AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, Debug, Default, PartialEq, Eq,
)]
pub struct StreakHeader {
    /// [`STREAK_MAGIC`].
    pub magic: [u8; 4],
    /// A holding qualifies for an epoch only if, by the epoch's end, it will have sent nothing for
    /// this long.
    pub min_streak_secs: u32,
    /// The least weight that registers (smaller holdings get no share, and no part of the total).
    pub min_weight: u64,
}

impl StreakHeader {
    /// A streak's header.
    pub fn new(min_streak_secs: u32, min_weight: u64) -> Self {
        Self {
            magic: STREAK_MAGIC,
            min_streak_secs,
            min_weight,
        }
    }

    /// Parses it from a state account's data (the discriminator and the base header first),
    /// checking the length and [`STREAK_MAGIC`]. `None` otherwise; never panics.
    pub fn parse(data: &[u8]) -> Option<Self> {
        use streak_offsets as o;
        if data.len() < o::END || data[o::MAGIC..o::MAGIC + 4] != STREAK_MAGIC {
            return None;
        }
        let mut streak = [0u8; 4];
        streak.copy_from_slice(&data[o::MIN_STREAK_SECS..o::MIN_STREAK_SECS + 4]);
        let mut weight = [0u8; 8];
        weight.copy_from_slice(&data[o::MIN_WEIGHT..o::MIN_WEIGHT + 8]);
        Some(Self {
            magic: STREAK_MAGIC,
            min_streak_secs: u32::from_le_bytes(streak),
            min_weight: u64::from_le_bytes(weight),
        })
    }

    /// The header's bytes as they sit at [`streak_offsets::MAGIC`] (what Borsh writes for it).
    pub fn encode(&self) -> [u8; STREAK_HEADER_LEN] {
        let mut out = [0u8; STREAK_HEADER_LEN];
        out[..4].copy_from_slice(&self.magic);
        out[4..8].copy_from_slice(&self.min_streak_secs.to_le_bytes());
        out[8..].copy_from_slice(&self.min_weight.to_le_bytes());
        out
    }

    /// The least weight that registers: `min_weight`, and at least 1.
    pub fn floor(&self) -> u64 {
        self.min_weight.max(1)
    }
}

/// Whether a holding that last sent (or first received) at `since` qualifies for `epoch` (of
/// `epoch_secs`): by the epoch's end it will have sent nothing for `min_streak_secs`. A `since` of
/// 0 (a holding the hook never saw receive) never qualifies.
pub fn streak_qualifies(since: i64, epoch: u32, epoch_secs: u32, min_streak_secs: u32) -> bool {
    since > 0 && since <= round_end(epoch, epoch_secs).saturating_sub(i64::from(min_streak_secs))
}

/// What a holding that held `held` since the header's epoch began registers for it: `held` if that
/// is at least the floor and the holding qualifies ([`streak_qualifies`], on its `since` before
/// this write), else 0.
pub fn streak_weight_for(header: &GameHeader, streak: &StreakHeader, since: i64, held: u64) -> u64 {
    if held >= streak.floor()
        && streak_qualifies(
            since,
            header.round,
            header.round_secs,
            streak.min_streak_secs,
        )
    {
        held
    } else {
        0
    }
}

/// The holding's first write of the header's epoch: the slots rolled (a live weight of an earlier
/// epoch kept in the previous slot) and the current slot marked with the epoch, with the weight
/// `held` registers ([`streak_weight_for`]) added to the total (none if the total would overflow).
fn open_epoch(header: &mut GameHeader, streak: &StreakHeader, slots: &mut Slots, held: u64) {
    slots.roll(header.round);
    let mut weight = streak_weight_for(header, streak, slots.since, held);
    match header.total.checked_add(weight) {
        Some(total) => header.total = total,
        None => weight = 0,
    }
    slots.current = Range {
        round: header.round,
        start: 0,
        weight,
    };
}

/// A send (or a burn) from an eligible holding at `now` that left it `balance_left`: the header
/// rolled; the holding's weight for this epoch zeroed (the total loses it) and its weight for the
/// epoch before forfeited (that total stays: what it forfeits stays in the pot); `since = now`. A
/// holding left with nothing is cleared to zeros (so it can be closed).
pub fn streak_on_send(header: &mut GameHeader, slots: &mut Slots, balance_left: u64, now: i64) {
    header.roll(now);
    if written_in(slots, header.round) {
        if slots.current.is_live() {
            header.total = header.total.saturating_sub(slots.current.weight);
        }
    } else {
        slots.roll(header.round);
    }
    slots.current = Range::none_in(header.round);
    slots.previous = Range::EMPTY;
    slots.since = now;
    if balance_left == 0 {
        *slots = Slots::default();
    }
}

/// A receive into an eligible holding at `now`, from `balance_before` to `balance_after`: the
/// header rolled; at the holding's first write of the epoch, its weight for what it held before (all
/// of it held since the epoch began); what arrives counts from the next epoch. `since` is set if it
/// was not. A receive of nothing changes nothing.
pub fn streak_on_receive(
    header: &mut GameHeader,
    streak: &StreakHeader,
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
        open_epoch(header, streak, slots, balance_before.min(balance_after));
    }
    if slots.since == 0 {
        slots.since = now;
    }
}

/// `enter` for an eligible holding of `balance` at `now`: the header rolled and, when the holding
/// has not been written this epoch (so it has held `balance` since the epoch began) and that weight
/// registers ([`streak_weight_for`]), its weight for the epoch. Answers whether the holding's data
/// changed: nothing is written for a holding written this epoch already, nor for one whose weight
/// would not register (so nobody can lock a holder out of an epoch by entering it).
pub fn streak_on_enter(
    header: &mut GameHeader,
    streak: &StreakHeader,
    slots: &mut Slots,
    balance: u64,
    now: i64,
) -> bool {
    header.roll(now);
    if balance == 0 || written_in(slots, header.round) {
        return false;
    }
    if streak_weight_for(header, streak, slots.since, balance) == 0 {
        return false;
    }
    open_epoch(header, streak, slots, balance);
    slots.current.is_live()
}

/// The weight a holding with `hook_data` and `balance` claims for `epoch` (of `epoch_secs`), under
/// a game with `min_streak_secs` and `min_weight`: its live weight for the epoch (current slot, else
/// previous), when that weight is at least the floor and at most its balance, and the holding still
/// qualifies (its `since`, which a send would have moved, and zeroed the weight with). 0 otherwise.
/// The caller also checks the holding is of the game's mint and its owner is [`crate::eligible`].
pub fn streak_weight(
    hook_data: &[u8; HOOK_DATA_LEN],
    epoch: u32,
    epoch_secs: u32,
    min_streak_secs: u32,
    min_weight: u64,
    balance: u64,
) -> u64 {
    let slots = Slots::decode(hook_data);
    match slots.range_in(epoch) {
        Some(r)
            if r.weight >= min_weight.max(1)
                && r.weight <= balance
                && streak_qualifies(slots.since, epoch, epoch_secs, min_streak_secs) =>
        {
            r.weight
        }
        _ => 0,
    }
}

/// A holding's share of an epoch's pot: `pot * weight / total`, rounded down (the dust stays in the
/// pot), never more than the pot; 0 for a total of 0.
pub fn share_of(pot: u64, weight: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    let share = u128::from(pot) * u128::from(weight.min(total)) / u128::from(total);
    share as u64
}
