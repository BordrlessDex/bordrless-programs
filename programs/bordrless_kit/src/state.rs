//! Accounts and per-holder state of the kit: [`KitConfig`] (one per token, at `["kit", mint]`),
//! the arguments `init` takes ([`KitInitArgs`]) and the 64 bytes the kit keeps in every holding
//! ([`HolderData`]).

use anchor_lang::prelude::*;
use bordrless_hook::HOOK_DATA_LEN;

use crate::constants::*;
use crate::error::KitError;

/// The kit's settings and reward accounting for one token, at `["kit", mint]` (`docs/hooks-v2.md`
/// §4.3). Created only by `init`, which the launch's kit-caller PDA for the mint must sign.
#[account]
#[derive(InitSpace, Debug, PartialEq, Eq)]
pub struct KitConfig {
    /// Layout version.
    pub version: u8,
    /// Bump of `["kit", mint]`.
    pub bump: u8,
    /// Bump of `PDA(["kit-caller", mint], LAUNCH_ID)`.
    pub kit_caller_bump: u8,
    /// Bit set of [`modules`].
    pub modules: u8,
    /// Set by `graduate`: max wallet no longer applies.
    pub graduated: bool,
    /// Sum of the balances of every holding whose owner is not excluded (the pool, the launch).
    pub eligible: u64,
    /// `supply / 1_000` at `init`: rewards are divided only while `eligible` is at least this.
    pub min_eligible: u64,
    /// The token.
    pub mint: Pubkey,
    /// The launch (excluded; its reserve only ever sends after `init`).
    pub launch: Pubkey,
    /// The launch pool (excluded).
    pub pool: Pubkey,
    /// The creator (the creator wallet lock's wallet).
    pub creator: Pubkey,
    /// The launch's quote mint (bridged SOL, which has no hook): rewards are paid in it.
    pub reward_mint: Pubkey,
    /// `holding(reward_mint, kit_config)`; the default key when holder rewards are off.
    pub reward_vault: Pubkey,
    /// The supply at `init` (all of it in the launch reserve).
    pub supply_at_init: u64,
    /// Max wallet in basis points of `supply_at_init`; 0 when off.
    pub max_wallet_bps: u16,
    /// `supply_at_init * max_wallet_bps / 10_000`; 0 when off. Fixed: burns never tighten it.
    pub max_wallet_amount: u64,
    /// The creator wallet unlocks at this time; 0 when off.
    pub creator_unlock_at: i64,
    /// Tokens bought from the pool before this time are locked; 0 when off.
    pub early_window_end: i64,
    /// ... until this time; 0 when off.
    pub early_unlock_at: i64,
    /// Rewards per eligible base unit, scaled by [`SCALE`].
    pub acc_per_share: u128,
    /// The scaled remainder of the last division, below the eligible supply it was divided by.
    pub rem: u128,
    /// Lamports waiting while `eligible < min_eligible`.
    pub held: u64,
    /// `reward_vault.amount + total_claimed` at the last sync, counting shares as seen when they
    /// arrive.
    pub seen: u64,
    /// Lamports of the running stream (the share or shares streaming now) not yet released to
    /// holders. A later share never changes it, nor `stream_last` and `stream_end`.
    pub stream_remaining: u64,
    /// The running stream's last release.
    pub stream_last: i64,
    /// The running stream's end. While nobody is eligible it moves on with the clock.
    pub stream_end: i64,
    /// Lamports divided among holders so far.
    pub total_distributed: u64,
    /// Lamports claimed so far.
    pub total_claimed: u64,
    /// Lamports shared so far.
    pub total_shared: u64,
    /// `init` time.
    pub created_at: i64,
    /// Lamports shared while the stream was running, waiting for it: they stream linearly over
    /// the hour after `stream_end`, `[stream_end, stream_end + SHARE_STREAM_SECS]`, and become the
    /// running stream at the first sync after `stream_end`. Taken from `reserved`, so the account
    /// keeps its size and every other field its offset.
    pub stream_next: u64,
    /// Reserved.
    pub reserved: [u8; 56],
}

