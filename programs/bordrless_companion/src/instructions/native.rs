//! Native-SOL companions (phase 3): a companion whose SOL is its creator address's lamports
//! instead of bridged SOL in the creator's holding.
//!
//! Every bucket (`pending_buyback`, `pending_holders`, `pending_beneficiary`, `pending_pot`) keeps
//! its meaning: a count of the SOL the creator holds. A bridged companion holds it as bridged SOL
//! in its holding; a native one as its creator address's lamports above the rent-exempt minimum of
//! an account with no data ([`crate::instructions::steps::sol_held`]). Its fee claims take the
//! launch's fees as lamports (`claim_creator_fees_sol`), its trades are the DEX's `swap_v2` with
//! the creator address as trader, its holders' share is the kit's `share_sol`, and every payout is
//! a plain transfer from the creator address (no unwrap). A wallet payout to a holding of bridged
//! SOL (an old-style destination) is wrapped into it by the creator address, which, a system
//! account with no data that this program signs for, is the bridge's `wrap_sol` user itself; a
//! bridged companion pays it in bridged SOL from its holding.
//!
//! The spare lamports a bridged creator address holds (what its launch did not spend of its
//! funding, which `withdraw` pays the beneficiary) would read as SOL once native: turning native
//! books them to `pending_beneficiary`, so the beneficiary is paid them and no claim splits them.
//!
//! `migrate_companion_to_native` and `revert_companion_to_bridged` are signed by the companion
//! program's upgrade authority, only while the DEX is paused, and are idempotent. Migrating needs
//! the companion's launch native (its pool and launch first); reverting comes before the launch's.
//! A companion launched while native launches are on is native from its launch.

use anchor_lang::prelude::*;
use bordrless_launch::client as launch_client;
use bordrless_launch::state::Launch;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::CompanionNativeSet;
use crate::instructions::game::upgrade_authority;
use crate::instructions::CreatorSeeds;
use crate::invoke::invoke_built;
use crate::state::*;

/// Accounts of `migrate_companion_to_native` and `revert_companion_to_bridged`. Remaining: the
/// bridge's `wrap_sol`/`unwrap_sol` accounts for the creator address
/// (`bordrless_bridge::client::unwrap_sol(creator, _)`'s).
#[event_cpi]
#[derive(Accounts)]
pub struct CompanionForm<'info> {
    /// The companion program's upgrade authority; pays a byte's rent when the companion grows.
    #[account(mut)]
    pub admin: Signer<'info>,
    /// CHECK: this program's ProgramData (parsed by `upgrade_authority`).
    pub program_data: UncheckedAccount<'info>,
    /// CHECK: the DEX config (address-checked), read for its pause.
    #[account(address = bordrless_swap::client::config_address())]
    pub dex_config: UncheckedAccount<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    #[account(address = launch_client::launch_address(&companion.mint))]
    pub launch: Box<Account<'info, Launch>>,
    pub system_program: Program<'info, System>,
}

fn check_form(a: &CompanionForm) -> Result<()> {
    require!(
        upgrade_authority(&a.program_data)? == Some(a.admin.key()),
        CompanionError::NotProtocolAuthority
    );
    let data = a.dex_config.try_borrow_data()?;
    let dex = bordrless_swap::state::Config::try_deserialize(&mut &data[..])?;
    require!(dex.paused, CompanionError::DexNotPaused);
    Ok(())
}

/// Turns `c` native: the creator's spare lamports booked to the beneficiary's part (answered),
/// the flag set. The caller has grown the account and moved the bridged SOL onto the creator.
fn turn_native(c: &mut Companion, spare: u64) -> Result<()> {
    c.pending_beneficiary = c
        .pending_beneficiary
        .checked_add(spare)
        .ok_or(CompanionError::MathOverflow)?;
    c.native_sol.on = true;
    Ok(())
}

