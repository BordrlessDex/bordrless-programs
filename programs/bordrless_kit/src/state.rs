//! Accounts and per-holder state of the kit: [`KitConfig`] (one per token, at `["kit", mint]`),
//! the arguments `init` takes ([`KitInitArgs`]) and the 64 bytes the kit keeps in every holding
//! ([`HolderData`]).

use anchor_lang::prelude::*;
use bordrless_hook::HOOK_DATA_LEN;

use crate::constants::*;
use crate::error::KitError;
use crate::legs::LegPools;

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
    /// The launch's creator is a companion's creator address (`PDA(["creator", mint], COMPANION_ID)`,
    /// docs/companions.md), decided once at `init`: the companion holds the creator's dev bag and what
    /// it buys back to burn, so it is excluded like the pool and the launch. Taken from `reserved`.
    pub creator_is_companion: bool,
    /// Reserved. `reserved[0]` is the native flag and `reserved[1..9]` the native kit's lamport
    /// base ([`KitConfig::is_native`]); `reserved[9]` is [`QUOTE_KIND_TOKENS`] for the kit of a coin
    /// quoted in tokens ([`KitConfig::is_token_quoted`], `init_legs`, `docs/token-quotes.md`
    /// §6.1). Zeros for every config made before.
    pub reserved: [u8; 55],
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

/// Holder rewards of a coin quoted in tokens (`docs/token-quotes.md` §7.5.1), `init_legs`' last
/// argument: `kind` 0 (none: the call the launchpad made before quote-token rewards), 1 (a one-leg
/// coin's token through the accumulator), 2 (every leg's token in rounds of `round_secs`), 3 (SOL).
/// Optional in the data: data that ends after `KitInitArgs` reads as kind 0; a part of it fails.
#[derive(AnchorSerialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LegsRewards {
    pub kind: u8,
    /// A rounds kit's round length (600 to 86,400 seconds); 0 for the other kinds.
    pub round_secs: u32,
}

