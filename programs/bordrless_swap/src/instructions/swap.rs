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
//!
//! A fill: a buy on a curve whose `before_swap` answer ends with one (`bordrless_hook`'s
//! `read_filled_answer`; a hook may fill when its pool's flags let it cut the input,
//! `BEFORE_SWAP_RETURNS_DELTA`) uses that much of its input instead of all of it, the hook's cuts
//! being of that part, and takes every token the curve has left rather than failing past its end
//! (`swap_out_filling`); the rest of the input never leaves the trader, and `min_amount_out` is
//! held to pro rata (the least the trader accepted for the whole input, for the part used). A fill
//! of zero, above the input, for a sell or on a pool that is not a curve is `BadFill`. The
//! launchpad answers one when a buy reaches the end of its curve, which then sells out and can
//! graduate in the same transaction.
//!
//! Native pools (`crate::native`): `swap_v2` trades a native pool's quote as lamports, the trader's
//! wallet paying a buy by a system transfer to the pool and the pool's lamports paying a sell, and
//! the quote-side cuts paid as lamports (or wrapped, to a holding of bridged SOL). The old `swap`
//! keeps working on a native pool, with the same accounts, through the pool's buffer: a buy's
//! bridged SOL joins it, a sell is paid from it (`BufferShort` when it cannot cover the sell).

use anchor_lang::prelude::*;
use bordrless_core::{
    mul_div_floor, output_share, protocol_share, swap_amounts_shared_with, swap_amounts_with,
    Reserves,
};
use bordrless_hook::{
    discriminators, pool_flags, Allowed, Phase, PoolHookArgs, PoolOp, MAX_HOOK_DATA,
};

use crate::constants::*;
use crate::error::{swap_failure, SwapError};
use crate::events::*;
use crate::hooks::{split_extras, Cut, NativeCut, PoolHookCall};
use crate::instructions::pool::PoolSeeds;
use crate::native::{
    buffer_in, buffer_out, check_credit_rent, check_native, move_lamports, transfer_from_wallet,
    BridgeSol,
};
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

/// Accounts of `swap_v2`, a swap on a native pool: as `swap`'s, without the quote vault, with
/// the trader's wallet as `trader_quote` (on a buy the trader itself, which pays; on a sell any
/// account to deliver the lamports to), the System Program, and the bridge's accounts, needed
/// only when a quote-side cut names a holding of bridged SOL (then the quote mint is passed
/// writable). The remaining accounts are as `swap`'s.
#[event_cpi]
#[derive(Accounts)]
pub struct SwapV2<'info> {
    /// The trader: owner (or delegate) of the input holding; pays a buy's lamports.
    #[account(mut)]
    pub trader: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, constraint = pool.quote_native() @ SwapError::NotNativePool)]
    pub pool: Account<'info, Pool>,
    /// CHECK: address-checked; writable only for a burn of base.
    #[account(address = pool.base_mint @ SwapError::WrongHolding)]
    pub base_mint: UncheckedAccount<'info>,
    /// CHECK: address-checked (bridged SOL); writable only to wrap a cut.
    #[account(address = pool.quote_mint @ SwapError::WrongHolding)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = pool.base_vault @ SwapError::WrongVault)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: the trader's base holding, or on a buy any holding of the base mint to deliver to
    /// (checked by the token program).
    #[account(mut)]
    pub trader_base: UncheckedAccount<'info>,
    /// CHECK: on a buy the trader (checked in the handler); on a sell any account to deliver the
    /// lamports to.
    #[account(mut)]
    pub trader_quote: UncheckedAccount<'info>,
    /// CHECK: the pool's hook program (checked in the handler).
    pub hook_program: Option<UncheckedAccount<'info>>,
    /// CHECK: with a hook, this program's signer of its callbacks (checked in the handler).
    pub hook_signer: Option<UncheckedAccount<'info>>,
    /// CHECK: the token program.
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    /// CHECK: the bridge (checked when used).
    pub bridge_program: Option<UncheckedAccount<'info>>,
    /// CHECK: the bridge's config (the bridge checks it).
    pub bridge_config: Option<UncheckedAccount<'info>>,
    /// CHECK: the SOL wrapper (the bridge checks it).
    #[account(mut)]
    pub sol_wrapper: Option<UncheckedAccount<'info>>,
    /// CHECK: the bridge's SOL vault (the bridge checks it).
    #[account(mut)]
    pub sol_vault: Option<UncheckedAccount<'info>>,
    /// CHECK: the bridge's event authority (the bridge checks it).
    pub bridge_event_authority: Option<UncheckedAccount<'info>>,
}

