//! Swaps (hook protocol v2, `docs/hooks-v2.md` §3.1).
//!
//! A buy (quote in, base out): the `before_swap` answer's deltas, then its burn, leave the trader's
//! quote holding (the trader signed the swap); the rest reaches the quote vault (`received`), which
//! pays the LP fee and the protocol fee; the curve runs on what is left; the `after_swap` answer's
//! deltas, then its burn, leave the base vault (the pool signs); the rest goes to the recipient.
//!
//! A sell (base in, quote out): the input as for a buy; the LP fee on `received` (flat model;
//! under the share model it leaves the curve's output with the protocol fee); the curve; the
//! protocol fee from the curve's output, kept in the quote vault (so the protocol fee is always in
//! the quote token); the `after_swap` answer is taken from what is left of the output; the rest
//! goes to the recipient.
//!
//! Two protocol fee models (`Pool.fee_model`). Flat (ordinary pools): a rate of the quote, as
//! above. Share (launch pools): the pool's share of what the hooks cut and someone received, on
//! both sides, measured (`cuts_in = amount_in - burn_in - received`, `cuts_out = amount_out -
//! burn_out - held - delivered`), base-side cuts valued at the swap's own price, each side's
//! share rounded up and never above its value. A buy's input share leaves what reached the vault
//! before the curve; a sell's input share leaves the curve's output before the hook is told; a
//! sell's output share is held back from the delivery; a buy's output share (and a sell's share of
//! a quote hook's own cut on the delivery) is set aside from the quote reserve after the swap.
//! Burns are not cuts anyone collects, so they are not shared. Under the share model the LP fee is
//! Bordrless's too (a launch pool's liquidity is locked for ever, so nothing compounds): taken in
//! the quote, from a buy's `received` before the curve or a sell's curve output before the hook is
//! told, and added to the protocol fee; a pool whose hooks cut nothing pays the LP fee alone.
//!
//! Amounts are measured, never assumed: `received` is what the input vault gained, and
//! `min_amount_out` is checked against what the recipient's holding gained, so a token whose own
//! hook takes a cut still prices and settles correctly.

use anchor_lang::prelude::*;
use bordrless_core::{output_share, protocol_share, swap_amounts, swap_amounts_shared, Reserves};
use bordrless_hook::{
    discriminators, pool_flags, Allowed, Phase, PoolHookArgs, PoolOp, MAX_HOOK_DATA,
};

use crate::constants::*;
use crate::error::{swap_failure, SwapError};
use crate::events::*;
use crate::hooks::{split_extras, Cut, PoolHookCall};
use crate::instructions::pool::PoolSeeds;
use crate::state::*;
use crate::token::{balance, read_holding, TokenAccounts, TokenSide};

/// Arguments of `swap`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct SwapArgs {
    /// 0 base → quote (a sell), 1 quote → base (a buy).
    pub direction: u8,
    /// Exact input.
    pub amount_in: u64,
    /// The least the recipient's holding must gain.
    pub min_amount_out: u64,
    /// Remaining accounts of the input mint's token hook (its program, the token program's
    /// signer for it, its extras; none without a hook).
    pub in_hook_accounts: u8,
    /// Remaining accounts of the output mint's token hook (likewise).
    pub out_hook_accounts: u8,
    /// Opaque data for the pool hook.
    pub hook_data: Vec<u8>,
}

/// Accounts of `swap`. The remaining accounts are the input mint's token-hook slice (its program,
/// the token program's signer for it, its extras), the output mint's, then the pool hook's
/// extras. A mint is passed writable only when the pool's hook may burn from that side (else
/// `MintNotWritable`); a transfer never write-locks a mint.
#[event_cpi]
#[derive(Accounts)]
pub struct Swap<'info> {
    /// The trader: owner (or delegate) of the input holding.
    pub trader: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut)]
    pub pool: Account<'info, Pool>,
    /// CHECK: address-checked; writable only for a burn of base.
    #[account(address = pool.base_mint @ SwapError::WrongHolding)]
    pub base_mint: UncheckedAccount<'info>,
    /// CHECK: address-checked; writable only for a burn of quote.
    #[account(address = pool.quote_mint @ SwapError::WrongHolding)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = pool.base_vault @ SwapError::WrongVault)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = pool.quote_vault @ SwapError::WrongVault)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: the trader's base holding, or on a buy any holding of the base mint to deliver to
    /// (checked by the token program).
    #[account(mut)]
    pub trader_base: UncheckedAccount<'info>,
    /// CHECK: the trader's quote holding, or on a sell any holding of the quote mint to deliver to.
    #[account(mut)]
    pub trader_quote: UncheckedAccount<'info>,
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