impl AnchorDeserialize for LegsRewards {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let mut first = [0u8; 1];
        let read = loop {
            match reader.read(&mut first) {
                Err(e) if e.kind() == borsh::io::ErrorKind::Interrupted => continue,
                other => break other?,
            }
        };
        if read == 0 {
            return Ok(Self::default());
        }
        let mut secs = [0u8; 4];
        reader.read_exact(&mut secs)?;
        Ok(Self {
            kind: first[0],
            round_secs: u32::from_le_bytes(secs),
        })
    }
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
            creator_is_companion: args.creator
                == Pubkey::find_program_address(
                    &[COMPANION_CREATOR_SEED, mint.as_ref()],
                    &COMPANION_ID,
                )
                .0,
            reserved: [0; 55],
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

    /// Whether the reward vault is lamports on this config (a native kit, `crate::native`) rather
    /// than the holding of bridged SOL at `reward_vault`. `reserved[0]`; zero for every config
    /// made before.
    pub fn is_native(&self) -> bool {
        self.reserved[0] == 1
    }

    /// A native kit's lamport base (`reserved[1..9]`): the config's lamports that are not rewards
    /// (its rent, and whatever it held when it turned native).
    pub fn lamports_base(&self) -> u64 {
        u64::from_le_bytes(self.reserved[1..9].try_into().expect("8 bytes"))
    }

    /// Turns the kit native with `base`, or back (`None`).
    pub fn set_native(&mut self, base: Option<u64>) {
        self.reserved[0] = u8::from(base.is_some());
        self.reserved[1..9].copy_from_slice(&base.unwrap_or(0).to_le_bytes());
    }

    /// A native kit's vault amount: the config's `lamports` above its base.
    pub fn native_vault(&self, lamports: u64) -> u64 {
        lamports.saturating_sub(self.lamports_base())
    }

    /// Whether this is the kit of a coin quoted in tokens (`reserved[9]`, set by `init_legs`): its
    /// callbacks take the coin's `Legs` after their two extras, and every leg's pool is its pool
    /// ([`crate::legs::LegPools`]). 0 for every config made before.
    pub fn is_token_quoted(&self) -> bool {
        self.reserved[QUOTE_KIND_OFFSET] == QUOTE_KIND_TOKENS
    }

    /// Marks the kit as a coin quoted in tokens'.
    pub fn set_token_quoted(&mut self) {
        self.reserved[QUOTE_KIND_OFFSET] = QUOTE_KIND_TOKENS;
    }

    /// The reward kind of a coin quoted in tokens with holder rewards (`reserved[10]`,
    /// `docs/token-quotes.md` §7.5.1): [`REWARD_KIND_TOKEN`], [`REWARD_KIND_ROUNDS`] or
    /// [`REWARD_KIND_SOL`]; 0 for every other kit, whose rewards are bridged or native SOL as before.
    pub fn reward_kind(&self) -> u8 {
        self.reserved[REWARD_KIND_OFFSET]
    }

    /// Sets the reward kind (`init_legs`).
    pub fn set_reward_kind(&mut self, kind: u8) {
        self.reserved[REWARD_KIND_OFFSET] = kind;
    }

    /// Whether holder rewards run through the kit's accumulator (`acc_per_share`, the holders'
    /// bytes 0..24): holder rewards on and not paid in rounds. Every kit made before: exactly
    /// [`Self::rewards_on`].
    pub fn accumulates(&self) -> bool {
        self.rewards_on() && self.reward_kind() != REWARD_KIND_ROUNDS
    }

    /// Whether rewards are paid in rounds (kind 2).
    pub fn pays_rounds(&self) -> bool {
        self.rewards_on() && self.reward_kind() == REWARD_KIND_ROUNDS
    }

    /// Whether rewards are a one-leg coin's token through the accumulator (kind 1): the vault is an
    /// SPL Token or Token-2022 account, read by [`crate::spl`].
    pub fn pays_token(&self) -> bool {
        self.rewards_on() && self.reward_kind() == REWARD_KIND_TOKEN
    }

    /// A rounds kit's round length (`reserved[11..15]`).
    pub fn round_secs(&self) -> u32 {
        u32::from_le_bytes(
            self.reserved[ROUND_SECS_OFFSET..ROUND_SECS_OFFSET + 4]
                .try_into()
                .expect("4 bytes"),
        )
    }

    /// Sets the round length.
    pub fn set_round_secs(&mut self, secs: u32) {
        self.reserved[ROUND_SECS_OFFSET..ROUND_SECS_OFFSET + 4]
            .copy_from_slice(&secs.to_le_bytes());
    }

    /// The round at `now` of a rounds kit: `⌊now / round_secs⌋` (0 for a clock before 1970 or a
    /// kit without rounds).
    pub fn round_at(&self, now: i64) -> u32 {
        let secs = i64::from(self.round_secs());
        if secs <= 0 || now <= 0 {
            return 0;
        }
        u32::try_from(now / secs).unwrap_or(u32::MAX)
    }

    /// A token kit's reward program (`reserved[15]`: 0 SPL Token, 1 Token-2022) and its reward
    /// mint's decimals (`reserved[16]`).
    pub fn reward_token(&self) -> (u8, u8) {
        (
            self.reserved[REWARD_PROGRAM_OFFSET],
            self.reserved[REWARD_DECIMALS_OFFSET],
        )
    }

    /// Sets them.
    pub fn set_reward_token(&mut self, program: u8, decimals: u8) {
        self.reserved[REWARD_PROGRAM_OFFSET] = program;
        self.reserved[REWARD_DECIMALS_OFFSET] = decimals;
    }

    /// Whether `owner` is excluded (the pool, the launch, or a companion that is the launch's
    /// creator, holding a vesting dev bag or what it buys back to burn): never settled, capped,
    /// counted in `eligible`, and never able to claim. Any other creator is a holder like anyone.
    pub fn is_excluded(&self, owner: &Pubkey) -> bool {
        *owner == self.pool
            || *owner == self.launch
            || (self.creator_is_companion && *owner == self.creator)
    }

    /// [`Self::is_excluded`], every leg's pool of a coin quoted in tokens (`legs`) counting as the
    /// pool.
    pub fn is_excluded_with(&self, owner: &Pubkey, legs: Option<&LegPools>) -> bool {
        self.is_excluded(owner) || legs.is_some_and(|l| l.has(owner))
    }

    /// Whether `owner` is the launch's pool, or with `legs` any leg's pool: a transfer out of it is
    /// a buy.
    pub fn is_pool(&self, owner: &Pubkey, legs: Option<&LegPools>) -> bool {
        *owner == self.pool || legs.is_some_and(|l| l.has(owner))
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

/// Bytes 32..56 of a holding of a rounds kit (`docs/token-quotes.md` §7.5.1): the least the holding
/// held over this round and the one before, kept by the callbacks, and the last round it was paid.
/// What a round pays a holding is its share of each leg's round amount on
/// [`RoundData::eligible_balance`], the least it held from the start of the round before until
/// now: a holding paid in round `r` held at least that much when the round's supply was taken (in
/// `r`), so the holdings paid in a round add up to at most that supply, and what anyone sends a
/// holding (a coin's dust included) never lowers what it is paid. Zero on every holding of every
/// other kit (the callbacks of other kits never write these bytes), and written back as zero when a
/// holding is emptied, so it can close.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RoundData {
    /// The round of the last update (bytes 32..36).
    pub mark: u32,
    /// The last round the holding was paid (bytes 36..40).
    pub paid: u32,
    /// The least it held during round `mark` (bytes 40..48).
    pub min_cur: u64,
    /// The least it held during round `mark - 1` (bytes 48..56).
    pub min_prev: u64,
}