/// The native leg of a `swap_v2`.
pub struct NativeLeg<'info> {
    /// The System Program.
    pub system_program: AccountInfo<'info>,
    /// The bridge, when its accounts were passed.
    pub bridge: Option<BridgeSol<'info>>,
}

impl<'info> NativeLeg<'info> {
    fn bridge(&self) -> Result<&BridgeSol<'info>> {
        let bridge = self
            .bridge
            .as_ref()
            .ok_or(SwapError::BridgeAccountsMissing)?;
        bridge.check()?;
        Ok(bridge)
    }
}

/// What a swap is run on: the accounts of `swap` or of `swap_v2`.
pub struct SwapCore<'a, 'info> {
    pub trader: AccountInfo<'info>,
    pub paused: bool,
    pub pool: &'a mut Account<'info, Pool>,
    pub base_mint: AccountInfo<'info>,
    pub quote_mint: AccountInfo<'info>,
    pub base_vault: AccountInfo<'info>,
    /// `swap`: the quote vault. `swap_v2`: none (the quote is the pool's lamports).
    pub quote_vault: Option<AccountInfo<'info>>,
    pub trader_base: AccountInfo<'info>,
    pub trader_quote: AccountInfo<'info>,
    pub hook_program: Option<AccountInfo<'info>>,
    pub hook_signer: Option<AccountInfo<'info>>,
    pub token: TokenAccounts<'info>,
    /// `swap_v2` only.
    pub native: Option<NativeLeg<'info>>,
    pub remaining: &'info [AccountInfo<'info>],
}

/// `swap`.
pub fn process_swap<'info>(mut ctx: Context<'info, Swap<'info>>, args: SwapArgs) -> Result<()> {
    let a = &mut ctx.accounts;
    let core = SwapCore {
        trader: a.trader.to_account_info(),
        paused: a.config.paused,
        pool: &mut a.pool,
        base_mint: a.base_mint.to_account_info(),
        quote_mint: a.quote_mint.to_account_info(),
        base_vault: a.base_vault.to_account_info(),
        quote_vault: Some(a.quote_vault.to_account_info()),
        trader_base: a.trader_base.to_account_info(),
        trader_quote: a.trader_quote.to_account_info(),
        hook_program: a.hook_program.as_ref().map(|x| x.to_account_info()),
        hook_signer: a.hook_signer.as_ref().map(|x| x.to_account_info()),
        token: TokenAccounts {
            program: a.token_program.to_account_info(),
            event_authority: a.token_event_authority.to_account_info(),
        },
        native: None,
        remaining: ctx.remaining_accounts,
    };
    let (swapped, _) = run_swap(core, args)?;
    emit_cpi!(swapped);
    Ok(())
}

