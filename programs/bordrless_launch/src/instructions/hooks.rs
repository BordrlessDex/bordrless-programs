//! The pool hook callbacks (`docs/hooks-v2.md` §5.4): the DEX calls these, signing with its
//! signer for this program, `["hook-authority", LAUNCH_ID]` under the DEX ([`DEX_HOOK_AUTHORITY`]).
//! Only the DEX can sign for it, and only when it calls this program as a pool's hook: the signer
//! it gives any other pool hook is that hook's own, so a hook that passes its signer on in a CPI
//! here is refused (`BadHookSigner`), and the arguments a callback trusts are the DEX's.
//! `before_swap` sets the LP fee (the sniper schedule) and cuts the input: a buy's creator and
//! holder fees, a sell's burn. `after_swap` cuts the output: a buy's burn, a sell's creator and
//! holder fees. The math is `bordrless_core`'s ([`launch_before_swap`], [`launch_after_swap`]):
//! fees round up, burns round down, creator + holder is dropped when it is not below the amount,
//! a burn when it is not below the amount, and the holder fee is taken only while holders hold at
//! least the kit's threshold.
//!
//! A launch whose custom hook takes a SOL cut (`docs/quote-cut.md`) also asks the hook's
//! `quote_cut` on each trade (a buy before the swap, a sell after it) and takes its answer, capped
//! by the config, from the same SOL as the creator fee, into the same holding. The creator's
//! trades to itself (a companion's, for a companion coin) and the token's hook-vault slots' skip
//! it: it would pay the creator's side from the creator's side.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program::{get_return_data, invoke_signed};
use bordrless_core::{launch_after_swap, launch_before_swap, sniper_lp_fee, HookCut};
use bordrless_hook::{clear_return_data, Delta, HookReturn, PoolHookArgs};
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::LaunchError;
use crate::events::QuoteCutTaken;
use crate::state::*;

/// Accounts of `before_initialize`: the prefix only. There is never a launch to find, so the
/// handler refuses.
#[derive(Accounts)]
pub struct RejectInitialize<'info> {
    /// CHECK: the DEX's signer for this program (address- and signer-checked).
    #[account(signer, address = DEX_HOOK_AUTHORITY @ LaunchError::BadHookSigner)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: unused.
    pub pool: UncheckedAccount<'info>,
    /// CHECK: unused.
    pub base_mint: UncheckedAccount<'info>,
    /// CHECK: unused.
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: unused.
    pub actor: UncheckedAccount<'info>,
}

/// `before_initialize`.
pub fn process_before_initialize(
    _ctx: Context<RejectInitialize>,
    _args: PoolHookArgs,
) -> Result<HookReturn> {
    err!(LaunchError::OnlyLaunchpadCreatesPools)
}

/// Accounts of `before_swap` and `after_swap`: the prefix, then the registry's extras at 5 to 8:
/// the launch, its quote holding, the holder vault and the kit config.
#[derive(Accounts)]
pub struct HookCallback<'info> {
    /// CHECK: the DEX's signer for this program (address- and signer-checked).
    #[account(signer, address = DEX_HOOK_AUTHORITY @ LaunchError::BadHookSigner)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: the pool (must be the launch's).
    pub pool: UncheckedAccount<'info>,
    /// CHECK: the base mint (must be the launch's).
    pub base_mint: UncheckedAccount<'info>,
    /// CHECK: the quote mint.
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: the trader.
    pub actor: UncheckedAccount<'info>,
    /// The launch of the base mint. Only `create_launch` creates a `Launch`, at
    /// `["launch", mint]` with that mint, so the owner, the discriminator and the mint bind it
    /// without a derivation.
    #[account(
        mut,
        constraint = launch.mint == base_mint.key() @ LaunchError::WrongPool,
        constraint = launch.pool == pool.key() @ LaunchError::WrongPool
    )]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: the launch's quote holding (address-checked); receives creator fees.
    #[account(mut, address = launch.quote_holding @ LaunchError::WrongHolding)]
    pub launch_quote: UncheckedAccount<'info>,
    /// CHECK: the holder vault (address-checked); receives holder fees (the DEX checks it is a
    /// writable holding of the quote before paying it).
    #[account(address = launch.holder_vault @ LaunchError::WrongHolderVault)]
    pub holder_vault: UncheckedAccount<'info>,
    /// CHECK: the kit config (address-checked); read, through the kit crate, only when holder
    /// rewards are on.
    #[account(address = launch.kit_config @ LaunchError::WrongKitAccount)]
    pub kit_config: UncheckedAccount<'info>,
}

