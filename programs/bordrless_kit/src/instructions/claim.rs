//! `claim`: a holder takes the rewards it is owed, in the reward mint (`docs/hooks-v2.md` §4.10).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use bordrless_hook::HOOK_AUTHORITY_SEED;
use bordrless_token::client as token_client;
use bordrless_token::state::Holding;

use crate::constants::*;
use crate::error::KitError;
use crate::events::RewardsClaimed;
use crate::instructions::callbacks::holding_amount;
use crate::math::settle;
use crate::state::{HolderData, KitConfig};

/// Accounts of `claim`.
#[event_cpi]
#[derive(Accounts)]
pub struct Claim<'info> {
    /// The holder: neither the pool nor the launch (checked in the handler).
    pub owner: Signer<'info>,
    /// The token's config, with holder rewards on (checked before the vault, which a token
    /// without holder rewards does not have).
    #[account(mut, constraint = kit_config.rewards_on() @ KitError::RewardsOff)]
    pub kit_config: Account<'info, KitConfig>,
    /// CHECK: the config's mint (address-checked; the token program checks it again in
    /// `write_hook_data`).
    #[account(address = kit_config.mint @ KitError::WrongMint)]
    pub mint: UncheckedAccount<'info>,
    /// The owner's holding of the token. Holdings exist only at `["holding", mint, owner]`, so
    /// the mint and the owner bind it.
    #[account(
        mut,
        constraint = holding.mint == kit_config.mint @ KitError::WrongHolding,
        constraint = holding.owner == owner.key() @ KitError::WrongHolding
    )]
    pub holding: Account<'info, Holding>,
    /// CHECK: the config's reward mint (address-checked).
    #[account(address = kit_config.reward_mint @ KitError::WrongRewardMint)]
    pub reward_mint: UncheckedAccount<'info>,
    /// CHECK: the config's reward vault (address-checked; its balance read in the handler, owner
    /// and discriminator checked).
    #[account(mut, address = kit_config.reward_vault @ KitError::WrongRewardVault)]
    pub reward_vault: UncheckedAccount<'info>,
    /// The owner's holding of the reward mint.
    #[account(
        mut,
        constraint = destination.mint == kit_config.reward_mint @ KitError::WrongDestination,
        constraint = destination.owner == owner.key() @ KitError::WrongDestination
    )]
    pub destination: Account<'info, Holding>,
    /// CHECK: this program's `["hook-authority"]` PDA, which signs `write_hook_data`.
    #[account(address = HOOK_AUTHORITY @ KitError::WrongProgram)]
    pub kit_hook_authority: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = TOKEN_ID @ KitError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    #[account(address = TOKEN_EVENT_AUTHORITY @ KitError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
}

/// `claim`: sync; settle the holder at its balance; pay `min(owed, vault)` (`NothingToClaim` when
/// that is 0) from the vault, the config signing as its owner; write the holder's bytes 0..24
/// through `write_hook_data`, keeping bytes 24..64 exactly as read (a claim never unlocks an
/// early buyer).
pub fn process_claim(ctx: Context<Claim>) -> Result<()> {
    let owner = ctx.accounts.owner.key();
    let now = Clock::get()?.unix_timestamp;
    let vault_amount = holding_amount(&ctx.accounts.reward_vault)?;
    let balance = ctx.accounts.holding.amount;
    let read = ctx.accounts.holding.hook_data;

    let config = &mut ctx.accounts.kit_config;
    require!(!config.is_excluded(&owner), KitError::NotAHolder);
    config.sync(vault_amount, now)?;
    let mut data = HolderData::read(&read);
    settle(&mut data, balance, balance, config.acc_per_share)?;
    let pay = data.owed.min(vault_amount);
    require!(pay > 0, KitError::NothingToClaim);
    data.owed -= pay;
    config.total_claimed = config
        .total_claimed
        .checked_add(pay)
        .ok_or(KitError::MathOverflow)?;
    let total_claimed = config.total_claimed;
    let mint = config.mint;
    let bump = [config.bump];

    // The pay, from the vault (the config owns it), in a mint without a hook (the token
    // program's id stands for its hook program and hook signer).
    let config_seeds: &[&[u8]] = &[KIT_SEED, mint.as_ref(), &bump];
    let ix = token_client::transfer(
        ctx.accounts.kit_config.key(),
        ctx.accounts.reward_vault.key(),
        ctx.accounts.destination.key(),
        ctx.accounts.reward_mint.key(),
        None,
        vec![],
        pay,
    );
    let token_program = ctx.accounts.token_program.to_account_info();
    invoke_signed(
        &ix,
        &[
            ctx.accounts.kit_config.to_account_info(),
            ctx.accounts.reward_vault.to_account_info(),
            ctx.accounts.destination.to_account_info(),
            ctx.accounts.reward_mint.to_account_info(),
            token_program.clone(),
            token_program.clone(),
            ctx.accounts.token_event_authority.to_account_info(),
            token_program,
        ],
        &[config_seeds],
    )?;

    // The holder's reward bytes; the rest as read.
    let ix = token_client::write_hook_data(
        HOOK_AUTHORITY,
        mint,
        ctx.accounts.holding.key(),
        data.with_rewards_of(&read),
    );
    invoke_signed(
        &ix,
        &[
            ctx.accounts.kit_hook_authority.to_account_info(),
            ctx.accounts.mint.to_account_info(),
            ctx.accounts.holding.to_account_info(),
            ctx.accounts.token_event_authority.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
        ],
        &[&[HOOK_AUTHORITY_SEED, &[HOOK_AUTHORITY_BUMP]]],
    )?;

    emit_cpi!(RewardsClaimed {
        mint,
        owner,
        amount: pay,
        owed_left: data.owed,
        total_claimed,
    });
    Ok(())
}
