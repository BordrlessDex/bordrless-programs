//! Wrapping and unwrapping.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::system_program;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::BridgeError;
use crate::events::*;
use crate::spl;
use crate::state::*;

/// The Bordrless token program's fixed accounts. A wrapped mint has no hook, so its token
/// instructions take no hook program and no hook signer (the token program's id stands for
/// both).
fn check_token(program: &AccountInfo, event_authority: &AccountInfo) -> Result<()> {
    require_keys_eq!(
        *program.key,
        bordrless_token::ID,
        BridgeError::WrongTokenProgram
    );
    require_keys_eq!(
        *event_authority.key,
        token_client::event_authority(),
        BridgeError::WrongTokenProgram
    );
    Ok(())
}

fn mint_wrapped<'info>(
    wrapper: &Account<'info, Wrapper>,
    wrapped_mint: &AccountInfo<'info>,
    destination: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    token_event_authority: &AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    let ix = token_client::mint_to(
        wrapper.key(),
        *wrapped_mint.key,
        *destination.key,
        None,
        vec![],
        amount,
    );
    let seeds: &[&[u8]] = &[
        WRAPPER_SEED,
        wrapper.underlying_mint.as_ref(),
        &[wrapper.bump],
    ];
    invoke_signed(
        &ix,
        &[
            wrapper.to_account_info(),
            wrapped_mint.clone(),
            destination.clone(),
            token_program.clone(),
            token_program.clone(),
            token_event_authority.clone(),
            token_program.clone(),
        ],
        &[seeds],
    )
    .map_err(Into::into)
}

fn burn_wrapped<'info>(
    user: &AccountInfo<'info>,
    source: &AccountInfo<'info>,
    wrapped_mint: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    token_event_authority: &AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    let ix = token_client::burn(
        *user.key,
        *source.key,
        *wrapped_mint.key,
        None,
        vec![],
        amount,
    );
    invoke_signed(
        &ix,
        &[
            user.clone(),
            source.clone(),
            wrapped_mint.clone(),
            token_program.clone(),
            token_program.clone(),
            token_event_authority.clone(),
            token_program.clone(),
        ],
        &[],
    )
    .map_err(Into::into)
}

/// Accounts of `wrap` and `unwrap`.
#[event_cpi]
#[derive(Accounts)]
pub struct Wrap<'info> {
    /// The user.
    pub user: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [WRAPPER_SEED, wrapper.underlying_mint.as_ref()], bump = wrapper.bump, constraint = !wrapper.native @ BridgeError::NativeWrapper)]
    pub wrapper: Account<'info, Wrapper>,
    /// CHECK: address-checked.
    #[account(address = wrapper.underlying_mint @ BridgeError::WrongTokenAccount)]
    pub underlying_mint: UncheckedAccount<'info>,
    /// CHECK: the user's token account of the underlying (checked in the handler).
    #[account(mut)]
    pub user_underlying: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = wrapper.vault @ BridgeError::WrongVault)]
    pub vault: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = wrapper.wrapped_mint @ BridgeError::WrongTokenAccount)]
    pub wrapped_mint: UncheckedAccount<'info>,
    /// CHECK: the user's holding of the wrapped mint (checked by the token program).
    #[account(mut)]
    pub user_wrapped: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(address = wrapper.underlying_program @ BridgeError::WrongTokenProgram)]
    pub underlying_program: UncheckedAccount<'info>,
    /// CHECK: the Bordrless token program.
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: its event authority.
    pub token_event_authority: UncheckedAccount<'info>,
}