/// Arguments of `init`, passed by the launch inside `create_launch` and trusted because its
/// kit-caller PDA signs; the hard bounds are checked again ([`check_params`]).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct KitInitArgs {
    /// The launch PDA (excluded owner; holds the whole supply at `init`).
    pub launch: Pubkey,
    /// The launch pool (excluded owner).
    pub pool: Pubkey,
    /// The creator.
    pub creator: Pubkey,
    /// The launch's quote mint, in which rewards are paid.
    pub reward_mint: Pubkey,
    /// Bit set of [`modules`]: not zero, within `0b1111`.
    pub modules: u8,
    /// `1..=9_999` exactly when max wallet is on, else 0.
    pub max_wallet_bps: u16,
    /// `now < creator_unlock_at <= now + 365 days` exactly when the creator wallet lock is on,
    /// else 0.
    pub creator_unlock_at: i64,
    /// `now < early_window_end < early_unlock_at <= now + 30 days` exactly when the early-buyer
    /// lock is on, else both 0.
    pub early_window_end: i64,
    /// See `early_window_end`.
    pub early_unlock_at: i64,
    /// Bump of `PDA(["kit-caller", mint], LAUNCH_ID)`.
    pub kit_caller_bump: u8,
}

/// The hard bounds `init` checks again on its arguments at `now` (§4.10).
pub fn check_params(args: &KitInitArgs, now: i64) -> Result<()> {
    let on = |module: u8| args.modules & module != 0;
    require!(
        args.modules != 0 && args.modules & !modules::ALL == 0,
        KitError::InvalidModules
    );
    if on(modules::MAX_WALLET) {
        require!(
            (1..=MAX_MAX_WALLET_BPS).contains(&args.max_wallet_bps),
            KitError::InvalidMaxWallet
        );
    } else {
        require!(args.max_wallet_bps == 0, KitError::InvalidMaxWallet);
    }
    if on(modules::CREATOR_WALLET_LOCK) {
        let latest = now
            .checked_add(MAX_CREATOR_LOCK_SECS)
            .ok_or(KitError::MathOverflow)?;
        require!(
            now < args.creator_unlock_at && args.creator_unlock_at <= latest,
            KitError::InvalidCreatorLock
        );
    } else {
        require!(args.creator_unlock_at == 0, KitError::InvalidCreatorLock);
    }
    if on(modules::EARLY_BUYER_LOCK) {
        let latest = now
            .checked_add(MAX_EARLY_LOCK_SECS)
            .ok_or(KitError::MathOverflow)?;
        require!(
            now < args.early_window_end
                && args.early_window_end < args.early_unlock_at
                && args.early_unlock_at <= latest,
            KitError::InvalidEarlyLock
        );
    } else {
        require!(
            args.early_window_end == 0 && args.early_unlock_at == 0,
            KitError::InvalidEarlyLock
        );
    }
    Ok(())
}

impl KitConfig {
    /// Account size.
    pub const LEN: usize = DISCRIMINATOR_LEN + Self::INIT_SPACE;

