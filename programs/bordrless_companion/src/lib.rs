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
//! Games (v2): a companion may also run a game (`create_game`, before the launch): a lottery whose
//! tickets the coin's own token hook keeps under the game ticket standard (`bordrless-game`), whose
//! pot is a fourth part of every fee claim, and whose draws are verifiable (ORAO VRF, [`oracle`]).
//! The companion holds the pot and makes every payout; it only reads the hook, never calls it. The
//! protocol can cap or block an unaudited game hook (`set_hook_status`), never take a pot. A pot
//! that pays no prize for long is drawn from a lower floor, then retired to the buyback by anyone
//! (`retire`); a blocked hook's buyback that can't be spent is burned as SOL (`burn_stranded`): no
//! pot is locked for ever, and none is ever paid to anyone but a winner.
//!
//! Layout: [`state`], [`instructions`] (create and launch, the steps, the game), [`invoke`] (how a
//! step calls another program), [`oracle`], [`events`], [`error`], [`client`].

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use bordrless_launch::instructions::CreateLaunchArgs;

pub mod client;
pub mod constants;
pub mod error;
pub mod events;
pub mod instructions;
pub mod invoke;
pub mod oracle;
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

    // ---- Games (v2) ----

    /// Makes the companion's game for `mint` (not launched yet; the mint signs): the pot's part of
    /// the split, the hook (prepared for the mint), the rounds, the minimum pot, the prize, the
    /// claim windows.
    pub fn create_game(ctx: Context<CreateGame>, args: CreateGameArgs) -> Result<()> {
        instructions::game::process_create_game(ctx, args)
    }

    /// Anyone: once `round` is over, commits its draw's seed, made from `slot` (one of the last
    /// three slots), and asks the oracle for it, the pot paying (or adopts a pending request for
    /// it), in one instruction; or rolls the round over: no tickets, too late, or the pot unable to
    /// pay for the oracle's request.
    pub fn draw<'info>(ctx: Context<'info, GameStep<'info>>, round: u32, slot: u64) -> Result<()> {
        instructions::game::process_draw(ctx, round, slot)
    }

    /// Anyone: stores the oracle's answer; the claim windows start.
    pub fn reveal<'info>(ctx: Context<'info, GameStep<'info>>) -> Result<()> {
        instructions::game::process_reveal(ctx)
    }

    /// Anyone: pays the draw's winner for attempt `attempt`, during its window.
    pub fn claim_prize<'info>(ctx: Context<'info, ClaimPrize<'info>>, attempt: u8) -> Result<()> {
        instructions::game::process_claim_prize(ctx, attempt)
    }

    /// Anyone: rolls over a draw that can't go on (nobody won it, or its claims ended).
    pub fn expire<'info>(ctx: Context<'info, GameStep<'info>>) -> Result<()> {
        instructions::game::process_expire(ctx)
    }

    /// Anyone: sends the pot of a game that has paid no prize for two dormant periods to the
    /// buyback, paying nobody.
    pub fn retire<'info>(ctx: Context<'info, GameStep<'info>>) -> Result<()> {
        instructions::game::process_retire(ctx)
    }

    /// Anyone: burns as SOL a blocked game hook's buyback that no buyback has spent or waited on
    /// for 30 days (the pot with it), paying nobody.
    pub fn burn_stranded<'info>(ctx: Context<'info, BurnStranded<'info>>) -> Result<()> {
        instructions::game::process_burn_stranded(ctx)
    }

    /// The protocol's upgrade authority: whether a game hook is audited, its pots' cap while not,
    /// and whether it is blocked.
    pub fn set_hook_status(
        ctx: Context<SetHookStatus>,
        hook: Pubkey,
        args: HookStatusArgs,
    ) -> Result<()> {
        instructions::game::process_set_hook_status(ctx, hook, args)
    }
}
