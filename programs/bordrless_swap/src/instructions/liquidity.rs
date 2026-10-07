//! Adding and removing liquidity.

use anchor_lang::prelude::*;
use bordrless_core::{initial_lp, lp_for_deposit, mul_div_floor, withdraw_for_lp};
use bordrless_hook::{
    discriminators, pool_flags, Allowed, Phase, PoolHookArgs, PoolOp, MAX_HOOK_DATA,
};

use crate::constants::*;
use crate::error::SwapError;
use crate::events::*;
use crate::hooks::{split_extras, PoolHookCall};
use crate::instructions::pool::PoolSeeds;
use crate::state::*;
use crate::token::{balance, read_holding, TokenAccounts, TokenSide};

/// Arguments of `add_liquidity`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct AddLiquidityArgs {
    /// Base offered.
    pub base_desired: u64,
    /// Quote offered.
    pub quote_desired: u64,
    /// The least LP acceptable.
    pub min_lp: u64,
    /// Remaining accounts of the base mint's token hook.
    pub base_hook_accounts: u8,
    /// Remaining accounts of the quote mint's token hook.
    pub quote_hook_accounts: u8,
    /// Opaque data for the pool hook.
    pub hook_data: Vec<u8>,
}

/// Arguments of `remove_liquidity`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct RemoveLiquidityArgs {
    /// LP burned.
    pub lp_amount: u64,
    /// The least base acceptable.
    pub min_base: u64,
    /// The least quote acceptable.
    pub min_quote: u64,
    /// Remaining accounts of the base mint's token hook.
    pub base_hook_accounts: u8,
    /// Remaining accounts of the quote mint's token hook.
    pub quote_hook_accounts: u8,
    /// Opaque data for the pool hook.
    pub hook_data: Vec<u8>,
}

/// Accounts of `add_liquidity` and `remove_liquidity`.
#[event_cpi]
#[derive(Accounts)]
pub struct Liquidity<'info> {
    /// The liquidity provider.
    pub provider: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut)]
    pub pool: Account<'info, Pool>,
    /// CHECK: address-checked.
    #[account(address = pool.base_mint @ SwapError::WrongHolding)]
    pub base_mint: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(address = pool.quote_mint @ SwapError::WrongHolding)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = pool.lp_mint @ SwapError::WrongHolding)]
    pub lp_mint: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = pool.base_vault @ SwapError::WrongVault)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = pool.quote_vault @ SwapError::WrongVault)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: the provider's base holding (checked by the token program).
    #[account(mut)]
    pub provider_base: UncheckedAccount<'info>,
    /// CHECK: the provider's quote holding.
    #[account(mut)]
    pub provider_quote: UncheckedAccount<'info>,
    /// CHECK: the provider's LP holding.
    #[account(mut)]
    pub provider_lp: UncheckedAccount<'info>,
    /// CHECK: the pool's hook program (checked in the handler).
    pub hook_program: Option<UncheckedAccount<'info>>,
    /// CHECK: with a hook, this program's signer of its callbacks, `["hook-authority",
    /// hook_program]` (checked in the handler); absent (this program's id) without.
    pub hook_signer: Option<UncheckedAccount<'info>>,
    /// CHECK: the token program.
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    pub token_event_authority: UncheckedAccount<'info>,
}

struct Prepared<'a, 'info> {
    token: TokenAccounts<'info>,
    call: Option<PoolHookCall<'a, 'info>>,
    base_extras: &'a [AccountInfo<'info>],
    quote_extras: &'a [AccountInfo<'info>],
}

fn prepare<'info>(
    ctx: &Context<'info, Liquidity<'info>>,
    base_hook_accounts: u8,
    quote_hook_accounts: u8,
    hook_data_len: usize,
) -> Result<Prepared<'info, 'info>> {
    require!(!ctx.accounts.config.paused, SwapError::Paused);
    require!(!ctx.accounts.pool.curve, SwapError::CurveLocked);
    require!(hook_data_len <= MAX_HOOK_DATA, SwapError::HookDataTooLong);
    let token = TokenAccounts {
        program: ctx.accounts.token_program.to_account_info(),
        event_authority: ctx.accounts.token_event_authority.to_account_info(),
    };
    token.check()?;
    let (base_extras, quote_extras, pool_extras) = split_extras(
        ctx.remaining_accounts,
        base_hook_accounts,
        quote_hook_accounts,
    )?;
    let call = PoolHookCall::of(
        ctx.accounts.pool.hook_program,
        ctx.accounts.pool.hook_signer_bump,
        ctx.accounts
            .hook_program
            .as_ref()
            .map(|a| a.to_account_info()),
        ctx.accounts
            .hook_signer
            .as_ref()
            .map(|a| a.to_account_info()),
        [
            ctx.accounts.pool.to_account_info(),
            ctx.accounts.base_mint.to_account_info(),
            ctx.accounts.quote_mint.to_account_info(),
            ctx.accounts.provider.to_account_info(),
        ],
        pool_extras,
    )?;
    Ok(Prepared {
        token,
        call,
        base_extras,
        quote_extras,
    })
}

