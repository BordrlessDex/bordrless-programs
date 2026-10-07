//! `share`: anyone sends reward-mint lamports that every holder receives pro rata, released
//! linearly over one hour of eligible holding so nobody can buy just before a share and sell just
//! after it (`docs/hooks-v2.md` §4.10, with the review fixes). The hour starts at once, or, while
//! an earlier share is still streaming, when that one ends: a later share never stretches an
//! earlier one, and never takes on its rate.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::KitError;
use crate::events::RewardsShared;
use crate::instructions::callbacks::holding_amount;
use crate::state::KitConfig;

/// Accounts of `share`.
#[event_cpi]
#[derive(Accounts)]
pub struct Share<'info> {
    /// Shares from `source`, as its owner or delegate (the token program checks).
    pub sharer: Signer<'info>,
    /// The token's config, with holder rewards on (checked before the vault).
    #[account(mut, constraint = kit_config.rewards_on() @ KitError::RewardsOff)]
    pub kit_config: Account<'info, KitConfig>,
    /// CHECK: a holding of the reward mint the sharer may spend (the token program checks the
    /// mint, the authority and the balance).
    #[account(mut)]
    pub source: UncheckedAccount<'info>,
    /// CHECK: the config's reward mint (address-checked).
    #[account(address = kit_config.reward_mint @ KitError::WrongRewardMint)]
    pub reward_mint: UncheckedAccount<'info>,
    /// CHECK: the config's reward vault (address-checked; its balance read in the handler, owner
    /// and discriminator checked).
    #[account(mut, address = kit_config.reward_vault @ KitError::WrongRewardVault)]
    pub reward_vault: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = TOKEN_ID @ KitError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    #[account(address = TOKEN_EVENT_AUTHORITY @ KitError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
}

/// `share(amount)`: at least 0.001 SOL, and only while holders hold at least `min_eligible`.
/// Sync; move `amount` into the vault; count it as seen (so the next sync does not take it for
/// fresh) and stream it at its own rate, `amount / 3_600` a second, over the next hour, or over
/// the hour after the running stream when one runs ([`KitConfig::add_share`]): nothing already
/// streaming ends later or earlier.
pub fn process_share(ctx: Context<Share>, amount: u64) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let before = holding_amount(&ctx.accounts.reward_vault)?;
    {
        let config = &mut ctx.accounts.kit_config;
        require!(amount >= MIN_SHARE_LAMPORTS, KitError::ShareTooSmall);
        require!(config.divides(), KitError::NoEligibleHolders);
        config.sync(before, now)?;
    }

    // The reward mint has no hook: the token program's id stands for its hook program and hook
    // signer.
    let ix = token_client::transfer(
        ctx.accounts.sharer.key(),
        ctx.accounts.source.key(),
        ctx.accounts.reward_vault.key(),
        ctx.accounts.reward_mint.key(),
        None,
        vec![],
        amount,
    );
    let token_program = ctx.accounts.token_program.to_account_info();
    invoke(
        &ix,
        &[
            ctx.accounts.sharer.to_account_info(),
            ctx.accounts.source.to_account_info(),
            ctx.accounts.reward_vault.to_account_info(),
            ctx.accounts.reward_mint.to_account_info(),
            token_program.clone(),
            token_program.clone(),
            ctx.accounts.token_event_authority.to_account_info(),
            token_program,
        ],
    )?;
    // What arrived (the reward mint has no hook, so all of it).
    let received = holding_amount(&ctx.accounts.reward_vault)?
        .checked_sub(before)
        .ok_or(KitError::MathOverflow)?;

    let config = &mut ctx.accounts.kit_config;
    config.add_share(received, now)?;
    let total_shared = config.total_shared;
    let mint = config.mint;
    emit_cpi!(RewardsShared {
        mint,
        from: ctx.accounts.sharer.key(),
        amount: received,
        total_shared,
    });
    Ok(())
}
