//! The off-chain mirror of the reward accounting (`docs/hooks-v2.md` §4.13): what a holder could
//! claim at `now`, as a pure function of the account state (the `KitConfig`, the reward vault's
//! balance, the holder's balance and hook data). Written from the specification's formulas apart
//! from the program's own sync, so the tests can hold one against the other; the TypeScript
//! mirror in `packages/shared` follows this one.

use anchor_lang::prelude::Pubkey;

use crate::constants::{SCALE, SHARE_STREAM_SECS};
use crate::state::{HolderData, KitConfig};

/// The state a sync at `now` would leave, without writing anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Synced {
    /// What the shared stream releases at `now`.
    pub released: u64,
    /// What arrived since the last sync, plus `released`.
    pub pending: u64,
    /// Whether this sync divides (`eligible > 0`, `eligible >= min_eligible`, something to
    /// divide).
    pub divides: bool,
    /// `acc_per_share` after the sync.
    pub acc_per_share: u128,
    /// `held` after the sync.
    pub held: u64,
    /// `stream_remaining` after the sync: the running stream's lamports not yet released.
    pub stream_remaining: u64,
    /// `stream_next` after the sync: shares waiting for the running stream.
    pub stream_next: u64,
    /// `total_distributed` after the sync.
    pub distributed: u64,
}

impl Synced {
    /// Shared lamports not yet released: the running stream's and the waiting shares'.
    pub fn streaming(&self) -> u64 {
        self.stream_remaining.saturating_add(self.stream_next)
    }
}

/// What a linear stream of `amount` lamports over `[from, to]` has left to release from `from`
/// releases at `now`, rounded down: all of it at or after `to`, nothing at or before `from`.
fn linear(amount: u64, from: i64, to: i64, now: i64) -> Option<u64> {
    if amount == 0 {
        return Some(0);
    }
    if now >= to {
        return Some(amount);
    }
    if now <= from {
        return Some(0);
    }
    let elapsed = u128::try_from(now.checked_sub(from)?).ok()?;
    let span = u128::try_from(to.checked_sub(from)?).ok()?;
    u64::try_from(u128::from(amount).checked_mul(elapsed)? / span).ok()
}

/// The shared stream at `now` (§4.7 with the review fixes): what it releases, and what is left
/// running and waiting after it, as `(released, stream_remaining, stream_next)`.
///
/// The running stream releases `stream_remaining` linearly over `[stream_last, stream_end]`. The
/// shares waiting behind it (`stream_next`) stream linearly over the hour after its end,
/// `[stream_end, stream_end + SHARE_STREAM_SECS]`, once it is out. Nothing is released while
/// holders hold less than `min_eligible`: the stream is paused then (its end moves on with the
/// clock), so a release is always divided at once among eligible holders.
pub fn stream_at(config: &KitConfig, now: i64) -> Option<(u64, u64, u64)> {
    let (running, waiting) = (config.stream_remaining, config.stream_next);
    let eligible = config.eligible > 0 && config.eligible >= config.min_eligible;
    if !eligible || (running == 0 && waiting == 0) {
        return Some((0, running, waiting));
    }
    let first = linear(running, config.stream_last, config.stream_end, now)?;
    let left = running - first;
    if left > 0 || waiting == 0 || now < config.stream_end {
        return Some((first, left, waiting));
    }
    let start = config.stream_end;
    let second = linear(waiting, start, start.checked_add(SHARE_STREAM_SECS)?, now)?;
    Some((first.checked_add(second)?, waiting - second, 0))
}

/// What the shared stream releases at `now`, rounded down ([`stream_at`]).
pub fn released_at(config: &KitConfig, now: i64) -> Option<u64> {
    stream_at(config, now).map(|(released, _, _)| released)
}

/// The state after a sync at `now` with the reward vault holding `vault_amount`. `None` when the
/// state is impossible (the vault and the claims below what was seen, or an overflow).
pub fn synced(config: &KitConfig, vault_amount: u64, now: i64) -> Option<Synced> {
    let (released, stream_remaining, stream_next) = stream_at(config, now)?;
    let pending = vault_amount
        .checked_add(config.total_claimed)?
        .checked_sub(config.seen)?
        .checked_add(released)?;
    let pot = pending.checked_add(config.held)?;
    let eligible = config.eligible > 0 && config.eligible >= config.min_eligible;
    let divides = eligible && pot > 0;
    let acc_per_share = if divides {
        let scaled = u128::from(pot)
            .checked_mul(SCALE)?
            .checked_add(config.rem)?;
        config
            .acc_per_share
            .checked_add(scaled / u128::from(config.eligible))?
    } else {
        config.acc_per_share
    };
    Some(Synced {
        released,
        pending,
        divides,
        acc_per_share,
        held: if eligible { 0 } else { pot },
        stream_remaining,
        stream_next,
        distributed: if divides {
            config.total_distributed.checked_add(pot)?
        } else {
            config.total_distributed
        },
    })
}

/// What `owner`, holding `balance` with `hook_data`, could claim at `now`: 0 for the pool, the
/// launch and a token without holder rewards; otherwise `owed + balance * (acc' - snapshot) /
/// SCALE`, rounded down, where `acc'` is `acc_per_share` after a sync at `now`. A claim pays
/// this, or the vault's balance if that is less.
pub fn claimable(
    config: &KitConfig,
    vault_amount: u64,
    owner: &Pubkey,
    balance: u64,
    hook_data: &[u8; 64],
    now: i64,
) -> Option<u64> {
    if !config.rewards_on() || config.is_excluded(owner) {
        return Some(0);
    }
    let after = synced(config, vault_amount, now)?;
    let data = HolderData::read(hook_data);
    let growth = after.acc_per_share.checked_sub(data.snapshot)?;
    let earned = u128::from(balance).checked_mul(growth)? / SCALE;
    data.owed.checked_add(u64::try_from(earned).ok()?)
}
