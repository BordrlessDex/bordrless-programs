//! The reward accounting (`docs/hooks-v2.md` §4.7 and §4.8, with the review fixes), as the program
//! runs it: the shared stream (the running stream's linear release, the shares made while it runs
//! waiting to stream over the hour after it, the whole paused while nobody is eligible), the sync
//! that divides what arrived among the eligible supply with an exact scaled remainder, and the
//! settle of one holder. Integer only, every operation checked; rounding always favours the vault.

use anchor_lang::prelude::*;

use crate::constants::{SCALE, SHARE_STREAM_SECS};
use crate::error::KitError;
use crate::state::{HolderData, KitConfig};

/// What a stream holding `remaining` lamports, last released at `last` and ending at `end`,
/// releases at `now`: everything at or after the end, else the elapsed share of what remains,
/// rounded down (nothing for a clock that has not moved past `last`).
pub fn stream_release(remaining: u64, last: i64, end: i64, now: i64) -> Option<u64> {
    if remaining == 0 {
        return Some(0);
    }
    if now >= end {
        return Some(remaining);
    }
    if now <= last {
        return Some(0);
    }
    let elapsed = u128::try_from(now.checked_sub(last)?).ok()?;
    let span = u128::try_from(end.checked_sub(last)?).ok()?;
    let released = u128::from(remaining).checked_mul(elapsed)? / span;
    u64::try_from(released).ok()
}

impl KitConfig {
    /// Whether holders hold enough for a sync to divide among them: `eligible > 0` and
    /// `eligible >= min_eligible`.
    pub fn divides(&self) -> bool {
        self.eligible > 0 && self.eligible >= self.min_eligible
    }

    /// Releases the shared stream up to `now` and answers what it released.
    ///
    /// The running stream (`stream_remaining` over `[stream_last, stream_end]`) releases linearly
    /// ([`stream_release`]). Once it is out (`now >= stream_end`), the shares waiting behind it
    /// (`stream_next`) become the running stream over the hour after its end, `[stream_end,
    /// stream_end + SHARE_STREAM_SECS]`, and the part of that hour already past is released with
    /// it: holders held through it. So every share streams linearly over exactly one hour of
    /// eligible holding, at its own rate: a later share never moves an earlier one's end, and
    /// never inherits its rate.
    ///
    /// While holders hold less than `min_eligible` the stream is paused: it releases nothing, and
    /// its end (and the waiting shares' hour after it) moves on by the time that passed, so it
    /// resumes at the same rate with the same time left. What the stream releases is therefore
    /// always divided at once among the holders of the moment: a share reaches only eligible
    /// holders, in proportion to how long they hold, and nothing of it waits in `held` for someone
    /// to buy, claim and sell in one transaction. (`eligible` changes only in callbacks, which
    /// sync first, so it was the same over the whole time since the last sync.)
    pub fn release(&mut self, now: i64) -> Result<u64> {
        if self.stream_remaining == 0 && self.stream_next == 0 {
            return Ok(0);
        }
        if !self.divides() {
            if now > self.stream_last {
                let paused = now
                    .checked_sub(self.stream_last)
                    .ok_or(KitError::MathOverflow)?;
                self.stream_end = self
                    .stream_end
                    .checked_add(paused)
                    .ok_or(KitError::MathOverflow)?;
                self.stream_last = now;
            }
            return Ok(0);
        }
        let mut released = stream_release(
            self.stream_remaining,
            self.stream_last,
            self.stream_end,
            now,
        )
        .ok_or(KitError::MathOverflow)?;
        self.stream_remaining = self
            .stream_remaining
            .checked_sub(released)
            .ok_or(KitError::MathOverflow)?;
        self.stream_last = self.stream_last.max(now);
        if self.stream_remaining == 0 && self.stream_next > 0 && now >= self.stream_end {
            // The waiting shares' hour began when the running stream ended.
            let start = self.stream_end;
            let end = start
                .checked_add(SHARE_STREAM_SECS)
                .ok_or(KitError::MathOverflow)?;
            let part =
                stream_release(self.stream_next, start, end, now).ok_or(KitError::MathOverflow)?;
            self.stream_remaining = self
                .stream_next
                .checked_sub(part)
                .ok_or(KitError::MathOverflow)?;
            self.stream_next = 0;
            self.stream_end = end;
            released = released.checked_add(part).ok_or(KitError::MathOverflow)?;
        }
        Ok(released)
    }

