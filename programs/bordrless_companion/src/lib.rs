//! `bordrless_companion`: a launch whose creator is a program (`docs/companions.md`).
//!
//! A companion is made for a mint before it exists (`create`), then creates the launch through the
//! launchpad with its creator address, `PDA(["creator", mint])`, as the launch's creator (`launch`).
//! Every creator fee then lands with the companion, and its code alone decides what happens to it:
//! bought back and burned, streamed to holders through the kit, or paid to the launcher, by the
//! split fixed at `create`. The launcher's own buy (`dev_buy`) is made by the companion and vests
//! to them (`release`). Every step but `dev_buy` is permissionless and pays its sender a bounty:
//! the token runs its rewards with no keeper and no backend to trust.
//!
//! Layout: [`state`], [`instructions`] (create and launch, the steps), [`invoke`] (how a step calls
//! another program), [`events`], [`error`], [`client`].

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use bordrless_launch::instructions::CreateLaunchArgs;

pub mod client;
pub mod constants;
pub mod error;
pub mod events;
pub mod instructions;
pub mod invoke;
pub mod state;

pub use instructions::*;

declare_id!("6ZUM1gWBH9hBBNoJoaVAGwSftyZ6CUda6vUZTW9MsJuo");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Bordrless companion",
    project_url: "https://github.com/BordrlessDex/bordrless-programs",
    contacts: "link:https://github.com/BordrlessDex/bordrless-programs/security/advisories/new",
    policy: "https://github.com/BordrlessDex/bordrless-programs/blob/main/SECURITY.md",
    source_code: "https://github.com/BordrlessDex/bordrless-programs"
}

#[program]
pub mod bordrless_companion {
    use super::*;

    /// Makes a companion for `mint` (not created yet): the split, the bounty, the buyback limits,
    /// the vesting; funds its creator address and creates its bridged-SOL holding.
    pub fn create(ctx: Context<Create>, args: CreateArgs) -> Result<()> {
        instructions::create::process_create(ctx, args)
    }

    /// Creates the launch through the launchpad, the creator address signing as its creator. The
    /// remaining accounts are `create_launch`'s.
    pub fn launch<'info>(
        ctx: Context<'info, LaunchIt<'info>>,
        args: CreateLaunchArgs,
    ) -> Result<()> {
        instructions::create::process_launch(ctx, args)
    }

    /// The beneficiary's buy, held by the companion and vesting to them.
    pub fn dev_buy<'info>(
        ctx: Context<'info, DevBuy<'info>>,
        lamports: u64,
        min_out: u64,
    ) -> Result<()> {
        instructions::steps::process_dev_buy(ctx, lamports, min_out)
    }

    /// Anyone: claims the creator fees and splits them.
    pub fn claim_fees<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
        instructions::steps::process_claim_fees(ctx)
    }

    /// Anyone: buys the token back with the pending buyback (capped, spaced) and burns it.
    pub fn buyback<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
        instructions::steps::process_buyback(ctx)
    }

    /// Anyone: streams the holders' part to holders through the kit.
    pub fn share<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
        instructions::steps::process_share(ctx)
    }

    /// Anyone: pays the beneficiary their part as SOL.
    pub fn withdraw<'info>(ctx: Context<'info, Withdraw<'info>>) -> Result<()> {
        instructions::steps::process_withdraw(ctx)
    }

    /// Anyone: sends the beneficiary the dev bag's vested tokens.
    pub fn release<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
        instructions::steps::process_release(ctx)
    }
}
