//! Accounts of the launchpad, and the token rules a launch fixes ([`LaunchRules`]).

use anchor_lang::prelude::*;
use bordrless_kit::modules;

use crate::constants::*;

/// The rules a launch fixes for its token (`docs/hooks-v2.md` §5.1): what the pool hook takes on
/// each side, and the kit modules the token gets. Holder rewards are on when either holder fee is
/// above 0.
#[derive(
    AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, Debug, Default, PartialEq, Eq,
)]
pub struct LaunchRules {
    /// Holder fee on buys, from the quote input, paid to the holder vault.
    pub holder_fee_buy_bps: u16,
    /// Holder fee on sells, from the quote output after the protocol fee.
    pub holder_fee_sell_bps: u16,
    /// Burn on buys, from the token output.
    pub burn_buy_bps: u16,
    /// Burn on sells, from the token input.
    pub burn_sell_bps: u16,
    /// Max wallet in basis points of the supply; 0 = off. Lifts at graduation.
    pub max_wallet_bps: u16,
    /// Creator wallet lock, counted from the launch; 0 = off.
    pub creator_lock_secs: u32,
    /// Early-buyer window, counted from the launch; 0 = off.
    pub early_window_secs: u32,
    /// Early-buyer unlock, counted from the launch; above `early_window_secs` (0 when the lock is
    /// off).
    pub early_lock_secs: u32,
}

impl LaunchRules {
    /// No rules: no kit, no holder fee, no burn.
    pub const NONE: Self = Self {
        holder_fee_buy_bps: 0,
        holder_fee_sell_bps: 0,
        burn_buy_bps: 0,
        burn_sell_bps: 0,
        max_wallet_bps: 0,
        creator_lock_secs: 0,
        early_window_secs: 0,
        early_lock_secs: 0,
    };

    /// Whether holder rewards are on (a holder fee on either side).
    pub fn rewards_on(&self) -> bool {
        self.holder_fee_buy_bps > 0 || self.holder_fee_sell_bps > 0
    }

    /// Whether the pool hook burns on either side (clients then pass the base mint writable).
    pub fn burns(&self) -> bool {
        self.burn_buy_bps > 0 || self.burn_sell_bps > 0
    }

    /// The kit modules these rules install (§5.1): 1 with a holder fee, 2 with max wallet, 4 with
    /// the creator wallet lock, 8 with the early-buyer lock. 0 means no kit and no token hook at
    /// all; a burn alone runs in the pool hook.
    pub fn modules(&self) -> u8 {
        let mut m = 0;
        if self.rewards_on() {
            m |= modules::HOLDER_REWARDS;
        }
        if self.max_wallet_bps > 0 {
            m |= modules::MAX_WALLET;
        }
        if self.creator_lock_secs > 0 {
            m |= modules::CREATOR_WALLET_LOCK;
        }
        if self.early_window_secs > 0 {
            m |= modules::EARLY_BUYER_LOCK;
        }
        m
    }

    /// The fee rates the pool hook applies with `creator_fee_bps`.
    pub fn fee_rates(&self, creator_fee_bps: u16) -> bordrless_core::LaunchFeeRates {
        bordrless_core::LaunchFeeRates {
            creator_fee_bps,
            holder_fee_buy_bps: self.holder_fee_buy_bps,
            holder_fee_sell_bps: self.holder_fee_sell_bps,
            burn_buy_bps: self.burn_buy_bps,
            burn_sell_bps: self.burn_sell_bps,
        }
    }
}

/// The bounds `create_launch` holds token rules to (§5.2), kept in the [`Config`]. `init_config`
/// and `set_config` keep each within the hard ceilings of [`crate::constants::ceilings`].
#[derive(
    AnchorSerialize, AnchorDeserialize, InitSpace, Clone, Copy, Debug, Default, PartialEq, Eq,
)]
pub struct RuleBounds {
    /// Largest holder fee per side.
    pub max_holder_fee_bps: u16,
    /// Largest burn per side.
    pub max_burn_bps: u16,
    /// Largest creator fee + holder fee + burn on one side.
    pub max_rules_fee_bps: u16,
    /// Smallest max wallet (when on).
    pub min_max_wallet_bps: u16,
    /// Largest max wallet.
    pub max_max_wallet_bps: u16,
    /// Longest creator wallet lock.
    pub max_creator_lock_secs: u32,
    /// Longest early-buyer window.
    pub max_early_window_secs: u32,
    /// Latest early-buyer unlock, counted from the launch.
    pub max_early_lock_secs: u32,
}

