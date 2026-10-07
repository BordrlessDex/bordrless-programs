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

use anchor_lang::prelude::*;
use bordrless_core::{launch_after_swap, launch_before_swap, sniper_lp_fee, HookCut};
use bordrless_hook::{Delta, HookReturn, PoolHookArgs};

use crate::constants::*;
use crate::error::LaunchError;
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
fn eligibility(ctx: &Context<HookCallback>, holder_fee_bps: u16) -> Result<(u64, u64)> {
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

/// `before_swap`: the LP fee (the sniper schedule; the creator's own first buy into its own
/// wallet inside the window pays the normal fee, once), then the cut from the input.
pub fn process_before_swap(ctx: Context<HookCallback>, args: PoolHookArgs) -> Result<HookReturn> {
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
    Ok(answer(launch, &cut, Some(lp_fee)))
}

/// `after_swap`: the cut from the output the DEX hands on (a buy: the curve's output; a sell: the
/// curve's output less the DEX's protocol fee, which is always in the quote).
pub fn process_after_swap(ctx: Context<HookCallback>, args: PoolHookArgs) -> Result<HookReturn> {
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
    Ok(answer(&mut ctx.accounts.launch, &cut, None))
}