/// The kit's eligible supply and threshold, read when this side takes a holder fee (which holder
/// rewards being on implies); zeros otherwise, as nothing reads them then.
fn eligibility(ctx: &Context<'_, HookCallback<'_>>, holder_fee_bps: u16) -> Result<(u64, u64)> {
    if holder_fee_bps == 0 || !ctx.accounts.launch.rewards_on() {
        return Ok((0, 0));
    }
    let kit = bordrless_kit::client::read_kit_config(&ctx.accounts.kit_config)?;
    require_keys_eq!(
        kit.mint,
        ctx.accounts.launch.mint,
        LaunchError::WrongKitAccount
    );
    Ok((kit.eligible, kit.min_eligible))
}

/// The answer that takes `cut`: the creator fee to the launch's quote holding, the holder fee to
/// the holder vault, each only when above zero, and the burn; the launch counts each. The
/// counters are running totals for display (`claim_creator_fees` pays the quote holding's
/// balance, not a counter), so they saturate: a counter never blocks a trade.
fn answer(launch: &mut Launch, cut: &HookCut, lp_fee_bps: Option<u16>) -> HookReturn {
    let mut ret = HookReturn {
        lp_fee_bps,
        burn: cut.burn,
        ..HookReturn::default()
    };
    if cut.creator_fee > 0 {
        ret.deltas.push(Delta {
            amount: cut.creator_fee,
            account: QUOTE_HOLDING_INDEX,
        });
        launch.creator_fees_accrued = launch.creator_fees_accrued.saturating_add(cut.creator_fee);
    }
    if cut.holder_fee > 0 {
        ret.deltas.push(Delta {
            amount: cut.holder_fee,
            account: HOLDER_VAULT_INDEX,
        });
        launch.holder_fees_accrued = launch.holder_fees_accrued.saturating_add(cut.holder_fee);
    }
    launch.burned_on_trades = launch.burned_on_trades.saturating_add(cut.burn);
    ret
}

/// Whether `actor` is one of `mint`'s `hook_vault` slots, which sell and buy the hook's token cut
/// on the token's own pool through `hook_vault`, whose swaps pass the pool's four extras only.
fn is_vault_slot(actor: &Pubkey, mint: &Pubkey) -> bool {
    (0..HOOK_VAULT_MAX_SLOTS).any(|i| {
        Pubkey::find_program_address(&[HOOK_VAULT_SLOT_SEED, mint.as_ref(), &[i]], &HOOK_VAULT_ID).0
            == *actor
    })
}

/// `amount * bps / 10_000`, rounded down: a dust trade pays no SOL cut rather than failing on it
/// (the DEX refuses a swap whose fees take its whole input).
fn fee_of(amount: u64, bps: u16) -> u64 {
    let product = u128::from(amount) * u128::from(bps);
    u64::try_from(product / 10_000).unwrap_or(u64::MAX)
}