/// The owner of `holding` when the pool's hook will be told about it (the default key otherwise,
/// which is never sent).
fn recipient_of(call: &Option<PoolHookCall>, holding: &AccountInfo) -> Result<Pubkey> {
    match call {
        Some(_) => Ok(read_holding(holding)?.owner),
        None => Ok(Pubkey::default()),
    }
}

#[allow(clippy::too_many_arguments)]
fn hook_args(
    pool: &Pool,
    pool_key: Pubkey,
    provider: Pubkey,
    recipient: Pubkey,
    op: PoolOp,
    base: u64,
    quote: u64,
    lp: u64,
    hook_data: Vec<u8>,
) -> PoolHookArgs {
    PoolHookArgs {
        op,
        phase: Phase::Before,
        pool: pool_key,
        base_mint: pool.base_mint,
        quote_mint: pool.quote_mint,
        actor: provider,
        recipient,
        direction: 0,
        amount_in: base,
        amount_out: quote,
        base_reserve: pool.base_reserve,
        quote_reserve: pool.quote_reserve,
        virtual_base: pool.virtual_base,
        virtual_quote: pool.virtual_quote,
        lp_fee_bps: pool.lp_fee_bps,
        protocol_fee_bps: pool.protocol_fee_bps,
        swap_count: pool.swap_count,
        created_at: pool.created_at,
        lp_amount: lp,
        hook_data,
    }
}

