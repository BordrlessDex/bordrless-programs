//! The legs of a coin quoted in tokens (`docs/token-quotes.md` §6.1), as the kit sees them: the
//! launchpad's `Legs` at `["legs", mint]`, which its callbacks take after their two extras, read
//! in place ([`LegPools::read`]). Every leg's pool is the launch's pool to the kit (excluded from
//! holders, never capped, a buy out of it an early buy), the `Legs` says when the legs together
//! have sold what would have graduated a SOL coin, which lifts max wallet, and when trading opened
//! (the last leg's opening), from which the locks run ([`LegPools::delay`]).

use anchor_lang::prelude::*;

use crate::constants::*;
use crate::error::KitError;

/// What the kit reads of a coin's `Legs`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LegPools {
    /// The legs' pools, `count` of them (the rest the default key).
    pub pools: [Pubkey; LEGS_MAX],
    /// How many legs.
    pub count: usize,
    /// The launchpad lifted max wallet: the legs together sold the graduation point.
    pub lifted: bool,
    /// Every leg is open: trading is open.
    pub all_open: bool,
    /// The latest `opened_at` among the legs (when the last one opened, once `all_open`).
    pub last_opened_at: i64,
}

impl LegPools {
    /// `info` read as `mint`'s `Legs`: owned by the launchpad, a `Legs` by its discriminator and
    /// size, of this mint. Only the launchpad's `create_token_launch` makes a `Legs`, and only at
    /// `["legs", mint]` with that mint, so its owning program, its discriminator and its mint bind
    /// it without a derivation (as a `KitConfig` is bound).
    pub fn read(info: &AccountInfo, mint: &Pubkey) -> Result<Self> {
        require_keys_eq!(*info.owner, LAUNCH_ID, KitError::WrongLegs);
        let data = info.try_borrow_data()?;
        require!(
            data.len() >= LEGS_LEN && data[..8] == LEGS_DISCRIMINATOR,
            KitError::WrongLegs
        );
        let key = |at: usize| Pubkey::new_from_array(data[at..at + 32].try_into().expect("32"));
        require_keys_eq!(key(LEGS_MINT_OFFSET), *mint, KitError::WrongLegs);
        let count = usize::from(data[LEGS_COUNT_OFFSET]).min(LEGS_MAX);
        let mut pools = [Pubkey::default(); LEGS_MAX];
        let mut last_opened_at = 0i64;
        for (i, pool) in pools.iter_mut().enumerate().take(count) {
            let leg = LEGS_FIRST_LEG_OFFSET + i * LEGS_LEG_LEN;
            *pool = key(leg + LEGS_LEG_POOL_OFFSET);
            let at = leg + LEGS_LEG_OPENED_AT_OFFSET;
            let opened_at = i64::from_le_bytes(data[at..at + 8].try_into().expect("8"));
            last_opened_at = last_opened_at.max(opened_at);
        }
        Ok(Self {
            pools,
            count,
            lifted: data[LEGS_LIFTED_OFFSET] == 1,
            all_open: count > 0 && usize::from(data[LEGS_OPENED_OFFSET]) >= count,
            last_opened_at,
        })
    }

    /// Whether `owner` is one of the legs' pools.
    pub fn has(&self, owner: &Pubkey) -> bool {
        self.pools[..self.count].contains(owner)
    }

    /// How long after the launch (`created_at`, the config's) trading opened, at `now`: the last
    /// leg's opening once every leg is open, else `now` (nothing trades before then, so no lock
    /// has started). Every lock's time moves by it: the creator wallet lock, the early-buyer window
    /// and its unlock run from when trading opened, as a SOL coin's run from its launch, which is
    /// when it opens. Never below 0.
    pub fn delay(&self, created_at: i64, now: i64) -> i64 {
        let opened = if self.all_open {
            self.last_opened_at
        } else {
            now
        };
        opened.saturating_sub(created_at).max(0)
    }

    /// Leg `index`'s token, read from `info` (the coin's `Legs`, which [`Self::read`] bound to the
    /// mint): its quote mint, its program byte (0 SPL Token, 1 Token-2022) and its decimals.
    /// `NoSuchLeg` is `WrongLegs` here: the kit has no leg of its own.
    pub fn token(info: &AccountInfo, index: usize) -> Result<LegToken> {
        let data = info.try_borrow_data()?;
        let count = usize::from(data[LEGS_COUNT_OFFSET]).min(LEGS_MAX);
        require!(index < count, KitError::WrongLegs);
        let at = LEGS_FIRST_LEG_OFFSET + index * LEGS_LEG_LEN;
        let mint_at = at + LEGS_LEG_QUOTE_MINT_OFFSET;
        Ok(LegToken {
            quote_mint: Pubkey::new_from_array(data[mint_at..mint_at + 32].try_into().expect("32")),
            program: data[at + LEGS_LEG_QUOTE_PROGRAM_OFFSET],
            decimals: data[at + LEGS_LEG_DECIMALS_OFFSET],
        })
    }
}

/// One leg's token, as the `Legs` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegToken {
    pub quote_mint: Pubkey,
    /// 0 SPL Token, 1 Token-2022.
    pub program: u8,
    pub decimals: u8,
}

impl LegToken {
    /// The leg token's program.
    pub fn program_id(&self) -> Pubkey {
        crate::spl::program_id(self.program)
    }

    /// The kit config's (`config`) vault of this leg's token: its associated token account.
    pub fn vault(&self, config: &Pubkey) -> Pubkey {
        crate::spl::associated_address(config, &self.quote_mint, &self.program_id())
    }
}
