//! The vault (one per coin, with its three slots) and the price arithmetic of the guards, ported
//! from the companion (`bordrless_companion::state`: `spot_price`, `step_toward`, `bps_of`).

use anchor_lang::prelude::*;

use crate::constants::*;

/// One slot of a vault: its policy, fixed at `create_vault`, and its running state.
///
/// Fixed layout (no `Option`): an unused slot is all zeros with `policy == policy::UNUSED`.
#[derive(
    AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, Debug, Default, PartialEq, Eq,
)]
pub struct Slot {
    /// `policy::BURN`, `SELL_FOR_SOL`, `SELL_BUY_BURN`, or `UNUSED`.
    pub policy: u8,
    /// `SellForSol`: the wallet paid. `SellBuyBurn`: the DEX pool of the token bought and burned (a
    /// launchpad pool quoted in bridged SOL). The default key for `Burn`.
    pub target: Pubkey,
    /// Bump of the slot's owner, `PDA(["slot", mint, [i]])`.
    pub owner_bump: u8,
    /// `SellBuyBurn` with a custom-hook token: the most that token's own hook may cut from the
    /// slot's buy, as the creator declared it (basis points, at most `MAX_HOOK_CUT_BPS`): the buy's
    /// `min_out` is loosened by that cut and Bordrless's share of it. 0 otherwise.
    pub max_cut_bps: u16,
    /// The slot's last sell (or its burn, or its retirement); 0 before the first. The next sell is
    /// an interval later.
    pub last_at: i64,
    /// The last time the slot's sell waited (the price more than 3% below the reference); 0 before
    /// the first. The next attempt is `MIN_INTERVAL` later: a wait and a sell never share a
    /// transaction (or a block), and a one-block dip costs the slot a minute, not an interval.
    pub waited_at: i64,
    /// The sell's reference price (quote per base unit times `PRICE_SCALE`): set to the pool's price
    /// at `open_vault`, raised only by a sell, lowered toward the price only while a sell waits.
    pub reference_price: u128,
    /// When the sell's reference clock last restarted: at `open_vault`, at every sale, and at every
    /// wait that stepped the reference. A wait steps it once for each interval since then.
    pub reference_at: i64,
    /// Bridged SOL the slot holds for later: `SellBuyBurn`'s proceeds, to buy with;
    /// `SellForSol`'s payment to an empty wallet that it could not yet open (below a wallet's
    /// rent-exempt minimum).
    pub pending_sol: u64,
    /// The buy's reference price (of the token bought): set at `open_vault`, lowered only by a buy,
    /// raised toward the price only while a buy waits (the companion's buyback reference, as is).
    pub buy_reference: u128,
    /// When the buy's reference clock last restarted (at `open_vault`, every buy, and every wait
    /// that stepped it).
    pub buy_reference_at: i64,
    /// The slot's last buy; 0 before the first.
    pub buy_last_at: i64,
    /// The last time the slot's buy waited; the next attempt is `MIN_INTERVAL` later.
    pub buy_waited_at: i64,
    /// Coin sold so far.
    pub sold: u64,
    /// SOL the sells brought in so far (before bounties).
    pub sol_out: u64,
    /// The other token bought and burned so far (`SellBuyBurn`).
    pub x_burned: u64,
    /// Coin burned so far (`Burn`, and `retire`).
    pub burned: u64,
    /// Bounties paid so far (lamports).
    pub bounties: u64,
}

impl Slot {
    /// Whether the slot sells its coin.
    pub fn sells(&self) -> bool {
        self.policy == policy::SELL_FOR_SOL || self.policy == policy::SELL_BUY_BURN
    }

    /// When the slot last ran: the latest of `opened_at`, its last sell, burn or retirement, its
    /// last buy, and its last wait on either leg.
    pub fn last_run(&self, opened_at: i64) -> i64 {
        opened_at
            .max(self.last_at)
            .max(self.buy_last_at)
            .max(self.waited_at)
            .max(self.buy_waited_at)
    }
}

/// A coin's vault, `PDA(["vault", mint])`.
#[account]
#[derive(InitSpace, Debug)]
pub struct Vault {
    /// Layout version.
    pub version: u8,
    /// Bump.
    pub bump: u8,
    /// The coin.
    pub mint: Pubkey,
    /// The coin's own token hook (its launch's `custom_hook`), which sends the vault its cuts.
    pub hook: Pubkey,
    /// Who paid for the vault (informational).
    pub creator: Pubkey,
    /// Slots in use: `slots[..n_slots]`.
    pub n_slots: u8,
    pub slots: [Slot; MAX_SLOTS],
    /// The crank's pay, in basis points of the SOL a sell or a buy moves.
    pub bounty_bps: u16,
    /// The most one sell takes of the pool's quote side (basis points), and never more than the
    /// launch's `pool_share_bps`.
    pub max_sell_bps: u16,
    /// The least time between two sells of a slot, and between two buys.
    pub interval: i64,
    /// The most the coin's own hook may cut from the vault's sell, as its creator declared it: the
    /// sell's `min_out` is loosened by that cut and Bordrless's share of it, and by nothing more.
    pub max_hook_cut_bps: u16,
    /// `open_vault` has run: the slots' holdings exist and their references are set.
    pub opened: bool,
    pub opened_at: i64,
    pub created_at: i64,
    /// The last time any instruction changed the vault.
    pub last_activity_at: i64,
    /// Reserved.
    pub reserved: [u8; 64],
}