/// `swap_v2`.
pub fn process_swap_v2<'info>(
    mut ctx: Context<'info, SwapV2<'info>>,
    args: SwapArgs,
) -> Result<()> {
    let a = &mut ctx.accounts;
    let info = |x: &Option<UncheckedAccount<'info>>| x.as_ref().map(|x| x.to_account_info());
    let bridge = match (
        info(&a.bridge_program),
        info(&a.bridge_config),
        info(&a.sol_wrapper),
        info(&a.sol_vault),
        info(&a.bridge_event_authority),
    ) {
        (Some(program), Some(config), Some(wrapper), Some(sol_vault), Some(event_authority)) => {
            Some(BridgeSol {
                program,
                config,
                wrapper,
                sol_vault,
                wrapped_mint: a.quote_mint.to_account_info(),
                event_authority,
                token_program: a.token_program.to_account_info(),
                token_event_authority: a.token_event_authority.to_account_info(),
                system_program: a.system_program.to_account_info(),
            })
        }
        _ => None,
    };
    let core = SwapCore {
        trader: a.trader.to_account_info(),
        paused: a.config.paused,
        pool: &mut a.pool,
        base_mint: a.base_mint.to_account_info(),
        quote_mint: a.quote_mint.to_account_info(),
        base_vault: a.base_vault.to_account_info(),
        quote_vault: None,
        trader_base: a.trader_base.to_account_info(),
        trader_quote: a.trader_quote.to_account_info(),
        hook_program: info(&a.hook_program),
        hook_signer: info(&a.hook_signer),
        token: TokenAccounts {
            program: a.token_program.to_account_info(),
            event_authority: a.token_event_authority.to_account_info(),
        },
        native: Some(NativeLeg {
            system_program: a.system_program.to_account_info(),
            bridge,
        }),
        remaining: ctx.remaining_accounts,
    };
    let (swapped, native) = run_swap(core, args)?;
    emit_cpi!(swapped);
    if let Some(native) = native {
        emit_cpi!(native);
    }
    Ok(())
}

