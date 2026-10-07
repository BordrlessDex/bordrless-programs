//! Transfers and burns, with hooks.

use anchor_lang::prelude::*;
use bordrless_hook::{discriminators, token_flags, Allowed, Phase, TokenHookArgs, TokenOp};

use crate::error::TokenError;
use crate::events::*;
use crate::hooks::{apply_deltas, HookCall};
use crate::state::*;

/// Checks that `authority` may move `amount` out of `holding` and, for a delegate, spends the
/// allowance. Answers whether it acted as the delegate.
pub(crate) fn spend_authority(
    holding: &mut Holding,
    authority: &Pubkey,
    amount: u64,
) -> Result<bool> {
    if holding.owner == *authority {
        return Ok(false);
    }
    if holding.delegate == Some(*authority) {
        require!(
            holding.delegated_amount >= amount,
            TokenError::InsufficientDelegation
        );
        holding.delegated_amount -= amount;
        return Ok(true);
    }
    err!(TokenError::NotAuthorized)
}

/// Accounts of `transfer`. The mint is read-only: no transfer write-locks a mint.
#[event_cpi]
#[derive(Accounts)]
pub struct Transfer<'info> {
    /// The source's owner or delegate.
    pub authority: Signer<'info>,
    #[account(mut, has_one = mint @ TokenError::MintMismatch)]
    pub source: Account<'info, Holding>,
    #[account(mut, has_one = mint @ TokenError::MintMismatch, constraint = destination.key() != source.key() @ TokenError::SameAccount)]
    pub destination: Account<'info, Holding>,
    pub mint: Account<'info, Mint>,
    /// CHECK: the mint's hook program, when it has one (checked in the handler).
    pub hook_program: Option<UncheckedAccount<'info>>,
    /// CHECK: when the mint has a hook, this program's signer of its callbacks,
    /// `["hook-authority", hook_program]` (checked in the handler); unused (this program's id)
    /// otherwise.
    pub hook_signer: Option<UncheckedAccount<'info>>,
}

/// `transfer`. When `before_transfer` answers: up to three deltas (with `TRANSFER_RETURNS_DELTA`)
/// whose sum is at most `amount`, each credited to a holding the hook names, and new hook data
/// for either side (with `WRITES_HOOK_DATA`), written with the balances. Never a burn. The source
/// loses `amount`; the destination gains `amount` less the deltas.
pub fn process_transfer<'info>(ctx: Context<'info, Transfer<'info>>, amount: u64) -> Result<()> {
    require!(amount > 0, TokenError::ZeroAmount);
    let clock = Clock::get()?;
    let mint = &ctx.accounts.mint;
    let source = &mut ctx.accounts.source;
    let destination = &mut ctx.accounts.destination;
    require!(!source.frozen && !destination.frozen, TokenError::Frozen);
    let is_delegate = spend_authority(source, ctx.accounts.authority.key, amount)?;
    require!(source.amount >= amount, TokenError::InsufficientFunds);

    let hook = mint.hook();
    let call = HookCall::of(
        mint,
        ctx.accounts
            .hook_program
            .as_ref()
            .map(|a| a.to_account_info()),
        ctx.accounts
            .hook_signer
            .as_ref()
            .map(|a| a.to_account_info()),
        [
            mint.to_account_info(),
            source.to_account_info(),
            destination.to_account_info(),
            ctx.accounts.authority.to_account_info(),
        ],
        ctx.remaining_accounts,
    )?;
    let mut args = TokenHookArgs {
        op: TokenOp::Transfer,
        phase: Phase::Before,
        mint: mint.key(),
        source: source.key(),
        destination: destination.key(),
        source_owner: source.owner,
        destination_owner: destination.owner,
        authority: ctx.accounts.authority.key(),
        authority_is_delegate: is_delegate,
        amount,
        delta: 0,
        source_balance: source.amount,
        destination_balance: destination.amount,
        decimals: mint.decimals,
        supply: mint.supply,
        source_hook_data: source.hook_data,
        destination_hook_data: destination.hook_data,
    };

    // Before: the hook may answer deltas and both sides' hook data, as the mint's flags allow.
    let mut taken = 0u64;
    let mut deltas: Vec<DeltaApplied> = Vec::new();
    let mut source_data: Option<[u8; 64]> = None;
    let mut destination_data: Option<[u8; 64]> = None;
    if let (Some(call), Some((_, flags))) = (&call, hook) {
        if flags & token_flags::BEFORE_TRANSFER != 0 {
            let allowed = Allowed::token(TokenOp::Transfer, Phase::Before, flags);
            if let Some((answer, sum)) =
                call.invoke_for_answer(discriminators::BEFORE_TRANSFER, &args, allowed)?
            {
                require!(sum <= amount, TokenError::DeltaTooLarge);
                deltas = apply_deltas(
                    ctx.remaining_accounts,
                    &answer.deltas,
                    &mint.key(),
                    &[source.key(), destination.key()],
                )?;
                taken = sum;
                source_data = answer.source_hook_data;
                destination_data = answer.destination_hook_data;
            }
        }
    }

    // The balances, then the hook data answered for each side.
    source.amount -= amount;
    destination.amount = destination
        .amount
        .checked_add(amount - taken)
        .ok_or(TokenError::MathOverflow)?;
    if let Some(data) = source_data {
        source.hook_data = data;
    }
    if let Some(data) = destination_data {
        destination.hook_data = data;
    }

    if let (Some(call), Some((_, flags))) = (&call, hook) {
        if flags & token_flags::AFTER_TRANSFER != 0 {
            source.exit(&crate::ID)?;
            destination.exit(&crate::ID)?;
            args.phase = Phase::After;
            args.delta = taken;
            args.source_balance = source.amount;
            args.destination_balance = destination.amount;
            args.source_hook_data = source.hook_data;
            args.destination_hook_data = destination.hook_data;
            call.invoke(discriminators::AFTER_TRANSFER, &args)?;
        }
    }

    emit_cpi!(Transferred {
        mint: mint.key(),
        source: source.key(),
        destination: destination.key(),
        source_owner: source.owner,
        destination_owner: destination.owner,
        authority: ctx.accounts.authority.key(),
        amount,
        deltas,
        source_post: source.amount,
        destination_post: destination.amount,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}

/// Accounts of `burn`.
#[event_cpi]
#[derive(Accounts)]
pub struct Burn<'info> {
    /// The source's owner or delegate.
    pub authority: Signer<'info>,
    #[account(mut, has_one = mint @ TokenError::MintMismatch)]
    pub source: Account<'info, Holding>,
    #[account(mut)]
    pub mint: Account<'info, Mint>,
    /// CHECK: the mint's hook program, when it has one (checked in the handler).
    pub hook_program: Option<UncheckedAccount<'info>>,
    /// CHECK: when the mint has a hook, this program's signer of its callbacks,
    /// `["hook-authority", hook_program]` (checked in the handler); unused (this program's id)
    /// otherwise.
    pub hook_signer: Option<UncheckedAccount<'info>>,
}