/// `add_liquidity`.
pub fn process_add_liquidity<'info>(
    ctx: Context<'info, Liquidity<'info>>,
    args: AddLiquidityArgs,
) -> Result<()> {
    let clock = Clock::get()?;
    require!(
        args.base_desired > 0 && args.quote_desired > 0,
        SwapError::ZeroAmount
    );
    let p = prepare(
        &ctx,
        args.base_hook_accounts,
        args.quote_hook_accounts,
        args.hook_data.len(),
    )?;
    let pool_key = ctx.accounts.pool.key();
    // The recipient of an add is the owner of the LP holding.
    let recipient = recipient_of(&p.call, &ctx.accounts.provider_lp)?;
    let mut a = hook_args(
        &ctx.accounts.pool,
        pool_key,
        ctx.accounts.provider.key(),
        recipient,
        PoolOp::AddLiquidity,
        args.base_desired,
        args.quote_desired,
        0,
        args.hook_data.clone(),
    );
    if let Some(call) = &p.call {
        if ctx.accounts.pool.runs(pool_flags::BEFORE_ADD_LIQUIDITY) {
            call.invoke(discriminators::BEFORE_ADD_LIQUIDITY, &a, Allowed::NONE)?;
        }
    }

    // What to pull: at the pool's ratio when it has reserves, as offered when it is empty.
    let (base_reserve, quote_reserve, lp_supply) = (
        ctx.accounts.pool.base_reserve,
        ctx.accounts.pool.quote_reserve,
        ctx.accounts.pool.lp_supply,
    );
    let empty = lp_supply == 0 || base_reserve == 0 || quote_reserve == 0;
    let (base_used, quote_used) = if empty {
        (args.base_desired, args.quote_desired)
    } else {
        let (b, q, _) = lp_for_deposit(
            args.base_desired,
            args.quote_desired,
            base_reserve,
            quote_reserve,
            lp_supply,
        )
        .ok_or(SwapError::DepositTooSmall)?;
        (b, q)
    };

    let provider = ctx.accounts.provider.to_account_info();
    let base_mint_hook = bordrless_token::client::read_mint(&ctx.accounts.base_mint)?.hook_program;
    let quote_mint_hook =
        bordrless_token::client::read_mint(&ctx.accounts.quote_mint)?.hook_program;
    let base_before = balance(&ctx.accounts.base_vault)?;
    p.token.transfer(
        &provider,
        &ctx.accounts.provider_base.to_account_info(),
        &ctx.accounts.base_vault.to_account_info(),
        &ctx.accounts.base_mint.to_account_info(),
        &TokenSide::of(p.base_extras, base_mint_hook.is_some()),
        base_used,
        &[],
    )?;
    let base_received = balance(&ctx.accounts.base_vault)?
        .checked_sub(base_before)
        .ok_or(SwapError::MathOverflow)?;
    let quote_before = balance(&ctx.accounts.quote_vault)?;
    p.token.transfer(
        &provider,
        &ctx.accounts.provider_quote.to_account_info(),
        &ctx.accounts.quote_vault.to_account_info(),
        &ctx.accounts.quote_mint.to_account_info(),
        &TokenSide::of(p.quote_extras, quote_mint_hook.is_some()),
        quote_used,
        &[],
    )?;
    let quote_received = balance(&ctx.accounts.quote_vault)?
        .checked_sub(quote_before)
        .ok_or(SwapError::MathOverflow)?;
    require!(
        base_received > 0 && quote_received > 0,
        SwapError::NothingReceived
    );

    // LP from what actually arrived.
    let (lp, new_supply) = if empty {
        let (to_provider, total) =
            initial_lp(base_received, quote_received).ok_or(SwapError::DepositTooSmall)?;
        (
            to_provider,
            lp_supply
                .checked_add(total)
                .ok_or(SwapError::MathOverflow)?,
        )
    } else {
        let by_base = mul_div_floor(
            u128::from(base_received),
            u128::from(lp_supply),
            u128::from(base_reserve),
        )
        .ok_or(SwapError::MathOverflow)?;
        let by_quote = mul_div_floor(
            u128::from(quote_received),
            u128::from(lp_supply),
            u128::from(quote_reserve),
        )
        .ok_or(SwapError::MathOverflow)?;
        let lp = u64::try_from(by_base.min(by_quote)).map_err(|_| SwapError::MathOverflow)?;
        require!(lp > 0, SwapError::DepositTooSmall);
        (
            lp,
            lp_supply.checked_add(lp).ok_or(SwapError::MathOverflow)?,
        )
    };
    require!(lp >= args.min_lp, SwapError::LpBelowMinimum);

    let pool_info = ctx.accounts.pool.to_account_info();
    let seeds = PoolSeeds::of(&ctx.accounts.pool);
    let pool_seeds = seeds.seeds();
    p.token.mint_to(
        &pool_info,
        &ctx.accounts.lp_mint.to_account_info(),
        &ctx.accounts.provider_lp.to_account_info(),
        lp,
        &[&pool_seeds],
    )?;

    {
        let pool = &mut ctx.accounts.pool;
        pool.base_reserve = pool
            .base_reserve
            .checked_add(base_received)
            .ok_or(SwapError::MathOverflow)?;
        pool.quote_reserve = pool
            .quote_reserve
            .checked_add(quote_received)
            .ok_or(SwapError::MathOverflow)?;
        pool.lp_supply = new_supply;
    }
    if let Some(call) = &p.call {
        if ctx.accounts.pool.runs(pool_flags::AFTER_ADD_LIQUIDITY) {
            ctx.accounts.pool.exit(&crate::ID)?;
            a.phase = Phase::After;
            a.amount_in = base_received;
            a.amount_out = quote_received;
            a.lp_amount = lp;
            a.base_reserve = ctx.accounts.pool.base_reserve;
            a.quote_reserve = ctx.accounts.pool.quote_reserve;
            call.invoke(discriminators::AFTER_ADD_LIQUIDITY, &a, Allowed::NONE)?;
        }
    }
    let pool = &ctx.accounts.pool;
    emit_cpi!(LiquidityAdded {
        pool: pool_key,
        provider: ctx.accounts.provider.key(),
        base_amount: base_received,
        quote_amount: quote_received,
        lp_minted: lp,
        base_reserve: pool.base_reserve,
        quote_reserve: pool.quote_reserve,
        lp_supply: pool.lp_supply,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}

/// `remove_liquidity`.
pub fn process_remove_liquidity<'info>(
    ctx: Context<'info, Liquidity<'info>>,
    args: RemoveLiquidityArgs,
) -> Result<()> {
    let clock = Clock::get()?;
    require!(args.lp_amount > 0, SwapError::ZeroAmount);
    let p = prepare(
        &ctx,
        args.base_hook_accounts,
        args.quote_hook_accounts,
        args.hook_data.len(),
    )?;
    let pool_key = ctx.accounts.pool.key();
    let (base_reserve, quote_reserve, lp_supply) = (
        ctx.accounts.pool.base_reserve,
        ctx.accounts.pool.quote_reserve,
        ctx.accounts.pool.lp_supply,
    );
    let (base_out, quote_out) =
        withdraw_for_lp(args.lp_amount, base_reserve, quote_reserve, lp_supply)
            .ok_or(SwapError::InsufficientLiquidity)?;
    require!(
        base_out >= args.min_base && quote_out >= args.min_quote,
        SwapError::WithdrawalBelowMinimum
    );
    require!(
        base_out > 0 || quote_out > 0,
        SwapError::WithdrawalBelowMinimum
    );
    // The recipient of a removal is the owner of the base holding.
    let recipient = recipient_of(&p.call, &ctx.accounts.provider_base)?;
    let mut a = hook_args(
        &ctx.accounts.pool,
        pool_key,
        ctx.accounts.provider.key(),
        recipient,
        PoolOp::RemoveLiquidity,
        base_out,
        quote_out,
        args.lp_amount,
        args.hook_data.clone(),
    );
    if let Some(call) = &p.call {
        if ctx.accounts.pool.runs(pool_flags::BEFORE_REMOVE_LIQUIDITY) {
            call.invoke(discriminators::BEFORE_REMOVE_LIQUIDITY, &a, Allowed::NONE)?;
        }
    }

    // The LP mint has no hook; the provider signed.
    let provider = ctx.accounts.provider.to_account_info();
    p.token.burn(
        &provider,
        &ctx.accounts.provider_lp.to_account_info(),
        &ctx.accounts.lp_mint.to_account_info(),
        &TokenSide::none(),
        args.lp_amount,
        &[],
    )?;
    let pool_info = ctx.accounts.pool.to_account_info();
    let seeds = PoolSeeds::of(&ctx.accounts.pool);
    let pool_seeds = seeds.seeds();
    let base_mint_hook = bordrless_token::client::read_mint(&ctx.accounts.base_mint)?.hook_program;
    let quote_mint_hook =
        bordrless_token::client::read_mint(&ctx.accounts.quote_mint)?.hook_program;
    if base_out > 0 {
        p.token.transfer(
            &pool_info,
            &ctx.accounts.base_vault.to_account_info(),
            &ctx.accounts.provider_base.to_account_info(),
            &ctx.accounts.base_mint.to_account_info(),
            &TokenSide::of(p.base_extras, base_mint_hook.is_some()),
            base_out,
            &[&pool_seeds],
        )?;
    }
    if quote_out > 0 {
        p.token.transfer(
            &pool_info,
            &ctx.accounts.quote_vault.to_account_info(),
            &ctx.accounts.provider_quote.to_account_info(),
            &ctx.accounts.quote_mint.to_account_info(),
            &TokenSide::of(p.quote_extras, quote_mint_hook.is_some()),
            quote_out,
            &[&pool_seeds],
        )?;
    }
    {
        let pool = &mut ctx.accounts.pool;
        pool.base_reserve = pool
            .base_reserve
            .checked_sub(base_out)
            .ok_or(SwapError::MathOverflow)?;
        pool.quote_reserve = pool
            .quote_reserve
            .checked_sub(quote_out)
            .ok_or(SwapError::MathOverflow)?;
        pool.lp_supply = pool
            .lp_supply
            .checked_sub(args.lp_amount)
            .ok_or(SwapError::MathOverflow)?;
    }
    if let Some(call) = &p.call {
        if ctx.accounts.pool.runs(pool_flags::AFTER_REMOVE_LIQUIDITY) {
            ctx.accounts.pool.exit(&crate::ID)?;
            a.phase = Phase::After;
            a.base_reserve = ctx.accounts.pool.base_reserve;
            a.quote_reserve = ctx.accounts.pool.quote_reserve;
            call.invoke(discriminators::AFTER_REMOVE_LIQUIDITY, &a, Allowed::NONE)?;
        }
    }
    let pool = &ctx.accounts.pool;
    emit_cpi!(LiquidityRemoved {
        pool: pool_key,
        provider: ctx.accounts.provider.key(),
        base_amount: base_out,
        quote_amount: quote_out,
        lp_burned: args.lp_amount,
        base_reserve: pool.base_reserve,
        quote_reserve: pool.quote_reserve,
        lp_supply: pool.lp_supply,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}