    /// Adds a share of `received` lamports, made at `now` right after the sync at `now`, to the
    /// stream (§4.10): counted as seen, so no sync takes it for fresh, and streamed linearly over
    /// one hour of eligible holding at its own rate, `received / SHARE_STREAM_SECS` a second:
    ///
    /// - nothing streams or waits: over `[now, now + SHARE_STREAM_SECS]`;
    /// - the running stream's whole hour is still ahead (it started this second): it joins it,
    ///   the same hour;
    /// - otherwise it waits for the running stream and streams over the hour after it, with the
    ///   shares already waiting there (`stream_next`): the same hour as theirs.
    ///
    /// So a later share never moves the running stream's end (nothing is stretched) and never
    /// takes on its rate (nothing is sped up), and every share starts streaming within the hour
    /// (of eligible holding). Refused (`MathOverflow`, a bug) when the stream was not synced at
    /// `now`: a share waiting behind a stream already over would be released at once.
    pub fn add_share(&mut self, received: u64, now: i64) -> Result<()> {
        let now = now.max(self.stream_last);
        if self.stream_remaining == 0 && self.stream_next == 0 {
            self.stream_remaining = received;
            self.stream_last = now;
            self.stream_end = now
                .checked_add(SHARE_STREAM_SECS)
                .ok_or(KitError::MathOverflow)?;
        } else {
            // Synced at `now`: what still streams is spread over `[now, stream_end]`, after `now`.
            let left = self
                .stream_end
                .checked_sub(now)
                .ok_or(KitError::MathOverflow)?;
            require!(self.stream_last == now && left > 0, KitError::MathOverflow);
            if self.stream_remaining > 0 && left == SHARE_STREAM_SECS {
                self.stream_remaining = self
                    .stream_remaining
                    .checked_add(received)
                    .ok_or(KitError::MathOverflow)?;
            } else {
                self.stream_next = self
                    .stream_next
                    .checked_add(received)
                    .ok_or(KitError::MathOverflow)?;
            }
        }
        self.seen = self
            .seen
            .checked_add(received)
            .ok_or(KitError::MathOverflow)?;
        self.total_shared = self
            .total_shared
            .checked_add(received)
            .ok_or(KitError::MathOverflow)?;
        Ok(())
    }

    /// The sync (§4.7) with the reward vault holding `vault_amount` at `now`. What arrived since
    /// the last sync (`vault + total_claimed - seen`) and what the stream released are divided
    /// among the eligible supply, with whatever was held, when `eligible > 0` and
    /// `eligible >= min_eligible`; otherwise what arrived is held and the stream pauses
    /// ([`KitConfig::release`]). `eligible` is the value before the operation that syncs.
    pub fn sync(&mut self, vault_amount: u64, now: i64) -> Result<()> {
        let released = self.release(now)?;
        let total = vault_amount
            .checked_add(self.total_claimed)
            .ok_or(KitError::MathOverflow)?;
        // A shortfall is a bug: only `claim` moves lamports out, and it counts them as claimed.
        let fresh = total
            .checked_sub(self.seen)
            .and_then(|arrived| arrived.checked_add(released))
            .ok_or(KitError::MathOverflow)?;
        self.seen = total;
        if self.divides() {
            let pot = fresh.checked_add(self.held).ok_or(KitError::MathOverflow)?;
            self.held = 0;
            if pot > 0 {
                let eligible = u128::from(self.eligible);
                let scaled = u128::from(pot)
                    .checked_mul(SCALE)
                    .and_then(|s| s.checked_add(self.rem))
                    .ok_or(KitError::MathOverflow)?;
                let inc = scaled / eligible;
                self.rem = scaled - inc * eligible;
                self.acc_per_share = self
                    .acc_per_share
                    .checked_add(inc)
                    .ok_or(KitError::MathOverflow)?;
                self.total_distributed = self
                    .total_distributed
                    .checked_add(pot)
                    .ok_or(KitError::MathOverflow)?;
            }
        } else {
            self.held = self.held.checked_add(fresh).ok_or(KitError::MathOverflow)?;
        }
        Ok(())
    }
}

/// The settle of one holder (§4.8) at its balance before the operation, `balance`:
/// `owed += balance * (acc_per_share - snapshot) / SCALE` (rounded down), then
/// `snapshot = acc_per_share`, or 0 when the balance after the operation, `balance_after`, is 0.
pub fn settle(
    data: &mut HolderData,
    balance: u64,
    balance_after: u64,
    acc_per_share: u128,
) -> Result<()> {
    let growth = acc_per_share
        .checked_sub(data.snapshot)
        .ok_or(KitError::MathOverflow)?;
    let earned = u128::from(balance)
        .checked_mul(growth)
        .ok_or(KitError::MathOverflow)?
        / SCALE;
    let earned = u64::try_from(earned).map_err(|_| KitError::MathOverflow)?;
    data.owed = data
        .owed
        .checked_add(earned)
        .ok_or(KitError::MathOverflow)?;
    data.snapshot = if balance_after == 0 { 0 } else { acc_per_share };
    Ok(())
}