/// The custom hook's SOL cut of this trade (`docs/quote-cut.md`): 0 without one on this side, for
/// the creator's trades to itself and for the token's hook-vault slots'; else the hook's `quote_cut`
/// answer (a `u16` of basis points in its return data; none is 0), capped by the launch, of
/// `amount` (a buy's input, a sell's output), rounded down, dropped when it and `taken` (the creator
/// and holder fees) would not stay below `amount`. The trade fails when the accounts are missing
/// or wrong, or the hook fails or answers anything else.
fn quote_cut<'info>(
    ctx: &Context<'info, HookCallback<'info>>,
    args: &PoolHookArgs,
    buy: bool,
    amount: u64,
    taken: u64,
) -> Result<u64> {
    let launch = &ctx.accounts.launch;
    let Some((hook, cut)) = launch.quote_cut() else {
        return Ok(0);
    };
    let max_bps = cut.max_bps(buy);
    // The creator trading for itself (a companion's buys, sells and scripts all trade creator to
    // creator): the cut would pay the creator's side from the creator's side. Not when it delivers
    // to someone else, or it could relay cut-free buys for anyone.
    if max_bps == 0 || (args.actor == launch.creator && args.recipient == launch.creator) {
        return Ok(0);
    }
    let extras = ctx.remaining_accounts;
    if extras.len() < QUOTE_CUT_EXTRAS {
        require!(
            is_vault_slot(&args.actor, &launch.mint),
            LaunchError::QuoteCutAccountsMissing
        );
        return Ok(0);
    }
    let (state_bump, signer_bump) = launch.quote_cut_bumps();
    let [program, holding, state, signer] = [&extras[0], &extras[1], &extras[2], &extras[3]];
    let state_key = Pubkey::create_program_address(
        &[HOOK_STATE_SEED, launch.mint.as_ref(), &[state_bump]],
        &hook,
    )
    .map_err(|_| error!(LaunchError::QuoteCutWrongAccount))?;
    let signer_seeds: &[&[u8]] = &[QUOTE_CUT_SIGNER_SEED, hook.as_ref(), &[signer_bump]];
    let signer_key = Pubkey::create_program_address(signer_seeds, &crate::ID)
        .map_err(|_| error!(LaunchError::QuoteCutWrongAccount))?;
    require!(
        *program.key == hook
            && *holding.key == token_client::holding_address(&launch.mint, &args.actor)
            && *state.key == state_key
            && state.is_writable
            && *signer.key == signer_key,
        LaunchError::QuoteCutWrongAccount
    );

    let mut data = Vec::with_capacity(8 + 256 + args.hook_data.len());
    data.extend_from_slice(&QUOTE_CUT_DISCRIMINATOR);
    args.serialize(&mut data)?;
    let ix = Instruction {
        program_id: hook,
        accounts: vec![
            AccountMeta::new_readonly(signer_key, true),
            AccountMeta::new_readonly(launch.key(), false),
            AccountMeta::new_readonly(ctx.accounts.pool.key(), false),
            AccountMeta::new_readonly(ctx.accounts.base_mint.key(), false),
            AccountMeta::new_readonly(ctx.accounts.quote_mint.key(), false),
            AccountMeta::new_readonly(ctx.accounts.actor.key(), false),
            AccountMeta::new_readonly(*holding.key, false),
            AccountMeta::new(state_key, false),
        ],
        data,
    };
    clear_return_data();
    invoke_signed(
        &ix,
        &[
            signer.clone(),
            launch.to_account_info(),
            ctx.accounts.pool.to_account_info(),
            ctx.accounts.base_mint.to_account_info(),
            ctx.accounts.quote_mint.to_account_info(),
            ctx.accounts.actor.to_account_info(),
            holding.clone(),
            state.clone(),
            program.clone(),
        ],
        &[signer_seeds],
    )?;
    let answered = match get_return_data() {
        Some((from, bytes)) if from == hook => {
            let raw: [u8; 2] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| error!(LaunchError::QuoteCutBadAnswer))?;
            u16::from_le_bytes(raw)
        }
        _ => 0,
    };
    let bps = answered.min(max_bps);
    let fee = fee_of(amount, bps);
    let fee = match taken.checked_add(fee) {
        Some(total) if total < amount => fee,
        _ => 0,
    };
    if fee > 0 {
        emit!(QuoteCutTaken {
            launch: launch.key(),
            actor: args.actor,
            direction: args.direction,
            answered_bps: answered,
            bps,
            amount: fee,
        });
    }
    Ok(fee)
}