impl Vault {
    /// Account size.
    pub const LEN: usize = 8 + Self::INIT_SPACE;

    /// The vault of `mint`.
    pub fn address(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[VAULT_SEED, mint.as_ref()], &crate::ID)
    }

    /// Slot `i`'s owner of `mint`.
    pub fn slot_owner(mint: &Pubkey, i: u8) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[SLOT_SEED, mint.as_ref(), &[i]], &crate::ID)
    }
}

/// A pool's spot price, quote per base unit times `PRICE_SCALE`, from its effective reserves (real
/// and virtual); `None` for an empty base side.
pub fn spot_price(
    quote_reserve: u64,
    virtual_quote: u64,
    base_reserve: u64,
    virtual_base: u64,
) -> Option<u128> {
    let quote = u128::from(quote_reserve) + u128::from(virtual_quote);
    let base = u128::from(base_reserve) + u128::from(virtual_base);
    (base > 0).then(|| quote * PRICE_SCALE / base)
}

/// `reference` moved toward `spot` by at most `REFERENCE_STEP_BPS` of it (all the way when unset).
pub fn step_toward(reference: u128, spot: u128) -> u128 {
    if reference == 0 {
        return spot;
    }
    let step = reference * u128::from(REFERENCE_STEP_BPS) / u128::from(BPS);
    if spot > reference {
        spot.min(reference + step)
    } else {
        spot.max(reference.saturating_sub(step))
    }
}

/// `reference` moved toward `spot` once for every `interval` since `reference_at` (at least once,
/// at most `MAX_REFERENCE_STEPS`), so one crank after a long quiet catches up.
pub fn catch_up(reference: u128, spot: u128, reference_at: i64, interval: i64, now: i64) -> u128 {
    let steps = (now.saturating_sub(reference_at) / interval.max(1)).clamp(1, MAX_REFERENCE_STEPS);
    let mut r = reference;
    for _ in 0..steps {
        let next = step_toward(r, spot);
        if next == r {
            break;
        }
        r = next;
    }
    r
}

/// `part` basis points of `amount`, rounded down.
pub fn bps_of(amount: u64, part: u64) -> u64 {
    (u128::from(amount) * u128::from(part) / u128::from(BPS)) as u64
}

/// Bordrless's share of a cut of `bps` basis points (`share_bps` of it, rounded up), as the DEX
/// takes it on a launch pool.
pub fn share_of(bps: u64, share_bps: u64) -> u64 {
    (bps * share_bps).div_ceil(BPS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vault_fits_its_space() {
        let v = Vault {
            version: VERSION,
            bump: 255,
            mint: Pubkey::new_unique(),
            hook: Pubkey::new_unique(),
            creator: Pubkey::new_unique(),
            n_slots: 3,
            slots: [Slot {
                policy: policy::SELL_BUY_BURN,
                target: Pubkey::new_unique(),
                owner_bump: 254,
                max_cut_bps: MAX_HOOK_CUT_BPS,
                last_at: i64::MAX,
                waited_at: i64::MAX,
                reference_price: u128::MAX,
                reference_at: i64::MAX,
                pending_sol: u64::MAX,
                buy_reference: u128::MAX,
                buy_reference_at: i64::MAX,
                buy_last_at: i64::MAX,
                buy_waited_at: i64::MAX,
                sold: u64::MAX,
                sol_out: u64::MAX,
                x_burned: u64::MAX,
                burned: u64::MAX,
                bounties: u64::MAX,
            }; MAX_SLOTS],
            bounty_bps: 100,
            max_sell_bps: 100,
            interval: MAX_INTERVAL,
            max_hook_cut_bps: MAX_HOOK_CUT_BPS,
            opened: true,
            opened_at: 1,
            created_at: 2,
            last_activity_at: 3,
            reserved: [0; 64],
        };
        let mut data = Vec::new();
        v.try_serialize(&mut data).unwrap();
        assert_eq!(data.len(), Vault::LEN);
        assert_eq!(Slot::INIT_SPACE, 164);
        const { assert!(Vault::LEN < 720) };
    }

    #[test]
    fn the_reference_steps_and_catches_up() {
        let r = 1_000_000u128;
        assert_eq!(step_toward(0, 7), 7);
        assert_eq!(step_toward(r, 2 * r), r * 105 / 100);
        assert_eq!(step_toward(r, r / 2), r * 95 / 100);
        assert_eq!(step_toward(r, r + 1), r + 1);
        // Three intervals since it last moved: three steps down, never past the price.
        let three = catch_up(r, r / 2, 0, 60, 180);
        assert_eq!(three, r * 95 / 100 * 95 / 100 * 95 / 100);
        assert_eq!(catch_up(r, r * 99 / 100, 0, 60, 6_000), r * 99 / 100);
        // At least one step, even inside the first interval.
        assert_eq!(catch_up(r, r / 2, 0, 60, 1), r * 95 / 100);
    }

    #[test]
    fn shares_round_up() {
        assert_eq!(share_of(100, 2_500), 25);
        assert_eq!(share_of(1, 2_500), 1);
        assert_eq!(share_of(0, 2_500), 0);
        assert_eq!(bps_of(1_000, 9_800), 980);
    }
}
