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
//! A coin may instead be quoted in 1 to 6 real SPL Token or Token-2022 mints
//! (`create_token_launch`, `docs/token-quotes.md`): one leg pool per quote, opened by `open_leg`,
//! the creator fee taken in each leg's token and claimed per leg; such a coin never graduates.
//!
//! Layout: [`state`], [`instructions`] (config, launch configs, launch, graduate, claim, hook
//! callbacks), [`cpi`] (the instructions this program invokes), [`events`], [`error`], [`client`].

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use bordrless_hook::{HookReturn, PoolHookArgs};
use state::QuoteCut;

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

    /// Anyone: makes a `LaunchConfig` whose custom hook also takes a SOL cut on every trade
    /// (`docs/quote-cut.md`): the hook's `quote_cut` answers a rate, the pool hook takes at most
    /// `cut`'s cap (at most 1,000 basis points a side) of the trade's SOL with the creator fee.
    /// `author_share_bps` 0: as `create_config`; else as `create_listed_config`.
    pub fn create_quote_cut_config(
        ctx: Context<CreateConfig>,
        args: CreateConfigArgs,
        author_share_bps: u16,
        cut: QuoteCut,
    ) -> Result<()> {
        launch_config::process_create_quote_cut_config(ctx, args, author_share_bps, cut)
    }

    /// Anyone: makes a `LaunchConfig` with a partner (`docs/partner-fee.md`), Meteora-style: every
    /// launch made from it, whoever makes it, pays `partner` `partner_share_bps` of its creator fee
    /// (1 to 10,000 basis points of it), as SOL to the partner's holding, on every claim. It may
    /// also carry an author's share (`author_share_bps`, 0 to 5,000; at most 10,000 with the
    /// partner's) and a custom hook's SOL cut (`cut`, zeros for none). Fixed for ever.
    pub fn create_partner_config(
        ctx: Context<CreatePartnerConfig>,
        args: CreateConfigArgs,
        author_share_bps: u16,
        cut: QuoteCut,
        partner: Pubkey,
        partner_share_bps: u16,
    ) -> Result<()> {
        launch_config::process_create_partner_config(
            ctx,
            args,
            author_share_bps,
            cut,
            partner,
            partner_share_bps,
        )
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
    pub fn claim_author_fees<'info>(ctx: Context<'info, ClaimAuthorFees<'info>>) -> Result<()> {
        claim::process_claim_author_fees(ctx)
    }

    /// Anyone: pays out the creator fees of a launch made from a partner config
    /// (`docs/partner-fee.md`), every party at once (the partner, the config's author when the
    /// launch pays one, the creator), each to its own fixed holding of the quote.
    pub fn distribute_fees<'info>(ctx: Context<'info, DistributeFees<'info>>) -> Result<()> {
        claim::process_distribute_fees(ctx)
    }

    /// `claim_creator_fees` for a native launch: its creator fees paid in SOL from the launch
    /// account to the creator's wallet (and the author's and the partner's parts to theirs).
    pub fn claim_creator_fees_sol<'info>(
        ctx: Context<'info, ClaimCreatorFeesSol<'info>>,
    ) -> Result<()> {
        claim::process_claim_creator_fees_sol(ctx)
    }

    /// `claim_author_fees` for a native launch, in SOL.
    pub fn claim_author_fees_sol<'info>(
        ctx: Context<'info, ClaimAuthorFeesSol<'info>>,
    ) -> Result<()> {
        claim::process_claim_author_fees_sol(ctx)
    }

    /// `distribute_fees` for a native launch, in SOL.
    pub fn distribute_fees_sol<'info>(ctx: Context<'info, DistributeFeesSol<'info>>) -> Result<()> {
        claim::process_distribute_fees_sol(ctx)
    }

    /// Admin: whether launches made from now on are native.
    pub fn set_native_launches(ctx: Context<SetNativeLaunches>, on: bool) -> Result<()> {
        native::process_set_native_launches(ctx, on)
    }

    /// Admin, with the DEX paused and the launch's pool native: turns a launch native (its
    /// creator fees and its kit's rewards to lamports). Idempotent.
    pub fn migrate_launch_to_native(ctx: Context<LaunchForm>) -> Result<()> {
        native::process_migrate_launch_to_native(ctx)
    }

    /// Admin, with the DEX paused: turns a native launch back to bridged SOL. Idempotent.
    pub fn revert_launch_to_bridged(ctx: Context<LaunchForm>) -> Result<()> {
        native::process_revert_launch_to_bridged(ctx)
    }

    /// Pool hook: refuses pools with this hook that this program did not create.
    pub fn before_initialize(
        ctx: Context<RejectInitialize>,
        args: PoolHookArgs,
    ) -> Result<HookReturn> {
        hooks::process_before_initialize(ctx, args)
    }

    /// Pool hook: the anti-sniper LP fee; a buy's creator and holder fees (and its custom hook's
    /// SOL cut), a sell's burn. A buy past the end of the curve is filled: the answer, set as the
    /// return data (`bordrless_hook::set_answer`), ends with the part of the input the swap uses.
    pub fn before_swap<'info>(
        ctx: Context<'info, HookCallback<'info>>,
        args: PoolHookArgs,
    ) -> Result<()> {
        hooks::process_before_swap(ctx, args)
    }

    /// Pool hook: a buy's burn; a sell's creator and holder fees (and its custom hook's SOL cut),
    /// from the output the DEX hands on.
    pub fn after_swap<'info>(
        ctx: Context<'info, HookCallback<'info>>,
        args: PoolHookArgs,
    ) -> Result<HookReturn> {
        hooks::process_after_swap(ctx, args)
    }

    /// Creates a coin quoted in 1 to 6 SPL Token or Token-2022 mints (`docs/token-quotes.md`
    /// §2.2): the mint, the whole supply to the launch, the `Launch` and its `Legs`. The remaining
    /// accounts are the quote mints, in leg order. Each leg then opens with `open_leg`. With rules
    /// (§6.1: the burn, max wallet, the creator wallet lock, the early-buyer lock; no holder fee
    /// yet) as `create_launch` takes them; when they install the kit, the mint names it as its hook
    /// and the kit's six accounts follow the quote mints. Data without rules reads as none.
    pub fn create_token_launch<'info>(
        ctx: Context<'info, CreateTokenLaunch<'info>>,
        args: CreateTokenLaunchArgs,
    ) -> Result<()> {
        token_launch::process_create_token_launch(ctx, args)
    }

    /// Anyone: opens leg `index` of a launch quoted in tokens: its creator-fee account, its pool
    /// on the DEX (this program as the hook, the leg's share deposited) and the pool's registry.
    /// Trading starts once every leg is open. With kit rules, the remaining accounts are the kit,
    /// the token program's signer of its callbacks and the kit config.
    pub fn open_leg<'info>(ctx: Context<'info, OpenLeg<'info>>, index: u8) -> Result<()> {
        token_launch::process_open_leg(ctx, index)
    }

    /// Anyone: pays leg `index`'s creator fees to the creator's associated token account of the
    /// leg's quote (created if needed).
    pub fn claim_creator_fees_token<'info>(
        ctx: Context<'info, ClaimCreatorFeesToken<'info>>,
        index: u8,
    ) -> Result<()> {
        token_launch::process_claim_creator_fees_token(ctx, index)
    }
}