/// `cut` with the hook's SOL cut `fee` added to the creator fee (same holding, one delta), counted.
fn with_quote_cut(launch: &mut Launch, mut cut: HookCut, fee: u64) -> HookCut {
    if fee > 0 {
        cut.creator_fee = cut.creator_fee.saturating_add(fee);
        launch.add_quote_cut(fee);
    }
    cut
}

/// `before_swap`: the LP fee (the sniper schedule; the creator's own first buy into its own
/// wallet inside the window pays the normal fee, once), then the cut from the input.
pub fn process_before_swap<'info>(
    ctx: Context<'info, HookCallback<'info>>,
    args: PoolHookArgs,
) -> Result<HookReturn> {
    require_keys_eq!(args.pool, ctx.accounts.launch.pool, LaunchError::WrongPool);
    let now = Clock::get()?.unix_timestamp;
    let buy = args.direction == 1;
    let rates = {
        let l = &ctx.accounts.launch;
        l.rules.fee_rates(l.creator_fee_bps)
    };
    let (eligible, min_eligible) =
        eligibility(&ctx, if buy { rates.holder_fee_buy_bps } else { 0 })?;
    let launch = &mut ctx.accounts.launch;
    let own_first = buy
        && args.actor == launch.creator
        && args.recipient == launch.creator
        && !launch.creator_bought
        && now < launch.created_at.saturating_add(launch.sniper_window_secs);
    let lp_fee = if own_first {
        launch.creator_bought = true;
        launch.lp_fee_bps
    } else {
        sniper_lp_fee(
            now,
            launch.created_at,
            launch.sniper_window_secs,
            launch.sniper_start_bps,
            launch.lp_fee_bps,
        )
    };
    let cut = launch_before_swap(buy, args.amount_in, &rates, eligible, min_eligible);
    // A buy's SOL cut, from the input with the creator fee.
    let fee = if buy {
        quote_cut(
            &ctx,
            &args,
            true,
            args.amount_in,
            cut.creator_fee.saturating_add(cut.holder_fee),
        )?
    } else {
        0
    };
    let launch = &mut ctx.accounts.launch;
    let cut = with_quote_cut(launch, cut, fee);
    Ok(answer(launch, &cut, Some(lp_fee)))
}

/// `after_swap`: the cut from the output the DEX hands on (a buy: the curve's output; a sell: the
/// curve's output less the DEX's protocol fee, which is always in the quote).
pub fn process_after_swap<'info>(
    ctx: Context<'info, HookCallback<'info>>,
    args: PoolHookArgs,
) -> Result<HookReturn> {
    require_keys_eq!(args.pool, ctx.accounts.launch.pool, LaunchError::WrongPool);
    let buy = args.direction == 1;
    let rates = {
        let l = &ctx.accounts.launch;
        l.rules.fee_rates(l.creator_fee_bps)
    };
    // A sell's input has left the seller by now: the kit counts it out of the eligible supply.
    let (eligible, min_eligible) =
        eligibility(&ctx, if buy { 0 } else { rates.holder_fee_sell_bps })?;
    let cut = launch_after_swap(buy, args.amount_out, &rates, eligible, min_eligible);
    // A sell's SOL cut, from the output with the creator fee.
    let fee = if buy {
        0
    } else {
        quote_cut(
            &ctx,
            &args,
            false,
            args.amount_out,
            cut.creator_fee.saturating_add(cut.holder_fee),
        )?
    };
    let launch = &mut ctx.accounts.launch;
    let cut = with_quote_cut(launch, cut, fee);
    Ok(answer(launch, &cut, None))
}
