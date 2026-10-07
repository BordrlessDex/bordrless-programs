//! Creator fee claims.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::LaunchError;
use crate::events::CreatorFeesClaimed;
use crate::instructions::launch::LaunchSeeds;
use crate::state::*;

/// Accounts of `claim_creator_fees`.
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimCreatorFees<'info> {
    /// The creator.
    pub creator: Signer<'info>,
    #[account(mut, seeds = [LAUNCH_SEED, launch.mint.as_ref()], bump = launch.bump, has_one = creator @ LaunchError::NotCreator)]
    pub launch: Account<'info, Launch>,
    /// CHECK: address-checked.
    #[account(address = launch.quote_mint @ LaunchError::WrongHolding)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = launch.quote_holding @ LaunchError::WrongHolding)]
    pub launch_quote: UncheckedAccount<'info>,
    /// CHECK: the creator's holding of the quote (checked in the handler).
    #[account(mut)]
    pub creator_quote: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = bordrless_token::ID @ LaunchError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    pub token_event_authority: UncheckedAccount<'info>,
}

/// `claim_creator_fees`.
pub fn process_claim_creator_fees(ctx: Context<ClaimCreatorFees>) -> Result<()> {
    let clock = Clock::get()?;
    let launch_key = ctx.accounts.launch.key();
    let quote_mint = ctx.accounts.launch.quote_mint;
    require_keys_eq!(
        ctx.accounts.creator_quote.key(),
        token_client::holding_address(&quote_mint, ctx.accounts.creator.key),
        LaunchError::WrongHolding
    );
    require_keys_eq!(
        ctx.accounts.token_event_authority.key(),
        token_client::event_authority(),
        LaunchError::WrongProgram
    );
    let amount = token_client::read_holding(&ctx.accounts.launch_quote)?.amount;
    require!(amount > 0, LaunchError::NothingToClaim);
    let launch_seeds = LaunchSeeds::new(ctx.accounts.launch.mint, ctx.accounts.launch.bump);
    let seeds = launch_seeds.seeds();
    let ix = token_client::transfer(
        launch_key,
        ctx.accounts.launch_quote.key(),
        ctx.accounts.creator_quote.key(),
        quote_mint,
        None,
        vec![],
        amount,
    );
    // Bridged SOL has no hook: the token program's id stands for its hook program and signer.
    let token_program = ctx.accounts.token_program.to_account_info();
    invoke_signed(
        &ix,
        &[
            ctx.accounts.launch.to_account_info(),
            ctx.accounts.launch_quote.to_account_info(),
            ctx.accounts.creator_quote.to_account_info(),
            ctx.accounts.quote_mint.to_account_info(),
            token_program.clone(),
            token_program.clone(),
            ctx.accounts.token_event_authority.to_account_info(),
            token_program,
        ],
        &[&seeds],
    )?;
    let launch = &mut ctx.accounts.launch;
    launch.creator_fees_claimed = launch
        .creator_fees_claimed
        .checked_add(amount)
        .ok_or(LaunchError::MathOverflow)?;
    emit_cpi!(CreatorFeesClaimed {
        launch: launch_key,
        mint: launch.mint,
        creator: ctx.accounts.creator.key(),
        amount,
        claimed_total: launch.creator_fees_claimed,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}