/// Program configuration at `["config"]`. Launches snapshot the terms they are created with.
#[account]
#[derive(InitSpace, Debug)]
pub struct Config {
    /// Layout version.
    pub version: u8,
    /// Bump.
    pub bump: u8,
    /// May change the config.
    pub admin: Pubkey,
    /// Receives launch fees (lamports); must also be the DEX config's treasury.
    pub treasury: Pubkey,
    /// The quote every launch is paired with (bridged SOL).
    pub quote_mint: Pubkey,
    /// Lamports paid to launch.
    pub launch_fee_lamports: u64,
    /// LP fee of launch pools.
    pub lp_fee_bps: u16,
    /// Largest creator fee.
    pub max_creator_fee_bps: u16,
    /// Seconds of elevated LP fee after creation.
    pub sniper_window_secs: i64,
    /// LP fee at the start of the window.
    pub sniper_start_bps: u16,
    /// Share of the supply sold on the curve.
    pub curve_bps: u16,
    /// Supply of a launched token, base units.
    pub supply: u64,
    /// Decimals of a launched token.
    pub decimals: u8,
    /// Smallest virtual quote reserve.
    pub min_virtual_quote: u64,
    /// Largest virtual quote reserve.
    pub max_virtual_quote: u64,
    /// Stops new launches.
    pub paused: bool,
    /// Launches so far.
    pub launches: u64,
    /// The bounds of token rules.
    pub rule_bounds: RuleBounds,
    /// Reserved.
    pub reserved: [u8; 64],
}

impl Config {
    /// Account size.
    pub const LEN: usize = DISCRIMINATOR_LEN + Self::INIT_SPACE;
}

/// A launch config (`docs/hooks-v2.md` §5.7): the rules, the creator fee and, for a
/// build-your-own token, the creator's own token hook, made with the SDK by anyone as a keypair
/// account (so its key can be shared and pasted), fixed once created and usable for any number of
/// launches. `create_launch` takes one as an optional account; without one the rules come inline.
/// A config can never touch the protocol's share.
#[account]
#[derive(InitSpace, Debug)]
pub struct LaunchConfig {
    /// Layout version.
    pub version: u8,
    /// Who made it.
    pub creator: Pubkey,
    /// The token rules (within the launch config's bounds when made, checked again at launch).
    pub rules: LaunchRules,
    /// Creator fee on every swap, in basis points of the quote.
    pub creator_fee_bps: u16,
    /// The creator's own token hook program (§5.8), when the config is for a build-your-own
    /// token; then no kit module may be on (one token hook per mint). The mint is created with it
    /// and no hook authority.
    pub custom_hook: Option<Pubkey>,
    /// The hook's flags (`bordrless_hook::token_flags`); 0 without a custom hook.
    pub custom_hook_flags: u16,
    /// A short name, for the site.
    #[max_len(32)]
    pub label: String,
    /// Creation time.
    pub created_at: i64,
    /// A listed config's author share (`create_listed_config`): the part of the creator fee, in
    /// basis points of it (at most `MAX_AUTHOR_SHARE_BPS`), paid to `creator` (the config's
    /// author) on every launch made from it by someone else. Fixed at creation; 0 for a config
    /// made with `create_config`.
    pub author_share_bps: u16,
    /// Reserved.
    pub reserved: [u8; 30],
}

impl LaunchConfig {
    /// Account size.
    pub const LEN: usize = DISCRIMINATOR_LEN + Self::INIT_SPACE;

    /// The custom hook and its flags, when the config names one.
    pub fn hook(&self) -> Option<(Pubkey, u16)> {
        self.custom_hook.map(|h| (h, self.custom_hook_flags))
    }
}