/// A swap on `c` (see the module).
pub fn run_swap<'info>(
    c: SwapCore<'_, 'info>,
    args: SwapArgs,
) -> Result<(Swapped, Option<SwappedNative>)> {
    let clock = Clock::get()?;
    let SwapArgs {
        direction,
        amount_in: requested,
        min_amount_out,
        in_hook_accounts,
        out_hook_accounts,
        hook_data,
    } = args;
    // What the swap uses: the whole input, or the part a fill names.
    let mut amount_in = requested;
    require!(!c.paused, SwapError::Paused);
    require!(amount_in > 0, SwapError::ZeroAmount);
    require!(direction <= 1, SwapError::InvalidDirection);
    require!(hook_data.len() <= MAX_HOOK_DATA, SwapError::HookDataTooLong);
    let buy = direction == 1;
    let token = c.token;
    token.check()?;
    let (in_extras, out_extras, pool_extras) =
        split_extras(c.remaining, in_hook_accounts, out_hook_accounts)?;
    let pool_acc = c.pool;
    let pool_info = pool_acc.to_account_info();
    let pool_key = pool_acc.key();
    // `swap_v2`: the quote side is lamports (the pool's own account stands for its vault).
    let native = c.native;
    let native_in = native.is_some() && buy;
    let native_out = native.is_some() && !buy;
    // The old `swap` on a native pool: the quote side goes through the buffer.
    let buffered = native.is_none() && pool_acc.quote_native();
    let quote_side = c.quote_vault.clone().unwrap_or_else(|| pool_info.clone());

    // The sides.
    let base_mint = c.base_mint.clone();
    let quote_mint = c.quote_mint.clone();
    let (in_mint, out_mint, vault_in, vault_out, trader_in, trader_out) = if buy {
        (
            quote_mint.clone(),
            base_mint.clone(),
            quote_side.clone(),
            c.base_vault.clone(),
            c.trader_quote.clone(),
            c.trader_base.clone(),
        )
    } else {
        (
            base_mint.clone(),
            quote_mint.clone(),
            c.base_vault.clone(),
            quote_side.clone(),
            c.trader_base.clone(),
            c.trader_quote.clone(),
        )
    };
    let in_hook = bordrless_token::client::read_mint(&in_mint)?.hook_program;
    let out_hook = bordrless_token::client::read_mint(&out_mint)?.hook_program;
    // A native sell delivers to an account, not a holding: the account is the recipient.
    let recipient = if native_out {
        *trader_out.key
    } else {
        read_holding(&trader_out)?.owner
    };
    // A native buy is paid by the trader's own wallet.
    if native_in {
        require_keys_eq!(*trader_in.key, *c.trader.key, SwapError::WrongNativeAccount);
    }
    // A native sell delivers lamports: never to the pool or its vaults, nor to any account of the
    // token program (a holding of bridged SOL passed out of habit would keep them as lamports,
    // stranded, not as bridged SOL).
    if native_out {
        require!(
            *trader_out.key != pool_key
                && *trader_out.key != pool_acc.base_vault
                && *trader_out.key != pool_acc.quote_vault
                && *trader_out.owner != bordrless_token::ID,
            SwapError::WrongNativeAccount
        );
    }
    // No delta may go to a vault or to either of the trader's holdings (on a native swap, nor to
    // the pool, its quote vault or the trader).
    let mut forbidden = vec![
        *vault_in.key,
        *vault_out.key,
        *trader_in.key,
        *trader_out.key,
    ];
    if native.is_some() {
        forbidden.extend([pool_key, pool_acc.quote_vault, *c.trader.key]);
    }

    let call = PoolHookCall::of(
        pool_acc.hook_program,
        pool_acc.hook_signer_bump,
        c.hook_program,
        c.hook_signer,
        [pool_info.clone(), base_mint, quote_mint, c.trader.clone()],
        pool_extras,
    )?;
    let pool: &Pool = pool_acc;
    let flags = pool.hook_flags;
    let mut hook_args = PoolHookArgs {
        op: PoolOp::Swap,
        phase: Phase::Before,
        pool: pool_key,
        base_mint: pool.base_mint,
        quote_mint: pool.quote_mint,
        actor: c.trader.key(),
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

    // 1. Before: the hook may set the LP fee, cut the input (deltas and a burn), as the pool's
    //    flags allow, and fill a buy on a curve (the part of the input the swap uses, its cuts of
    //    that part); the answer is checked before anything moves.
    let mut lp_fee_bps = pool.lp_fee_bps;
    let mut cut_in = Cut::none();
    let mut ncut_in = NativeCut::none();
    let mut filled = false;
    if let Some(call) = &call {
        if pool.runs(pool_flags::BEFORE_SWAP) {
            let allowed = Allowed::pool(PoolOp::Swap, Phase::Before, flags);
            // A hook that may cut the input (`BEFORE_SWAP_RETURNS_DELTA`) may fill a buy.
            if let Some((ret, taken, fill)) = call.invoke_filled(
                discriminators::BEFORE_SWAP,
                &hook_args,
                allowed,
                allowed.deltas,
            )? {
                if let Some(fee) = ret.lp_fee_bps {
                    require!(fee <= MAX_LP_FEE_BPS, SwapError::FeeTooHigh);
                    lp_fee_bps = fee;
                }
                if let Some(fill) = fill {
                    // Only a buy the transaction sends the DEX itself (a program that buys through
                    // the DEX counts on its whole input being spent), on a launch pool's curve (a
                    // curve another program opens as its hook may not fill: it could keep a buyer's
                    // whole input for the last tokens).
                    let direct = anchor_lang::solana_program::instruction::get_stack_height()
                        == anchor_lang::solana_program::instruction::TRANSACTION_LEVEL_STACK_HEIGHT;
                    require!(
                        buy && pool.curve
                            && pool.shares_cuts()
                            && direct
                            && fill > 0
                            && fill <= requested,
                        SwapError::BadFill
                    );
                    amount_in = fill;
                    filled = true;
                }
                if native_in {
                    ncut_in = call.native_cut(&ret, taken, amount_in, &forbidden)?;
                } else {
                    cut_in = call.cut(&ret, taken, amount_in, &in_mint, &forbidden)?;
                }
            }
        }
    }
    let (cut_in_taken, cut_in_burn) = if native_in {
        (ncut_in.taken, 0)
    } else {
        (cut_in.taken, cut_in.burn)
    };

    // 2. The input: the cut from the trader's holding (each delta, then the burn), then the rest
    //    to the vault, measured there. A native buy: each delta from the trader's wallet (as
    //    lamports, or wrapped into a holding of bridged SOL), then the rest to the pool.
    let trader = c.trader.clone();
    let received = if let (true, Some(leg)) = (native_in, native.as_ref()) {
        for (to, amount, wrapped) in &ncut_in.deltas {
            if *wrapped {
                leg.bridge()?.wrap_from_wallet(&trader, to, *amount)?;
            } else {
                check_credit_rent(to, *amount)?;
                transfer_from_wallet(&leg.system_program, &trader, to, *amount)?;
            }
        }
        let before = pool_info.lamports();
        transfer_from_wallet(
            &leg.system_program,
            &trader,
            &pool_info,
            amount_in - cut_in_taken,
        )?;
        pool_info
            .lamports()
            .checked_sub(before)
            .ok_or(SwapError::MathOverflow)?
    } else {
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
            amount_in - cut_in_taken,
            &[],
        )?;
        balance(&vault_in)?
            .checked_sub(vault_in_before)
            .ok_or(SwapError::MathOverflow)?
    };
    require!(received > 0, SwapError::NothingReceived);
    // What the hooks took from the input and someone received: the pool hook's deltas and any
    // cut the input mint's own hook took from those transfers (measured, in the input token).
    let cuts_in = amount_in
        .checked_sub(cut_in_burn)
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
        swap_amounts_shared_with(
            buy, received, lp_fee_bps, share_bps, cuts_in, &reserves, filled,
        )
    } else {
        swap_amounts_with(
            buy,
            received,
            lp_fee_bps,
            pool.protocol_fee_bps,
            &reserves,
            filled,
        )
    }
    .map_err(swap_failure)?;

    // 4. Reserves: `to_reserve_in` enters the input reserve (`received` less a buy's protocol fee,
    //    and less the LP fee too under the share model); the curve's
    //    whole output leaves the output reserve (the hook's cut, the sell's protocol fee and the
    //    delivery all come out of it); the protocol fee is set aside in the quote vault.
    {
        let pool: &mut Pool = pool_acc;
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
            // The old swap on a native pool: the bridged SOL joined the buffer.
            if buffered {
                buffer_in(pool, received)?;
            }
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
    let mut ncut_out = NativeCut::none();
    if let Some(call) = &call {
        if pool_acc.runs(pool_flags::AFTER_SWAP) {
            pool_acc.exit(&crate::ID)?;
            hook_args.phase = Phase::After;
            hook_args.amount_in = received;
            hook_args.amount_out = amounts.amount_out;
            hook_args.base_reserve = pool_acc.base_reserve;
            hook_args.quote_reserve = pool_acc.quote_reserve;
            hook_args.lp_fee_bps = lp_fee_bps;
            let allowed = Allowed::pool(PoolOp::Swap, Phase::After, flags);
            if let Some((ret, taken)) =
                call.invoke(discriminators::AFTER_SWAP, &hook_args, allowed)?
            {
                if native_out {
                    ncut_out = call.native_cut(&ret, taken, amounts.amount_out, &forbidden)?;
                } else {
                    cut_out = call.cut(&ret, taken, amounts.amount_out, &out_mint, &forbidden)?;
                }
            }
        }
    }
    let (cut_out_taken, cut_out_burn) = if native_out {
        (ncut_out.taken, 0)
    } else {
        (cut_out.taken, cut_out.burn)
    };

    // 6. The output: the cut from the vault (each delta, then the burn; the pool signs), then the
    //    rest to the recipient, measured there. Under the share model a sell holds back the pool's
    //    share of the hook's quote deltas from the delivery (the trader pays it). A native sell:
    //    each delta and the delivery from the pool's lamports.
    let held_back = if shared && !buy {
        let deltas_sum = cut_out_taken - cut_out_burn;
        protocol_share(deltas_sum, share_bps).ok_or(SwapError::MathOverflow)?
    } else {
        0
    };
    let to_deliver = amounts
        .amount_out
        .checked_sub(cut_out_taken)
        .and_then(|rest| rest.checked_sub(held_back))
        .filter(|rest| *rest > 0)
        .ok_or(SwapError::FeeExceedsOutput)?;
    let seeds = PoolSeeds::of(pool_acc);
    let pool_seeds = seeds.seeds();
    let delivered = if let (true, Some(leg)) = (native_out, native.as_ref()) {
        // The wrapped deltas first: each is a CPI, at which the runtime must see every lamport
        // this program has moved so far on accounts it passes, so no direct move precedes them.
        for (to, amount, _) in ncut_out.deltas.iter().filter(|d| d.2) {
            leg.bridge()?
                .wrap_from_pool(&pool_info, &trader, to, *amount)?;
        }
        for (to, amount, _) in ncut_out.deltas.iter().filter(|d| !d.2) {
            check_credit_rent(to, *amount)?;
            move_lamports(&pool_info, to, *amount)?;
        }
        let before = trader_out.lamports();
        check_credit_rent(&trader_out, to_deliver)?;
        move_lamports(&pool_info, &trader_out, to_deliver)?;
        trader_out
            .lamports()
            .checked_sub(before)
            .ok_or(SwapError::MathOverflow)?
    } else {
        // The old swap's sell on a native pool is paid from the buffer, which must cover it.
        let buffer_before = if buffered && !buy {
            let leaving = cut_out_taken
                .checked_add(to_deliver)
                .ok_or(SwapError::MathOverflow)?;
            require!(pool_acc.buffer_quote() >= leaving, SwapError::BufferShort);
            Some(balance(&vault_out)?)
        } else {
            None
        };
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
        if let Some(before) = buffer_before {
            let left = before
                .checked_sub(balance(&vault_out)?)
                .ok_or(SwapError::MathOverflow)?;
            buffer_out(pool_acc, left)?;
        }
        balance(&trader_out)?
            .checked_sub(trader_out_before)
            .ok_or(SwapError::MathOverflow)?
    };
    // A fill holds the minimum to pro rata: what the trader accepted for the whole input, for the
    // part the swap used.
    let min_out = if amount_in < requested {
        mul_div_floor(
            u128::from(min_amount_out),
            u128::from(amount_in),
            u128::from(requested),
        )
        .and_then(|m| u64::try_from(m).ok())
        .ok_or(SwapError::MathOverflow)?
    } else {
        min_amount_out
    };
    require!(delivered >= min_out, SwapError::Slippage);
    // What the hooks took from the output and someone received (measured, in the output token).
    let cuts_out = amounts
        .amount_out
        .checked_sub(cut_out_burn)
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
        let pool: &mut Pool = pool_acc;
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
    // A native pool's books against its lamports and buffer.
    check_native(&pool_info, pool_acc, c.quote_vault.as_ref())?;

    let pool: &Pool = pool_acc;
    let (deltas_in, deltas_out) = (
        if native_in {
            ncut_in.paid()
        } else {
            cut_in.paid()
        },
        if native_out {
            ncut_out.paid()
        } else {
            cut_out.paid()
        },
    );
    let native_event = native.as_ref().map(|_| SwappedNative {
        pool: pool_key,
        direction,
        quote_lamports: if buy { received } else { delivered },
        quote_deltas: if buy {
            ncut_in.native_paid()
        } else {
            ncut_out.native_paid()
        },
        buffer_quote: pool.buffer_quote(),
        pool_lamports: pool_info.lamports(),
    });
    let swapped = Swapped {
        pool: pool_key,
        trader: c.trader.key(),
        recipient,
        direction,
        amount_in,
        deltas_in,
        burn_in: cut_in_burn,
        cuts_in,
        received_in: received,
        lp_fee: amounts.lp_fee,
        protocol_fee,
        lp_fee_bps,
        amount_out: amounts.out_gross,
        deltas_out,
        burn_out: cut_out_burn,
        cuts_out,
        delivered_out: delivered,
        base_reserve: pool.base_reserve,
        quote_reserve: pool.quote_reserve,
        virtual_base: pool.virtual_base,
        virtual_quote: pool.virtual_quote,
        swap_count: pool.swap_count,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    };
    Ok((swapped, native_event))
}