    /// The config address of `mint`, and its bump.
    pub fn address(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[KIT_SEED, mint.as_ref()], &crate::ID)
    }

    /// The config `init` writes for `mint` with `supply` (the whole of it in the launch reserve)
    /// at `now`: the arguments' bounds and the supply's are checked, `min_eligible` and
    /// `max_wallet_amount` derived, the accounting zeroed. `reward_vault` is present exactly when
    /// holder rewards are on.
    pub fn install(
        args: &KitInitArgs,
        mint: Pubkey,
        supply: u64,
        bump: u8,
        reward_vault: Option<Pubkey>,
        now: i64,
    ) -> Result<Self> {
        check_params(args, now)?;
        require!(
            (MIN_SUPPLY..=MAX_SUPPLY).contains(&supply),
            KitError::SupplyOutOfBounds
        );
        let rewards = args.modules & modules::HOLDER_REWARDS != 0;
        match (rewards, reward_vault) {
            (true, None) => return err!(KitError::MissingRewardVault),
            (false, Some(_)) => return err!(KitError::WrongRewardVault),
            _ => {}
        }
        let max_wallet_amount = if args.modules & modules::MAX_WALLET != 0 {
            let cap = u128::from(supply) * u128::from(args.max_wallet_bps) / u128::from(BPS);
            let cap = u64::try_from(cap).map_err(|_| KitError::MathOverflow)?;
            // A cap of nothing would refuse every buy until graduation.
            require!(cap > 0, KitError::InvalidMaxWallet);
            cap
        } else {
            0
        };
        Ok(Self {
            version: VERSION,
            bump,
            kit_caller_bump: args.kit_caller_bump,
            modules: args.modules,
            graduated: false,
            eligible: 0,
            min_eligible: supply / MIN_ELIGIBLE_DIVISOR,
            mint,
            launch: args.launch,
            pool: args.pool,
            creator: args.creator,
            reward_mint: args.reward_mint,
            reward_vault: reward_vault.unwrap_or_default(),
            supply_at_init: supply,
            max_wallet_bps: args.max_wallet_bps,
            max_wallet_amount,
            creator_unlock_at: args.creator_unlock_at,
            early_window_end: args.early_window_end,
            early_unlock_at: args.early_unlock_at,
            acc_per_share: 0,
            rem: 0,
            held: 0,
            seen: 0,
            stream_remaining: 0,
            stream_last: 0,
            stream_end: 0,
            total_distributed: 0,
            total_claimed: 0,
            total_shared: 0,
            created_at: now,
            stream_next: 0,
            reserved: [0; 56],
        })
    }

    /// Whether `module` ([`modules`]) is on.
    pub fn has(&self, module: u8) -> bool {
        self.modules & module != 0
    }

    /// Whether holder rewards are on.
    pub fn rewards_on(&self) -> bool {
        self.has(modules::HOLDER_REWARDS)
    }

    /// Whether `owner` is excluded (the pool or the launch): never settled, capped, counted in
    /// `eligible`, and never able to claim.
    pub fn is_excluded(&self, owner: &Pubkey) -> bool {
        *owner == self.pool || *owner == self.launch
    }

    /// Whether the token may never be sent to `owner` (§4.5): the launch, this config (at
    /// `config`), the default key (the system program's address) and the protocol's programs.
    pub fn is_refused_destination(&self, config: &Pubkey, owner: &Pubkey) -> bool {
        *owner == self.launch
            || owner == config
            || *owner == Pubkey::default()
            || *owner == TOKEN_ID
            || *owner == SWAP_ID
            || *owner == BRIDGE_ID
            || *owner == LAUNCH_ID
            || *owner == crate::ID
    }
}

/// The kit's 64 bytes in a holding, little-endian (§4.4): `snapshot` (bytes 0..16) and `owed`
/// (16..24) for holder rewards, `early_locked` (24..32) for the early-buyer lock, and 32
/// reserved bytes, zero. All zero for a holding the kit has never written, and written back as
/// all zero whenever nothing is left to keep, so the holding can close.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HolderData {
    /// `acc_per_share` at the holder's last settle (0 while the balance is 0).
    pub snapshot: u128,
    /// Rewards earned and not claimed, in lamports.
    pub owed: u64,
    /// Tokens bought in the early window, locked until `early_unlock_at`.
    pub early_locked: u64,
}

impl HolderData {
    /// The fields of a holding's hook data.
    pub fn read(data: &[u8; HOOK_DATA_LEN]) -> Self {
        let mut snapshot = [0u8; 16];
        snapshot.copy_from_slice(&data[0..16]);
        let mut owed = [0u8; 8];
        owed.copy_from_slice(&data[16..24]);
        let mut early_locked = [0u8; 8];
        early_locked.copy_from_slice(&data[24..32]);
        Self {
            snapshot: u128::from_le_bytes(snapshot),
            owed: u64::from_le_bytes(owed),
            early_locked: u64::from_le_bytes(early_locked),
        }
    }

    /// The hook data these fields make, the reserved bytes zero.
    pub fn to_bytes(&self) -> [u8; HOOK_DATA_LEN] {
        let mut data = [0u8; HOOK_DATA_LEN];
        data[0..16].copy_from_slice(&self.snapshot.to_le_bytes());
        data[16..24].copy_from_slice(&self.owed.to_le_bytes());
        data[24..32].copy_from_slice(&self.early_locked.to_le_bytes());
        data
    }

    /// `read` bytes with the reward fields (bytes 0..24) replaced by these and bytes 24..64 kept
    /// exactly as they are: what `claim` writes.
    pub fn with_rewards_of(&self, read: &[u8; HOOK_DATA_LEN]) -> [u8; HOOK_DATA_LEN] {
        let mut data = *read;
        data[0..16].copy_from_slice(&self.snapshot.to_le_bytes());
        data[16..24].copy_from_slice(&self.owed.to_le_bytes());
        data
    }
}