impl RoundData {
    /// Bytes 32..56 of a holding's hook data.
    pub fn read(data: &[u8; HOOK_DATA_LEN]) -> Self {
        let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().expect("4"));
        let u64_at = |at: usize| u64::from_le_bytes(data[at..at + 8].try_into().expect("8"));
        Self {
            mark: u32_at(32),
            paid: u32_at(36),
            min_cur: u64_at(40),
            min_prev: u64_at(48),
        }
    }

    /// `data` with bytes 32..56 these and the rest as they are.
    pub fn write(&self, data: &mut [u8; HOOK_DATA_LEN]) {
        data[32..36].copy_from_slice(&self.mark.to_le_bytes());
        data[36..40].copy_from_slice(&self.paid.to_le_bytes());
        data[40..48].copy_from_slice(&self.min_cur.to_le_bytes());
        data[48..56].copy_from_slice(&self.min_prev.to_le_bytes());
    }

    /// Moves the record to `round`, the holding having held `before` since its last update (every
    /// change of a balance goes through a callback, so it held exactly that since then).
    pub fn roll(&mut self, round: u32, before: u64) {
        if round > self.mark {
            self.min_prev = if round == self.mark.saturating_add(1) {
                self.min_cur
            } else {
                before
            };
            self.min_cur = before;
            self.mark = round;
        }
    }

    /// A transfer at `round` that leaves the holding with `after`, from `before`: the record rolled
    /// to the round and its least lowered to `after`. Emptied, the record is cleared (the holding
    /// held nothing, so it is owed nothing until it has held again through a whole round).
    pub fn on_transfer(&mut self, round: u32, before: u64, after: u64) {
        if after == 0 {
            *self = Self::default();
            return;
        }
        self.roll(round, before);
        self.min_cur = self.min_cur.min(after);
    }

    /// What round `round` pays the holding on, holding `balance` now: the least it held from the
    /// start of the round before until now.
    pub fn eligible_balance(&self, round: u32, balance: u64) -> u64 {
        if self.mark >= round {
            self.min_prev.min(self.min_cur)
        } else if self.mark.saturating_add(1) == round {
            self.min_cur.min(balance)
        } else {
            balance
        }
    }
}

/// One leg of a round: what the round shares of the leg's vault, and what it has paid of it.
#[derive(
    AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, Debug, Default, PartialEq, Eq,
)]
pub struct RoundLeg {
    /// The leg vault's balance when the round started.
    pub amount: u64,
    /// Paid of it so far this round.
    pub paid: u64,
}

/// A rounds kit's current round (`docs/token-quotes.md` §7.5.1), at `["kit-rounds", mint]`: made by
/// the first `start_round` (its sender paying), written by every `start_round` and `pay_round`.
#[account]
#[derive(InitSpace, Debug, PartialEq, Eq)]
pub struct KitRounds {
    /// Layout version.
    pub version: u8,
    /// Bump.
    pub bump: u8,
    /// The coin.
    pub mint: Pubkey,
    /// The round started (`⌊now / round_secs⌋`).
    pub round: u32,
    /// When it started.
    pub started_at: i64,
    /// The kit's eligible supply when it started: the divisor of every holding's share.
    pub supply: u64,
    /// How many legs.
    pub count: u8,
    /// Each leg's amount and what has been paid of it.
    pub legs: [RoundLeg; LEGS_MAX],
    /// Reserved.
    pub reserved: [u8; 32],
}

impl KitRounds {
    /// Account size.
    pub const LEN: usize = DISCRIMINATOR_LEN + Self::INIT_SPACE;

    /// The address of `mint`'s, and its bump.
    pub fn address(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[KIT_ROUNDS_SEED, mint.as_ref()], &crate::ID)
    }
}