/// `burn`. `before_burn` may answer the source's hook data (with `WRITES_HOOK_DATA`), nothing else.
pub fn process_burn<'info>(ctx: Context<'info, Burn<'info>>, amount: u64) -> Result<()> {
    require!(amount > 0, TokenError::ZeroAmount);
    let clock = Clock::get()?;
    let mint = &mut ctx.accounts.mint;
    let source = &mut ctx.accounts.source;
    require!(!source.frozen, TokenError::Frozen);
    let is_delegate = spend_authority(source, ctx.accounts.authority.key, amount)?;
    require!(source.amount >= amount, TokenError::InsufficientFunds);

    let hook = mint.hook();
    let call = HookCall::of(
        mint,
        ctx.accounts
            .hook_program
            .as_ref()
            .map(|a| a.to_account_info()),
        ctx.accounts
            .hook_signer
            .as_ref()
            .map(|a| a.to_account_info()),
        [
            mint.to_account_info(),
            source.to_account_info(),
            mint.to_account_info(),
            ctx.accounts.authority.to_account_info(),
        ],
        ctx.remaining_accounts,
    )?;
    let mut args = TokenHookArgs {
        op: TokenOp::Burn,
        phase: Phase::Before,
        mint: mint.key(),
        source: source.key(),
        destination: mint.key(),
        source_owner: source.owner,
        destination_owner: Pubkey::default(),
        authority: ctx.accounts.authority.key(),
        authority_is_delegate: is_delegate,
        amount,
        delta: 0,
        source_balance: source.amount,
        destination_balance: 0,
        decimals: mint.decimals,
        supply: mint.supply,
        source_hook_data: source.hook_data,
        destination_hook_data: [0; 64],
    };
    let mut source_data: Option<[u8; 64]> = None;
    if let (Some(call), Some((_, flags))) = (&call, hook) {
        if flags & token_flags::BEFORE_BURN != 0 {
            let allowed = Allowed::token(TokenOp::Burn, Phase::Before, flags);
            if let Some((answer, _)) =
                call.invoke_for_answer(discriminators::BEFORE_BURN, &args, allowed)?
            {
                source_data = answer.source_hook_data;
            }
        }
    }
    source.amount -= amount;
    mint.supply = mint
        .supply
        .checked_sub(amount)
        .ok_or(TokenError::MathOverflow)?;
    if let Some(data) = source_data {
        source.hook_data = data;
    }
    if let (Some(call), Some((_, flags))) = (&call, hook) {
        if flags & token_flags::AFTER_BURN != 0 {
            source.exit(&crate::ID)?;
            mint.exit(&crate::ID)?;
            args.phase = Phase::After;
            args.source_balance = source.amount;
            args.supply = mint.supply;
            args.source_hook_data = source.hook_data;
            call.invoke(discriminators::AFTER_BURN, &args)?;
        }
    }
    emit_cpi!(Burned {
        mint: mint.key(),
        source: source.key(),
        source_owner: source.owner,
        authority: ctx.accounts.authority.key(),
        amount,
        source_post: source.amount,
        supply_post: mint.supply,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}