/// A launch at `["launch", mint]`.
#[account]
#[derive(InitSpace, Debug)]
pub struct Launch {
    /// Layout version.
    pub version: u8,
    /// Bump.
    pub bump: u8,
    /// The token.
    pub mint: Pubkey,
    /// Who launched it.
    pub creator: Pubkey,
    /// The pool on the DEX.
    pub pool: Pubkey,
    /// The quote mint.
    pub quote_mint: Pubkey,
    /// `STATUS_CURVE` or `STATUS_GRADUATED`.
    pub status: u8,
    /// Creator fee on every swap, in basis points of the quote.
    pub creator_fee_bps: u16,
    /// LP fee of the pool.
    pub lp_fee_bps: u16,
    /// Sniper window.
    pub sniper_window_secs: i64,
    /// LP fee at the start of the window.
    pub sniper_start_bps: u16,
    /// Virtual quote reserve at opening.
    pub virtual_quote: u64,
    /// Virtual base reserve.
    pub virtual_base: u64,
    /// Real quote reserve at which the launch graduates.
    pub graduation_quote: u64,
    /// Tokens on the curve.
    pub curve_tokens: u64,
    /// Tokens reserved for graduation.
    pub reserve_tokens: u64,
    /// The launch's holding of the token (the reserve, then nothing).
    pub reserve_holding: Pubkey,
    /// The launch's holding of the quote (creator fees).
    pub quote_holding: Pubkey,
    /// The launch's holding of the pool's LP mint (locked for ever).
    pub lp_holding: Pubkey,
    /// Creation time.
    pub created_at: i64,
    /// Graduation time (0 until then).
    pub graduated_at: i64,
    /// Creator fees taken so far.
    pub creator_fees_accrued: u64,
    /// Creator fees claimed so far.
    pub creator_fees_claimed: u64,
    /// Base added to the pool at graduation.
    pub graduation_topup: u64,
    /// Base burned at graduation.
    pub graduation_burned: u64,
    /// The token rules, fixed at launch.
    pub rules: LaunchRules,
    /// The kit modules of `rules` (0: no kit, no token hook).
    pub modules: u8,
    /// `PDA(["kit", mint], KIT_ID)`, derived whatever the modules (the pool hook's fixed key).
    pub kit_config: Pubkey,
    /// `holding(quote_mint, kit_config)`, derived whatever the modules: the kit's reward vault
    /// when holder rewards are on, where holder fees go.
    pub holder_vault: Pubkey,
    /// Bump of `PDA(["kit-caller", mint], LAUNCH_ID)`, which signs the kit's `init` and
    /// `graduate`; 0 without a kit.
    pub kit_caller_bump: u8,
    /// The creator wallet unlocks at this time; 0 when off.
    pub creator_unlock_at: i64,
    /// Buys from the pool before this time are locked; 0 when off.
    pub early_window_end: i64,
    /// ... until this time; 0 when off.
    pub early_unlock_at: i64,
    /// The creator's own first buy took the normal LP fee inside the sniper window (once).
    pub creator_bought: bool,
    /// Holder fees taken so far (bridged SOL, into the holder vault).
    pub holder_fees_accrued: u64,
    /// Tokens burned on trades (both sides; not the graduation burn).
    pub burned_on_trades: u64,
    /// The `LaunchConfig` the launch was made from; the default key for inline rules.
    pub config: Pubkey,
    /// The creator's own token hook (from the config), when the token has one. It runs on every
    /// transfer, mint and burn its flags name; nobody vets its code.
    pub custom_hook: Option<Pubkey>,
    /// The custom hook's flags; 0 without one.
    pub custom_hook_flags: u16,
    /// The config author's share of the creator fee (basis points of it), from a listed config
    /// whose author is not the launch's creator; 0 otherwise. Fixed at launch: every claim pays
    /// the author this part of what it takes.
    pub author_share_bps: u16,
    /// Creator fees paid to the config's author so far (quote).
    pub author_fees_paid: u64,
    /// Reserved.
    pub reserved: [u8; 22],
}

impl Launch {
    /// Account size.
    pub const LEN: usize = DISCRIMINATOR_LEN + Self::INIT_SPACE;

    /// The launch address of `mint`.
    pub fn address(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[LAUNCH_SEED, mint.as_ref()], &crate::ID)
    }

    /// Whether the token has a kit (any kit module).
    pub fn has_kit(&self) -> bool {
        self.modules != 0
    }

    /// Whether the token's hook is the creator's own program.
    pub fn has_custom_hook(&self) -> bool {
        self.custom_hook.is_some()
    }

    /// The token's hook program: the creator's own, the kit, or none.
    pub fn token_hook(&self) -> Option<Pubkey> {
        self.custom_hook
            .or_else(|| self.has_kit().then_some(crate::constants::KIT_ID))
    }

    /// Whether holder rewards are on.
    pub fn rewards_on(&self) -> bool {
        self.modules & modules::HOLDER_REWARDS != 0
    }
}