/// The creator address's lamports above the rent-exempt minimum of an account with no data.
fn spare_of(creator: &AccountInfo) -> Result<u64> {
    Ok(creator
        .lamports()
        .saturating_sub(Rent::get()?.minimum_balance(0)))
}

/// A companion launched while native launches are on: native from its launch (the creator address
/// pays the byte; its spare lamports booked to the beneficiary). Called by `launch` once the
/// launchpad made the launch native. Answers the spare booked.
pub(crate) fn native_at_launch<'info>(
    companion: &mut Account<'info, Companion>,
    creator: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
) -> Result<u64> {
    let info = companion.to_account_info();
    let seeds = CreatorSeeds::of(companion);
    companion.grow_for_native(&info, creator, system_program, &[&seeds.seeds()])?;
    let spare = spare_of(creator)?;
    turn_native(companion, spare)?;
    Ok(spare)
}

/// `migrate_companion_to_native`.
pub fn process_migrate_companion_to_native<'info>(
    ctx: Context<'info, CompanionForm<'info>>,
) -> Result<()> {
    check_form(&ctx.accounts)?;
    if ctx.accounts.companion.is_native() {
        return Ok(());
    }
    require!(
        ctx.accounts.launch.is_native(),
        CompanionError::LaunchNotNative
    );
    let info = ctx.accounts.companion.to_account_info();
    let admin = ctx.accounts.admin.to_account_info();
    let system = ctx.accounts.system_program.to_account_info();
    // Grown for `quote_cut_claimed` first (as its next claim would: every cut so far claimed),
    // then for the flag; the admin pays both.
    let accrued = ctx.accounts.launch.quote_cut_accrued();
    ctx.accounts
        .companion
        .grow_for_quote_cut(&info, &admin, &system, accrued)?;
    ctx.accounts
        .companion
        .grow_for_native(&info, &admin, &system, &[])?;
    let creator = ctx.accounts.creator.to_account_info();
    let spare = spare_of(&creator)?;
    let mut all = ctx.accounts.to_account_infos();
    all.extend_from_slice(ctx.remaining_accounts);
    let holding = crate::instructions::steps::balance(&all, &BRIDGED_SOL_MINT, creator.key)?;
    let seeds = CreatorSeeds::of(&ctx.accounts.companion);
    if holding > 0 {
        invoke_built(
            &bordrless_bridge::client::unwrap_sol(*creator.key, holding),
            &all,
            &[&seeds.seeds()],
        )?;
    }
    let c = &mut ctx.accounts.companion;
    turn_native(c, spare)?;
    emit_cpi!(CompanionNativeSet {
        companion: c.key(),
        native: true,
        moved: holding,
        spare,
    });
    Ok(())
}

/// `revert_companion_to_bridged`: the creator address's SOL (its lamports above the minimum)
/// wrapped back into its holding by the creator address itself; the flag cleared.
pub fn process_revert_companion_to_bridged<'info>(
    ctx: Context<'info, CompanionForm<'info>>,
) -> Result<()> {
    check_form(&ctx.accounts)?;
    if !ctx.accounts.companion.is_native() {
        return Ok(());
    }
    let creator = ctx.accounts.creator.to_account_info();
    let moved = spare_of(&creator)?;
    let mut all = ctx.accounts.to_account_infos();
    all.extend_from_slice(ctx.remaining_accounts);
    let seeds = CreatorSeeds::of(&ctx.accounts.companion);
    if moved > 0 {
        let holding = token_client::holding_address(&BRIDGED_SOL_MINT, creator.key);
        require!(
            all.iter().any(|a| *a.key == holding),
            CompanionError::MissingAccount
        );
        invoke_built(
            &bordrless_bridge::client::wrap_sol(*creator.key, moved),
            &all,
            &[&seeds.seeds()],
        )?;
    }
    let c = &mut ctx.accounts.companion;
    c.native_sol.on = false;
    emit_cpi!(CompanionNativeSet {
        companion: c.key(),
        native: false,
        moved,
        spare: 0,
    });
    Ok(())
}