/// `wrap`.
pub fn process_wrap(ctx: Context<Wrap>, amount: u64) -> Result<()> {
    let clock = Clock::get()?;
    require!(!ctx.accounts.config.paused, BridgeError::Paused);
    require!(amount > 0, BridgeError::ZeroAmount);
    check_token(
        &ctx.accounts.token_program,
        &ctx.accounts.token_event_authority,
    )?;
    let program = ctx.accounts.wrapper.underlying_program;
    spl::check_token_account(
        &ctx.accounts.user_underlying,
        &program,
        &ctx.accounts.wrapper.underlying_mint,
        None,
    )?;
    let before = spl::amount(&ctx.accounts.vault)?;
    spl::transfer_checked(
        &ctx.accounts.underlying_program.to_account_info(),
        &ctx.accounts.user_underlying.to_account_info(),
        &ctx.accounts.underlying_mint.to_account_info(),
        &ctx.accounts.vault.to_account_info(),
        &ctx.accounts.user.to_account_info(),
        amount,
        ctx.accounts.wrapper.decimals,
        &[],
    )?;
    let received = spl::amount(&ctx.accounts.vault)?
        .checked_sub(before)
        .ok_or(BridgeError::MathOverflow)?;
    require!(received > 0, BridgeError::NothingReceived);
    mint_wrapped(
        &ctx.accounts.wrapper,
        &ctx.accounts.wrapped_mint.to_account_info(),
        &ctx.accounts.user_wrapped.to_account_info(),
        &ctx.accounts.token_program.to_account_info(),
        &ctx.accounts.token_event_authority.to_account_info(),
        received,
    )?;
    let wrapper = &mut ctx.accounts.wrapper;
    wrapper.total_wrapped = wrapper
        .total_wrapped
        .checked_add(received)
        .ok_or(BridgeError::MathOverflow)?;
    emit_cpi!(Wrapped {
        wrapper: wrapper.key(),
        underlying_mint: wrapper.underlying_mint,
        wrapped_mint: wrapper.wrapped_mint,
        user: ctx.accounts.user.key(),
        amount_sent: amount,
        amount_minted: received,
        total_wrapped: wrapper.total_wrapped,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}

/// `unwrap`.
pub fn process_unwrap(ctx: Context<Wrap>, amount: u64) -> Result<()> {
    let clock = Clock::get()?;
    require!(amount > 0, BridgeError::ZeroAmount);
    check_token(
        &ctx.accounts.token_program,
        &ctx.accounts.token_event_authority,
    )?;
    let program = ctx.accounts.wrapper.underlying_program;
    spl::check_token_account(
        &ctx.accounts.user_underlying,
        &program,
        &ctx.accounts.wrapper.underlying_mint,
        None,
    )?;
    require!(
        spl::amount(&ctx.accounts.vault)? >= amount,
        BridgeError::InsufficientVault
    );
    burn_wrapped(
        &ctx.accounts.user.to_account_info(),
        &ctx.accounts.user_wrapped.to_account_info(),
        &ctx.accounts.wrapped_mint.to_account_info(),
        &ctx.accounts.token_program.to_account_info(),
        &ctx.accounts.token_event_authority.to_account_info(),
        amount,
    )?;
    let wrapper = &ctx.accounts.wrapper;
    let seeds: &[&[u8]] = &[
        WRAPPER_SEED,
        wrapper.underlying_mint.as_ref(),
        &[wrapper.bump],
    ];
    spl::transfer_checked(
        &ctx.accounts.underlying_program.to_account_info(),
        &ctx.accounts.vault.to_account_info(),
        &ctx.accounts.underlying_mint.to_account_info(),
        &ctx.accounts.user_underlying.to_account_info(),
        &wrapper.to_account_info(),
        amount,
        wrapper.decimals,
        &[seeds],
    )?;
    let wrapper = &mut ctx.accounts.wrapper;
    wrapper.total_wrapped = wrapper
        .total_wrapped
        .checked_sub(amount)
        .ok_or(BridgeError::MathOverflow)?;
    emit_cpi!(Unwrapped {
        wrapper: wrapper.key(),
        underlying_mint: wrapper.underlying_mint,
        wrapped_mint: wrapper.wrapped_mint,
        user: ctx.accounts.user.key(),
        amount_burned: amount,
        total_wrapped: wrapper.total_wrapped,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}

/// Accounts of `wrap_sol` and `unwrap_sol`.
#[event_cpi]
#[derive(Accounts)]
pub struct WrapSol<'info> {
    /// The user (pays or receives the lamports).
    #[account(mut)]
    pub user: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [WRAPPER_SEED, NATIVE_MINT.as_ref()], bump = wrapper.bump, constraint = wrapper.native @ BridgeError::NotNativeWrapper)]
    pub wrapper: Account<'info, Wrapper>,
    /// CHECK: the SOL vault.
    #[account(mut, seeds = [SOL_VAULT_SEED], bump = SOL_VAULT_BUMP)]
    pub sol_vault: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = wrapper.wrapped_mint @ BridgeError::WrongTokenAccount)]
    pub wrapped_mint: UncheckedAccount<'info>,
    /// CHECK: the user's holding of wrapped SOL (checked by the token program).
    #[account(mut)]
    pub user_wrapped: UncheckedAccount<'info>,
    /// CHECK: the Bordrless token program.
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: its event authority.
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `wrap_sol`.
pub fn process_wrap_sol(ctx: Context<WrapSol>, lamports: u64) -> Result<()> {
    let clock = Clock::get()?;
    require!(!ctx.accounts.config.paused, BridgeError::Paused);
    require!(lamports > 0, BridgeError::ZeroAmount);
    check_token(
        &ctx.accounts.token_program,
        &ctx.accounts.token_event_authority,
    )?;
    system_program::transfer(
        CpiContext::new(
            ctx.accounts.system_program.key(),
            system_program::Transfer {
                from: ctx.accounts.user.to_account_info(),
                to: ctx.accounts.sol_vault.to_account_info(),
            },
        ),
        lamports,
    )?;
    mint_wrapped(
        &ctx.accounts.wrapper,
        &ctx.accounts.wrapped_mint.to_account_info(),
        &ctx.accounts.user_wrapped.to_account_info(),
        &ctx.accounts.token_program.to_account_info(),
        &ctx.accounts.token_event_authority.to_account_info(),
        lamports,
    )?;
    let wrapper = &mut ctx.accounts.wrapper;
    wrapper.total_wrapped = wrapper
        .total_wrapped
        .checked_add(lamports)
        .ok_or(BridgeError::MathOverflow)?;
    emit_cpi!(Wrapped {
        wrapper: wrapper.key(),
        underlying_mint: NATIVE_MINT,
        wrapped_mint: wrapper.wrapped_mint,
        user: ctx.accounts.user.key(),
        amount_sent: lamports,
        amount_minted: lamports,
        total_wrapped: wrapper.total_wrapped,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}

/// `unwrap_sol` and `unwrap_sol_above`. Plain: `amount == u64::MAX` unwraps the whole holding.
/// Above: `amount` is what to keep, and everything above it is unwrapped.
pub fn process_unwrap_sol(ctx: Context<WrapSol>, amount: u64, above: bool) -> Result<()> {
    let clock = Clock::get()?;
    check_token(
        &ctx.accounts.token_program,
        &ctx.accounts.token_event_authority,
    )?;
    let held = token_client::read_holding(&ctx.accounts.user_wrapped)?.amount;
    let amount = if above {
        held.saturating_sub(amount)
    } else if amount == u64::MAX {
        held
    } else {
        amount
    };
    require!(amount > 0, BridgeError::ZeroAmount);
    let rent = Rent::get()?.minimum_balance(0);
    require!(
        ctx.accounts.sol_vault.lamports().saturating_sub(rent) >= amount,
        BridgeError::InsufficientVault
    );
    burn_wrapped(
        &ctx.accounts.user.to_account_info(),
        &ctx.accounts.user_wrapped.to_account_info(),
        &ctx.accounts.wrapped_mint.to_account_info(),
        &ctx.accounts.token_program.to_account_info(),
        &ctx.accounts.token_event_authority.to_account_info(),
        amount,
    )?;
    let seeds: &[&[u8]] = &[SOL_VAULT_SEED, &[SOL_VAULT_BUMP]];
    system_program::transfer(
        CpiContext::new_with_signer(
            ctx.accounts.system_program.key(),
            system_program::Transfer {
                from: ctx.accounts.sol_vault.to_account_info(),
                to: ctx.accounts.user.to_account_info(),
            },
            &[seeds],
        ),
        amount,
    )?;
    let wrapper = &mut ctx.accounts.wrapper;
    wrapper.total_wrapped = wrapper
        .total_wrapped
        .checked_sub(amount)
        .ok_or(BridgeError::MathOverflow)?;
    emit_cpi!(Unwrapped {
        wrapper: wrapper.key(),
        underlying_mint: NATIVE_MINT,
        wrapped_mint: wrapper.wrapped_mint,
        user: ctx.accounts.user.key(),
        amount_burned: amount,
        total_wrapped: wrapper.total_wrapped,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}
