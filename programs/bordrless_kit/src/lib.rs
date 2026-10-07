//! `bordrless_kit`: the token hook a Bordrless launch installs for the rules it fixes at launch
//! (`docs/hooks-v2.md` §4).
//!
//! Four fixed, bounded modules ([`constants::modules`]): holder rewards (a share of each trade on
//! the launch pool, taken by the launch's pool hook, paid to holders in SOL through an exact
//! reward accounting kept in 64 bytes of each holding, claimable any time, plus shares streamed
//! over an hour), max wallet until graduation, the creator wallet lock and the early-buyer lock.
//!
//! Only the launchpad installs it: [`KitConfig`] is created only by `init`, which the launch
//! program's `["kit-caller", mint]` PDA must sign ([`constants::LAUNCH_ID`] is a constant here;
//! there is no crate dependency on the launch). A kit mint names this program as its hook with no
//! hook authority, so which program runs is fixed from the token's first instruction. A mint that
//! names the kit without a `KitConfig` can never move: every callback fails.
//!
//! The callbacks (`before_transfer`, `before_burn`) are signed by the token program's signer for
//! the kit, `["hook-authority", KIT_ID]` under the token program (a constant here, and not one any
//! other hook ever receives), bind every account they read (§4.6), make no CPI and emit no event;
//! their rules are pure functions in [`rules`], the reward math in [`math`]. `claim` and `share`
//! call the token program for the reward mint, which has no hook. [`mirror`] is the off-chain
//! mirror of what a holder can claim. [`client`] has the addresses, the flags of a module set and
//! the instruction builders the launch, the tests and the SDK use.

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use bordrless_hook::{HookReturn, TokenHookArgs};

pub mod client;
pub mod constants;
pub mod error;
pub mod events;
pub mod instructions;
pub mod math;
pub mod mirror;
pub mod rules;
pub mod state;
#[cfg(test)]
mod unit_tests;

pub use constants::{mint_flags, modules, LAUNCH_ID};
pub use instructions::*;
pub use state::{HolderData, KitConfig, KitInitArgs};

declare_id!("14RJQXPdJfkehit6ezktjd3xujamf8nVSKw2shKamaEH");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Bordrless kit",
    project_url: "https://github.com/BordrlessDex/bordrless-programs",
    contacts: "link:https://github.com/BordrlessDex/bordrless-programs/security/advisories/new",
    policy: "https://github.com/BordrlessDex/bordrless-programs/blob/main/SECURITY.md",
    source_code: "https://github.com/BordrlessDex/bordrless-programs"
}

/// This program's id.
pub const KIT_ID: Pubkey = ID;

/// Instructions of the kit.
#[program]
pub mod bordrless_kit {
    use super::*;

    /// Installs the kit on a launch's mint: creates its `KitConfig`, the reward vault (holder
    /// rewards) and the registry. Signed by the launch program's kit-caller PDA for the mint.
    pub fn init(ctx: Context<Init>, args: KitInitArgs) -> Result<()> {
        init::process_init(ctx, args)
    }

    /// The launch graduated: lifts max wallet. Signed by the kit-caller PDA; once.
    pub fn graduate(ctx: Context<Graduate>) -> Result<()> {
        graduate::process_graduate(ctx)
    }

    /// A holder claims the rewards it is owed.
    pub fn claim(ctx: Context<Claim>) -> Result<()> {
        claim::process_claim(ctx)
    }

    /// Shares `amount` of the reward mint with the holders, released over one hour: the next
    /// one, or the one after the share still streaming.
    pub fn share(ctx: Context<Share>, amount: u64) -> Result<()> {
        share::process_share(ctx, amount)
    }

    /// Token hook: every transfer of a kit mint.
    pub fn before_transfer(ctx: Context<Callback>, args: TokenHookArgs) -> Result<HookReturn> {
        callbacks::process_before_transfer(ctx, args)
    }

    /// Token hook: every burn of a kit mint with holder rewards or the early-buyer lock.
    pub fn before_burn(ctx: Context<Callback>, args: TokenHookArgs) -> Result<HookReturn> {
        callbacks::process_before_burn(ctx, args)
    }
}