/// `swap`.
pub fn process_swap<'info>(ctx: Context<'info, Swap<'info>>, args: SwapArgs) -> Result<()> {
    let clock = Clock::get()?;
    let SwapArgs {
        direction,
        amount_in,
        min_amount_out,
        in_hook_accounts,
        out_hook_accounts,
        hook_data,
    } = args;
    require!(!ctx.accounts.config.paused, SwapError::Paused);
    require!(amount_in > 0, SwapError::ZeroAmount);
    require!(direction <= 1, SwapError::InvalidDirection);
    require!(hook_data.len() <= MAX_HOOK_DATA, SwapError::HookDataTooLong);
    let buy = direction == 1;
    let token = TokenAccounts {
        program: ctx.accounts.token_program.to_account_info(),
        event_authority: ctx.accounts.token_event_authority.to_account_info(),
    };
    token.check()?;
    let (in_extras, out_extras, pool_extras) =
        split_extras(ctx.remaining_accounts, in_hook_accounts, out_hook_accounts)?;

    // The sides.
    let base_mint = ctx.accounts.base_mint.to_account_info();
    let quote_mint = ctx.accounts.quote_mint.to_account_info();
    let (in_mint, out_mint, vault_in, vault_out, trader_in, trader_out) = if buy {
        (
            quote_mint.clone(),
            base_mint.clone(),
            ctx.accounts.quote_vault.to_account_info(),
            ctx.accounts.base_vault.to_account_info(),
            ctx.accounts.trader_quote.to_account_info(),
            ctx.accounts.trader_base.to_account_info(),
        )
    } else {
        (
            base_mint.clone(),
            quote_mint.clone(),
            ctx.accounts.base_vault.to_account_info(),
            ctx.accounts.quote_vault.to_account_info(),
            ctx.accounts.trader_base.to_account_info(),
            ctx.accounts.trader_quote.to_account_info(),
        )
    };
    let in_hook = bordrless_token::client::read_mint(&in_mint)?.hook_program;
    let out_hook = bordrless_token::client::read_mint(&out_mint)?.hook_program;
    let recipient = read_holding(&trader_out)?.owner;
    // No delta may go to a vault or to either of the trader's holdings.
    let forbidden = [
        *vault_in.key,
        *vault_out.key,
        *trader_in.key,
        *trader_out.key,
    ];

    let pool_key = ctx.accounts.pool.key();
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
            base_mint,
            quote_mint,
            ctx.accounts.trader.to_account_info(),
        ],
        pool_extras,
    )?;
    let pool = &ctx.accounts.pool;
    let flags = pool.hook_flags;
    let mut hook_args = PoolHookArgs {
        op: PoolOp::Swap,
        phase: Phase::Before,
        pool: pool_key,
        base_mint: pool.base_mint,
        quote_mint: pool.quote_mint,
        actor: ctx.accounts.trader.key(),
        recipient,
        direction,
        amount_in,
        amount_out: 0,
        base_reserve: pool.base_reserve,
        quote_reserve: pool.quote_reserve,
        virtual_base: pool.virtual_base,
        virtual_quote: pool.virtual_quote,
        lp_fee_bps: pool.lp_fee_bps,
        protocol_fee_bps: pool.protocol_fee_bps,
        swap_count: pool.swap_count,
        created_at: pool.created_at,
        lp_amount: 0,
        hook_data,
    };

    // 1. Before: the hook may set the LP fee and cut the input (deltas and a burn), as the pool's
    //    flags allow; the answer is checked before anything moves.
    let mut lp_fee_bps = pool.lp_fee_bps;
    let mut cut_in = Cut::none();
    if let Some(call) = &call {
        if pool.runs(pool_flags::BEFORE_SWAP) {
            let allowed = Allowed::pool(PoolOp::Swap, Phase::Before, flags);
            if let Some((ret, taken)) =
                call.invoke(discriminators::BEFORE_SWAP, &hook_args, allowed)?
            {
                if let Some(fee) = ret.lp_fee_bps {
                    require!(fee <= MAX_LP_FEE_BPS, SwapError::FeeTooHigh);
                    lp_fee_bps = fee;
                }
                cut_in = call.cut(&ret, taken, amount_in, &in_mint, &forbidden)?;
            }
        }
    }

    // 2. The input: the cut from the trader's holding (each delta, then the burn), then the rest
    //    to the vault, measured there.
    let trader = ctx.accounts.trader.to_account_info();
    let in_side = TokenSide::of(in_extras, in_hook.is_some());
    for (holding, amount) in &cut_in.deltas {
        token.transfer(
            &trader,
            &trader_in,
            holding,
            &in_mint,
            &in_side,
            *amount,
            &[],
        )?;
    }
    if cut_in.burn > 0 {
        token.burn(&trader, &trader_in, &in_mint, &in_side, cut_in.burn, &[])?;
    }
    let vault_in_before = balance(&vault_in)?;
    token.transfer(
        &trader,
        &trader_in,
        &vault_in,
        &in_mint,
        &in_side,
        amount_in - cut_in.taken,
        &[],
    )?;
    let received = balance(&vault_in)?
        .checked_sub(vault_in_before)
        .ok_or(SwapError::MathOverflow)?;
    require!(received > 0, SwapError::NothingReceived);
    // What the hooks took from the input and someone received: the pool hook's deltas and any
    // cut the input mint's own hook took from those transfers (measured, in the input token).
    let cuts_in = amount_in
        .checked_sub(cut_in.burn)
        .and_then(|rest| rest.checked_sub(received))
        .ok_or(SwapError::MathOverflow)?;

    // 3. The fees and the curve; the protocol fee is in quote. Flat model: the LP fee on the input,
    //    and a rate of the input of a buy or of the curve's output of a sell. Share model: the LP
    //    fee (Bordrless's, in quote: a buy's from the input, a sell's from the curve's output) plus
    //    the pool's share of `cuts_in` (a buy's quote cut before the curve; a sell's base cut valued
    //    at the swap's price, from the curve's output before the hook is told).
    let reserves = Reserves {
        base_reserve: pool.base_reserve,
        quote_reserve: pool.quote_reserve,
        virtual_base: pool.virtual_base,
        virtual_quote: pool.virtual_quote,
    };
    let shared = pool.shares_cuts();
    let share_bps = pool.protocol_share_bps;
    let amounts = if shared {
        swap_amounts_shared(buy, received, lp_fee_bps, share_bps, cuts_in, &reserves)
    } else {
        swap_amounts(buy, received, lp_fee_bps, pool.protocol_fee_bps, &reserves)
    }
    .map_err(swap_failure)?;

    // 4. Reserves: `to_reserve_in` enters the input reserve (`received` less a buy's protocol fee,
    //    and less the LP fee too under the share model); the curve's
    //    whole output leaves the output reserve (the hook's cut, the sell's protocol fee and the
    //    delivery all come out of it); the protocol fee is set aside in the quote vault.
    {
        let pool = &mut ctx.accounts.pool;
        if buy {
            pool.quote_reserve = pool
                .quote_reserve
                .checked_add(amounts.to_reserve_in)
                .ok_or(SwapError::MathOverflow)?;
            pool.base_reserve = pool
                .base_reserve
                .checked_sub(amounts.out_gross)
                .ok_or(SwapError::InsufficientLiquidity)?;
            pool.quote_volume = pool
                .quote_volume
                .checked_add(u128::from(received))
                .ok_or(SwapError::MathOverflow)?;
            pool.base_volume = pool
                .base_volume
                .checked_add(u128::from(amounts.out_gross))
                .ok_or(SwapError::MathOverflow)?;
        } else {
            pool.base_reserve = pool
                .base_reserve
                .checked_add(amounts.to_reserve_in)
                .ok_or(SwapError::MathOverflow)?;
            pool.quote_reserve = pool
                .quote_reserve
                .checked_sub(amounts.out_gross)
                .ok_or(SwapError::InsufficientLiquidity)?;
            pool.base_volume = pool
                .base_volume
                .checked_add(u128::from(received))
                .ok_or(SwapError::MathOverflow)?;
            pool.quote_volume = pool
                .quote_volume
                .checked_add(u128::from(amounts.out_gross))
                .ok_or(SwapError::MathOverflow)?;
        }
        pool.protocol_fees_quote = pool
            .protocol_fees_quote
            .checked_add(amounts.protocol_fee)
            .ok_or(SwapError::MathOverflow)?;
        pool.swap_count = pool
            .swap_count
            .checked_add(1)
            .ok_or(SwapError::MathOverflow)?;
        pool.last_swap_at = clock.unix_timestamp;
    }

    // 5. After: the hook sees the result (the pool written as it now stands) and may cut the
    //    output it is told: the curve's output, less a sell's protocol fee.
    let mut cut_out = Cut::none();
    if let Some(call) = &call {
        if ctx.accounts.pool.runs(pool_flags::AFTER_SWAP) {
            ctx.accounts.pool.exit(&crate::ID)?;
            let pool = &ctx.accounts.pool;
            hook_args.phase = Phase::After;
            hook_args.amount_in = received;
            hook_args.amount_out = amounts.amount_out;
            hook_args.base_reserve = pool.base_reserve;
            hook_args.quote_reserve = pool.quote_reserve;
            hook_args.lp_fee_bps = lp_fee_bps;
            let allowed = Allowed::pool(PoolOp::Swap, Phase::After, flags);
            if let Some((ret, taken)) =
                call.invoke(discriminators::AFTER_SWAP, &hook_args, allowed)?
            {
                cut_out = call.cut(&ret, taken, amounts.amount_out, &out_mint, &forbidden)?;
            }
        }
    }

    // 6. The output: the cut from the vault (each delta, then the burn; the pool signs), then the
    //    rest to the recipient, measured there. Under the share model a sell holds back the pool's
    //    share of the hook's quote deltas from the delivery (the trader pays it).
    let held_back = if shared && !buy {
        let deltas_sum = cut_out.taken - cut_out.burn;
        protocol_share(deltas_sum, share_bps).ok_or(SwapError::MathOverflow)?
    } else {
        0
    };
    let to_deliver = amounts
        .amount_out
        .checked_sub(cut_out.taken)
        .and_then(|rest| rest.checked_sub(held_back))
        .filter(|rest| *rest > 0)
        .ok_or(SwapError::FeeExceedsOutput)?;
    let pool_info = ctx.accounts.pool.to_account_info();
    let seeds = PoolSeeds::of(&ctx.accounts.pool);
    let pool_seeds = seeds.seeds();
    let out_side = TokenSide::of(out_extras, out_hook.is_some());
    for (holding, amount) in &cut_out.deltas {
        token.transfer(
            &pool_info,
            &vault_out,
            holding,
            &out_mint,
            &out_side,
            *amount,
            &[&pool_seeds],
        )?;
    }
    if cut_out.burn > 0 {
        token.burn(
            &pool_info,
            &vault_out,
            &out_mint,
            &out_side,
            cut_out.burn,
            &[&pool_seeds],
        )?;
    }
    let trader_out_before = balance(&trader_out)?;
    token.transfer(
        &pool_info,
        &vault_out,
        &trader_out,
        &out_mint,
        &out_side,
        to_deliver,
        &[&pool_seeds],
    )?;
    let delivered = balance(&trader_out)?
        .checked_sub(trader_out_before)
        .ok_or(SwapError::MathOverflow)?;
    require!(delivered >= min_amount_out, SwapError::Slippage);
    // What the hooks took from the output and someone received (measured, in the output token).
    let cuts_out = amounts
        .amount_out
        .checked_sub(cut_out.burn)
        .and_then(|rest| rest.checked_sub(held_back))
        .and_then(|rest| rest.checked_sub(delivered))
        .ok_or(SwapError::MathOverflow)?;

    // 7. Share model: the pool's share of the output side's cuts (a buy's base cut valued at the
    //    swap's price). What the delivery held back is already in the vault; the rest (a buy's
    //    whole share, a sell's share of a quote hook's own cut on the delivery) is set aside from
    //    the quote reserve, the only quote the pool holds once the swap has run.
    let protocol_out = if shared {
        output_share(buy, share_bps, cuts_out, amounts.net_in, amounts.out_gross)
            .ok_or(SwapError::MathOverflow)?
    } else {
        0
    };
    let from_reserve = protocol_out
        .checked_sub(held_back)
        .ok_or(SwapError::MathOverflow)?;
    {
        let pool = &mut ctx.accounts.pool;
        pool.quote_reserve = pool
            .quote_reserve
            .checked_sub(from_reserve)
            .ok_or(SwapError::InsufficientLiquidity)?;
        pool.protocol_fees_quote = pool
            .protocol_fees_quote
            .checked_add(protocol_out)
            .ok_or(SwapError::MathOverflow)?;
    }
    let protocol_fee = amounts
        .protocol_fee
        .checked_add(protocol_out)
        .ok_or(SwapError::MathOverflow)?;

    let pool = &ctx.accounts.pool;
    emit_cpi!(Swapped {
        pool: pool_key,
        trader: ctx.accounts.trader.key(),
        recipient,
        direction,
        amount_in,
        deltas_in: cut_in.paid(),
        burn_in: cut_in.burn,
        cuts_in,
        received_in: received,
        lp_fee: amounts.lp_fee,
        protocol_fee,
        lp_fee_bps,
        amount_out: amounts.out_gross,
        deltas_out: cut_out.paid(),
        burn_out: cut_out.burn,
        cuts_out,
        delivered_out: delivered,
        base_reserve: pool.base_reserve,
        quote_reserve: pool.quote_reserve,
        virtual_base: pool.virtual_base,
        virtual_quote: pool.virtual_quote,
        swap_count: pool.swap_count,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}
