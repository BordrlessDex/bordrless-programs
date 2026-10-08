//! `bordrless_launch`: the Bordrless launchpad (`docs/hooks-v2.md` §5).
//!
//! One transaction creates a Bordrless Token Standard mint with fixed metadata and the token rules
//! the creator chose ([`state::LaunchRules`], within the config's bounds, given inline or through
//! a [`state::LaunchConfig`] made with the SDK), mints the supply to the launch PDA, revokes the
//! mint authority, installs the token's hook when the rules need one (the kit, `bordrless_kit`,
//! for holder rewards, max wallet, the creator wallet lock and the early-buyer lock; or, from a
//! config, the creator's own hook program, prepared for the mint beforehand), and creates a curve
//! pool on the Bordrless DEX with this program as the pool's hook and most of the supply
//! deposited. Trading is then ordinary DEX swaps; this program's `before_swap` and `after_swap`
//! callbacks apply the anti-sniper LP fee and take the creator fee, the holder fee (into the kit's
//! reward vault) and the burn; the DEX takes Bordrless's share of what they collect. Once the
//! pool's quote reserve reaches the graduation threshold, anyone calls `graduate`: the reserve
//! tops the pool up so the price is continuous, the rest is burned, the DEX finalizes the curve,
//! the LP lands in the launch PDA, which has no instruction to move it, and the kit is told (max
//! wallet lifts).
//!
//! The kit is called only through this program's `["kit-caller", mint]` PDA, which signs nothing
//! else; the launch PDA and this program's hook authority are never passed to it.
//!
//! Layout: [`state`], [`instructions`] (config, launch configs, launch, graduate, claim, hook
//! callbacks), [`cpi`] (the instructions this program invokes), [`events`], [`error`], [`client`].

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use bordrless_hook::{HookReturn, PoolHookArgs};

pub mod client;
pub mod constants;
pub mod cpi;
pub mod error;
pub mod events;
pub mod instructions;
pub mod state;

pub use instructions::*;

declare_id!("1jcBymHxBjniZDhNPy51Vgm5Nz7pLUdxa9UBHc4TavC");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Bordrless launchpad",
    project_url: "https://github.com/BordrlessDex/bordrless-programs",
    contacts: "link:https://github.com/BordrlessDex/bordrless-programs/security/advisories/new",
    policy: "https://github.com/BordrlessDex/bordrless-programs/blob/main/SECURITY.md",
    source_code: "https://github.com/BordrlessDex/bordrless-programs"
}

/// Instructions of the launchpad.
#[program]
pub mod bordrless_launch {
    use super::*;

    /// Creates the config; signed by the program's upgrade authority, once.
    pub fn init_config(ctx: Context<InitConfig>, args: ConfigArgs) -> Result<()> {
        config::process_init_config(ctx, args)
    }

    /// Admin: changes the config. Launches keep the terms they were created with.
    pub fn set_config(ctx: Context<SetConfig>, args: ConfigArgs) -> Result<()> {
        config::process_set_config(ctx, args)
    }

    /// Anyone: makes a `LaunchConfig` (rules, creator fee, optionally a custom token hook),
    /// fixed once created, for launches to use by its key.
    pub fn create_config(ctx: Context<CreateConfig>, args: CreateConfigArgs) -> Result<()> {
        launch_config::process_create_config(ctx, args)
    }

    /// Anyone: makes a `LaunchConfig` for the marketplace: as `create_config`, plus the author's
    /// share of the creator fee (1 to 5,000 basis points of it) on every launch someone else makes
    /// from it, fixed for ever.
    pub fn create_listed_config(
        ctx: Context<CreateConfig>,
        args: CreateConfigArgs,
        author_share_bps: u16,
    ) -> Result<()> {
        launch_config::process_create_listed_config(ctx, args, author_share_bps)
    }

    /// Creates a token and its curve pool, from the inline rules or from a `LaunchConfig`. With
    /// a custom hook, the remaining accounts are the hook, the token program's signer for it,
    /// its registry for the mint and the registry's extras.
    pub fn create_launch<'info>(
        ctx: Context<'info, CreateLaunch<'info>>,
        args: CreateLaunchArgs,
    ) -> Result<()> {
        launch::process_create_launch(ctx, args)
    }

    /// Graduates a launch whose pool has raised the threshold. Permissionless. With a custom
    /// hook, the remaining accounts are the hook, the token program's signer for it and the
    /// registry's extras.
    pub fn graduate<'info>(ctx: Context<'info, Graduate<'info>>) -> Result<()> {
        graduate::process_graduate(ctx)
    }

    /// The creator takes the creator fees collected so far.
    pub fn claim_creator_fees<'info>(ctx: Context<'info, ClaimCreatorFees<'info>>) -> Result<()> {
        claim::process_claim_creator_fees(ctx)
    }

    /// A listed config's author takes their share of a launch's creator fees; the creator's part
    /// is paid at the same time, as on the creator's own claim.
    pub fn claim_author_fees(ctx: Context<ClaimAuthorFees>) -> Result<()> {
        claim::process_claim_author_fees(ctx)
    }

    /// Pool hook: refuses pools with this hook that this program did not create.
    pub fn before_initialize(
        ctx: Context<RejectInitialize>,
        args: PoolHookArgs,
    ) -> Result<HookReturn> {
        hooks::process_before_initialize(ctx, args)
    }

    /// Pool hook: the anti-sniper LP fee; a buy's creator and holder fees, a sell's burn.
    pub fn before_swap(ctx: Context<HookCallback>, args: PoolHookArgs) -> Result<HookReturn> {
        hooks::process_before_swap(ctx, args)
    }

    /// Pool hook: a buy's burn; a sell's creator and holder fees, from the output the DEX hands
    /// on.
    pub fn after_swap(ctx: Context<HookCallback>, args: PoolHookArgs) -> Result<HookReturn> {
        hooks::process_after_swap(ctx, args)
    }
}
