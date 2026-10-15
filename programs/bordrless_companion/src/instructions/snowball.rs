//! Snowball (2026-10-12): a companion whose pot makes volume in its own token. Its goal is volume,
//! and it spends the pot to make it (the owner: "create volume and use all the funds"). `create_snowball`
//! (before the launch, the mint signing) sets the pot's part of every fee claim and the market
//! maker's terms; from then on every fee claim adds that part to the pot (`Companion.pending_pot`),
//! and `snowball_step` (anyone, paid a bounty) trades on the token's own pool, once an `interval`:
//!
//! - **A buy on a drop.** When the pool's price is `buy_step_bps` or more below the reference, it
//!   spends `buy_bps` of the pot.
//! - **A sell on a rise.** When the price is `sell_step_bps` or more above the reference, it sells
//!   `sell_bps` of the tokens it holds, and the SOL goes back to the pot.
//! - **A trade when the price is flat** (`flat_trades`): between the two, it trades anyway, on the
//!   side that rebalances what it holds (a sell while its tokens are worth more than its SOL, else a
//!   buy), so the pot keeps cycling through the pool every interval instead of waiting for a move.
//!   The same when the side the price calls for has nothing to trade: it trades the other side.
//! - **The reference** is the opening price until the first step, then the price each trade leaves.
//! - **A follow** (only without `flat_trades`): when the price calls for a side the pot can't trade
//!   (below `MIN_SNOWBALL_TRADE`), the reference moves to the price instead, so it stays centred on
//!   the market. A follow pays no bounty.
//!
//! One trade moves at most `max_trade` and `pool_bps` of the pool's SOL side (real and virtual, the
//! launcher's choice of 0.25% to 10%); it takes no less than the pool's own quote less the fees and
//! 2%; trades start a minute after the launch (and after any early-buyer window). Large or frequent
//! trades can be sandwiched by bots, which takes part of the pot: the launcher's choice. The pot has
//! no cap: it is the companion's own SOL and only these trades spend it.
//!
//! The market maker's tokens sit in the creator's token holding beside any dev bag;
//! `Snowball.tokens_held` counts them, so a sell never touches the dev bag. A Snowball token may
//! have a custom hook (2026-10-12): its buys and its sells carry it, with the hook's extra accounts
//! resolved from its registry for each direction, and what a trade moved is measured (a hook's cut
//! on a transfer included).

use anchor_lang::prelude::*;
use bordrless_core::swap_out;
use bordrless_launch::client as launch_client;
use bordrless_launch::state::Launch;
use bordrless_swap::state::Pool;

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::*;
use crate::instructions::steps::{
    available, balance, buy_fee_bound, creator_buy, creator_sell, ensure_holding, find, pay_sol,
    read, sol_held,
};
use crate::instructions::CreatorSeeds;
use crate::invoke::invoke_built;
use crate::state::*;

/// Arguments of `create_snowball`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct CreateSnowballArgs {
    /// Every fee claim's split besides the pot: the four parts (with `pot_bps`) sum to 10,000.
    pub split: Split,
    /// The pot's part of every fee claim: what the market maker trades with.
    pub pot_bps: u16,
    /// The drop from the reference that triggers a buy, and the rise that triggers a sell (0.5% to
    /// 20% each).
    pub buy_step_bps: u16,
    pub sell_step_bps: u16,
    /// The share of the pot a buy spends, and of its tokens a sell sells (1% to 100% each).
    pub buy_bps: u16,
    pub sell_bps: u16,
    /// The least time between trades (10 seconds to a day).
    pub interval: i64,
    /// The most one trade moves, lamports (at least 0.01 SOL).
    pub max_trade: u64,
    /// The most one trade moves as a share of the pool's SOL side (0.25% to 10%).
    pub pool_bps: u16,
    /// Trade every interval even when the price is within its steps (on the side that rebalances).
    pub flat_trades: bool,
}

/// Accounts of `create_snowball`.
#[event_cpi]
#[derive(Accounts)]
pub struct CreateSnowball<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The launch's mint, not created yet: it signs, so only whoever made the companion makes its
    /// Snowball.
    pub mint: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, mint.key().as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    #[account(init, payer = payer, space = Snowball::LEN, seeds = [SNOWBALL_SEED, mint.key().as_ref()], bump)]
    pub snowball: Box<Account<'info, Snowball>>,
    pub system_program: Program<'info, System>,
}

/// The terms within their bounds.
pub fn check_terms(args: &CreateSnowballArgs) -> Result<()> {
    require!(
        args.pot_bps > 0 && args.split.valid_with_pot(args.pot_bps),
        CompanionError::BadSplit
    );
    let step = MIN_SNOWBALL_STEP_BPS..=MAX_SNOWBALL_STEP_BPS;
    let size = MIN_SNOWBALL_SIZE_BPS..=MAX_SNOWBALL_SIZE_BPS;
    require!(
        step.contains(&args.buy_step_bps)
            && step.contains(&args.sell_step_bps)
            && size.contains(&args.buy_bps)
            && size.contains(&args.sell_bps)
            && (MIN_SNOWBALL_POOL_BPS..=MAX_SNOWBALL_POOL_BPS).contains(&args.pool_bps)
            && (MIN_SNOWBALL_INTERVAL..=MAX_SNOWBALL_INTERVAL).contains(&args.interval)
            && args.max_trade >= MIN_SNOWBALL_MAX_TRADE,
        CompanionError::BadSnowball
    );
    Ok(())
}

/// `create_snowball`: before the launch, the companion becomes a Snowball (`mode`), its split now
/// with the pot's part, and its market maker's terms are fixed.
pub fn process_create_snowball(
    ctx: Context<CreateSnowball>,
    args: CreateSnowballArgs,
) -> Result<()> {
    let c = &ctx.accounts.companion;
    require!(
        !c.launched && !c.is_game() && c.is_classic(),
        CompanionError::BadSnowball
    );
    check_terms(&args)?;
    // A buyback beside the pot needs the limits `create` checks only for its own split.
    if args.split.buyback_bps > 0 {
        require!(
            c.max_buyback >= MIN_MAX_BUYBACK
                && (MIN_BUYBACK_INTERVAL..=MAX_BUYBACK_INTERVAL).contains(&c.buyback_interval),
            CompanionError::BadBuybackLimits
        );
    }
    let now = Clock::get()?.unix_timestamp;
    let mint = ctx.accounts.mint.key();
    let c = &mut ctx.accounts.companion;
    c.split = args.split;
    c.pot_bps = args.pot_bps;
    c.mode = MODE_SNOWBALL;
    let s = &mut ctx.accounts.snowball;
    s.version = SNOWBALL_VERSION;
    s.bump = ctx.bumps.snowball;
    s.mint = mint;
    s.buy_step_bps = args.buy_step_bps;
    s.sell_step_bps = args.sell_step_bps;
    s.buy_bps = args.buy_bps;
    s.sell_bps = args.sell_bps;
    s.interval = args.interval;
    s.max_trade = args.max_trade;
    s.pool_bps = args.pool_bps;
    s.flat_trades = args.flat_trades;
    s.reference_price = 0;
    s.last_at = 0;
    s.tokens_held = 0;
    s.buys = 0;
    s.sells = 0;
    s.sol_spent = 0;
    s.sol_received = 0;
    s.tokens_bought = 0;
    s.tokens_sold = 0;
    s.bounties_total = 0;
    s.created_at = now;
    s.reserved = [0; 64];
    emit_cpi!(SnowballCreated {
        companion: c.key(),
        mint,
        pot_bps: args.pot_bps,
        buy_step_bps: args.buy_step_bps,
        sell_step_bps: args.sell_step_bps,
        buy_bps: args.buy_bps,
        sell_bps: args.sell_bps,
        interval: args.interval,
        max_trade: args.max_trade,
        pool_bps: args.pool_bps,
        flat_trades: args.flat_trades,
    });
    Ok(())
}

/// Accounts of `snowball_step`. Remaining: the DEX `swap`'s accounts (as a buy's), the pool, the
/// creator's token holding (created when missing, the sender paying), the bridge's `unwrap_sol`'s;
/// for a custom-hook token, also the hook's extra accounts for a sell and its registry.
#[event_cpi]
#[derive(Accounts)]
pub struct SnowballStep<'info> {
    /// Whoever sends it; paid the bounty on a trade.
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    #[account(address = launch_client::launch_address(&companion.mint))]
    pub launch: Box<Account<'info, Launch>>,
    #[account(mut, seeds = [SNOWBALL_SEED, companion.mint.as_ref()], bump = snowball.bump)]
    pub snowball: Box<Account<'info, Snowball>>,
    pub system_program: Program<'info, System>,
}

/// The fee bound of a sell on the launch's pool, in basis points of the quote it pays out: the LP
/// fee, the creator and holder fees, Bordrless's share of them (rounded up). The burn on sells comes
/// off the input instead.
fn sell_fee_bound(launch: &Launch) -> u64 {
    let r = &launch.rules;
    let cuts = u64::from(launch.creator_fee_bps) + u64::from(r.holder_fee_sell_bps);
    u64::from(launch.lp_fee_bps) + cuts + cuts.div_ceil(4)
}

/// Base units that sell for `quote_out` on the curve `x * y` (rounded down): what keeps a sell
/// within the trade cap.
fn base_for_quote(quote_out: u64, base_side: u128, quote_side: u128) -> u64 {
    let out = u128::from(quote_out);
    if out >= quote_side {
        return u64::MAX;
    }
    (out * base_side / (quote_side - out)).min(u128::from(u64::MAX)) as u64
}

/// The side a step trades.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Buy,
    Sell,
}

/// What each side could trade now, within the shares and the cap.
struct Plan {
    /// Lamports a buy would spend, bounty included.
    buy_total: u64,
    /// Tokens a sell would sell, and the SOL they fetch before the fees (after a burn on sells).
    sell_tokens: u64,
    sell_gross: u64,
}

impl Plan {
    fn can(&self, side: Side) -> bool {
        match side {
            Side::Buy => self.buy_total >= MIN_SNOWBALL_TRADE,
            Side::Sell => self.sell_gross >= MIN_SNOWBALL_TRADE,
        }
    }
}

fn plan(c: &Companion, s: &Snowball, launch: &Launch, pool: &Pool) -> Plan {
    let base_side = u128::from(pool.base_reserve) + u128::from(pool.virtual_base);
    let quote_side = u128::from(pool.quote_reserve) + u128::from(pool.virtual_quote);
    let pool_cap =
        (quote_side * u128::from(s.pool_bps) / u128::from(BPS)).min(u128::from(u64::MAX)) as u64;
    let cap = s.max_trade.min(pool_cap);
    let buy_total = bps_of(c.pending_pot, u64::from(s.buy_bps)).min(cap);
    let sell_tokens = bps_of(s.tokens_held, u64::from(s.sell_bps))
        .min(base_for_quote(cap, base_side, quote_side));
    let net_in = bps_of(
        sell_tokens,
        BPS.saturating_sub(u64::from(launch.rules.burn_sell_bps)),
    );
    let sell_gross = if net_in == 0 {
        0
    } else {
        swap_out(
            net_in,
            pool.base_reserve,
            pool.virtual_base,
            pool.quote_reserve,
            pool.virtual_quote,
        )
        .unwrap_or(0)
    };
    Plan {
        buy_total,
        sell_tokens,
        sell_gross,
    }
}

/// What the step does: a side to trade (and whether a step triggered it), a follow, or nothing.
enum Decision {
    Trade(Side, bool),
    Follow,
}

/// The decision at `spot` from `reference`: the side the price calls for when it can trade; else,
/// with `flat_trades`, the side that rebalances what it holds (or the other, if that one can't);
/// else a follow when a step triggered, or nothing due.
fn decide(c: &Companion, s: &Snowball, p: &Plan, spot: u128, reference: u128) -> Result<Decision> {
    let called = if spot <= Snowball::buy_below(reference, s.buy_step_bps) {
        Some(Side::Buy)
    } else if spot >= Snowball::sell_above(reference, s.sell_step_bps) {
        Some(Side::Sell)
    } else {
        None
    };
    if let Some(side) = called {
        if p.can(side) {
            return Ok(Decision::Trade(side, true));
        }
    }
    if !s.flat_trades {
        return match called {
            Some(_) => Ok(Decision::Follow),
            None => err!(CompanionError::SnowballNotDue),
        };
    }
    // Rebalance: sell while its tokens are worth more than its SOL, else buy.
    let tokens_value = u128::from(s.tokens_held) * spot / PRICE_SCALE;
    let preferred = if tokens_value > u128::from(c.pending_pot) {
        Side::Sell
    } else {
        Side::Buy
    };
    let other = if preferred == Side::Buy {
        Side::Sell
    } else {
        Side::Buy
    };
    if p.can(preferred) {
        return Ok(Decision::Trade(preferred, false));
    }
    if p.can(other) {
        return Ok(Decision::Trade(other, false));
    }
    err!(CompanionError::NothingToDo)
}

/// `snowball_step`: at most once an `interval`, from a minute after the launch and the end of any
/// early-buyer window: a buy or a sell as `decide` says, a follow, or `SnowballNotDue` /
/// `NothingToDo`.
pub fn process_snowball_step<'info>(ctx: Context<'info, SnowballStep<'info>>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let c = &ctx.accounts.companion;
    require!(c.is_snowball(), CompanionError::NotASnowball);
    require!(c.launched, CompanionError::NotLaunched);
    let s = &ctx.accounts.snowball;
    let launch = &ctx.accounts.launch;
    let opens = c
        .launched_at
        .saturating_add(BUYBACK_AFTER_LAUNCH)
        .max(launch.early_window_end);
    require!(
        now >= opens && now >= s.last_at.saturating_add(s.interval),
        CompanionError::SnowballNotDue
    );
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let pool_info = find(&all, &launch.pool)?;
    require_keys_eq!(*pool_info.owner, SWAP_ID, CompanionError::MissingAccount);
    let pool = read::<Pool>(pool_info)?;
    let spot = spot_price(
        pool.quote_reserve,
        pool.virtual_quote,
        pool.base_reserve,
        pool.virtual_base,
    )
    .ok_or(CompanionError::NoQuote)?;
    // The reference: the opening price until the first step.
    let reference = if s.reference_price == 0 {
        c.reference_price
    } else {
        s.reference_price
    };
    let p = plan(c, s, launch, &pool);
    let (side, on_step) = match decide(c, s, &p, spot, reference)? {
        Decision::Trade(side, on_step) => (side, on_step),
        Decision::Follow => return follow(ctx, reference, spot, now),
    };
    let (mint, creator) = (c.mint, ctx.accounts.creator.key());
    let seeds = CreatorSeeds::of(c);

    match side {
        Side::Buy => {
            let total = p.buy_total;
            let bounty = bps_of(total, u64::from(c.bounty_bps));
            let spend = total - bounty;
            // No less than the pool's own quote now, less the fees and the slippage.
            let net = bps_of(spend, BPS.saturating_sub(buy_fee_bound(launch)));
            let quote = swap_out(
                net,
                pool.quote_reserve,
                pool.virtual_quote,
                pool.base_reserve,
                pool.virtual_base,
            )
            .ok_or(CompanionError::NoQuote)?;
            let min_out = bps_of(quote, BPS - BUYBACK_SLIPPAGE_BPS);
            require!(min_out > 0, CompanionError::NoQuote);
            ensure_holding(&all, ctx.accounts.cranker.key(), mint, creator)?;
            let before = balance(&all, &mint, &creator)?;
            let buy = creator_buy(&all, launch, creator, spend, min_out, seeds.native)?;
            invoke_built(&buy, &all, &[&seeds.seeds()])?;
            let bought = balance(&all, &mint, &creator)?
                .checked_sub(before)
                .ok_or(CompanionError::MathOverflow)?;
            pay_sol(
                &all,
                &seeds,
                creator,
                &ctx.accounts.cranker.to_account_info(),
                bounty,
            )?;
            let price_after = price_of(&all, launch)?;
            let c = &mut ctx.accounts.companion;
            c.pending_pot = c
                .pending_pot
                .checked_sub(total)
                .ok_or(CompanionError::MathOverflow)?;
            c.bounties_total = c.bounties_total.saturating_add(bounty);
            let pot_after = c.pending_pot;
            let s = &mut ctx.accounts.snowball;
            s.tokens_held = s
                .tokens_held
                .checked_add(bought)
                .ok_or(CompanionError::MathOverflow)?;
            s.reference_price = price_after;
            s.last_at = now;
            s.buys = s.buys.saturating_add(1);
            s.sol_spent = s.sol_spent.saturating_add(spend);
            s.tokens_bought = s.tokens_bought.saturating_add(bought);
            s.bounties_total = s.bounties_total.saturating_add(bounty);
            emit_cpi!(SnowballTraded {
                companion: c.key(),
                side: 1,
                on_step,
                sol: spend,
                tokens: bought,
                bounty,
                price_before: spot,
                price_after,
                pot_after,
                tokens_after: s.tokens_held,
                cranker: ctx.accounts.cranker.key(),
            });
        }
        Side::Sell => {
            let tokens = p.sell_tokens;
            let net = bps_of(p.sell_gross, BPS.saturating_sub(sell_fee_bound(launch)));
            let min_out = bps_of(net, BPS - BUYBACK_SLIPPAGE_BPS);
            require!(min_out > 0, CompanionError::NoQuote);
            let before = sol_held(&all, &seeds, &creator)?;
            let sell = creator_sell(&all, launch, creator, tokens, min_out, seeds.native)?;
            invoke_built(&sell, &all, &[&seeds.seeds()])?;
            let got = sol_held(&all, &seeds, &creator)?
                .checked_sub(before)
                .ok_or(CompanionError::MathOverflow)?;
            let bounty = bps_of(got, u64::from(c.bounty_bps));
            pay_sol(
                &all,
                &seeds,
                creator,
                &ctx.accounts.cranker.to_account_info(),
                bounty,
            )?;
            let price_after = price_of(&all, launch)?;
            let c = &mut ctx.accounts.companion;
            c.pending_pot = c
                .pending_pot
                .checked_add(got - bounty)
                .ok_or(CompanionError::MathOverflow)?;
            c.bounties_total = c.bounties_total.saturating_add(bounty);
            let pot_after = c.pending_pot;
            let s = &mut ctx.accounts.snowball;
            s.tokens_held = s
                .tokens_held
                .checked_sub(tokens)
                .ok_or(CompanionError::MathOverflow)?;
            s.reference_price = price_after;
            s.last_at = now;
            s.sells = s.sells.saturating_add(1);
            s.sol_received = s.sol_received.saturating_add(got - bounty);
            s.tokens_sold = s.tokens_sold.saturating_add(tokens);
            s.bounties_total = s.bounties_total.saturating_add(bounty);
            emit_cpi!(SnowballTraded {
                companion: c.key(),
                side: 0,
                on_step,
                sol: got - bounty,
                tokens,
                bounty,
                price_before: spot,
                price_after,
                pot_after,
                tokens_after: s.tokens_held,
                cranker: ctx.accounts.cranker.key(),
            });
        }
    }
    Ok(())
}

/// The pool's price now (after a trade): the new reference.
fn price_of(all: &[AccountInfo], launch: &Launch) -> Result<u128> {
    let after = read::<Pool>(find(all, &launch.pool)?)?;
    spot_price(
        after.quote_reserve,
        after.virtual_quote,
        after.base_reserve,
        after.virtual_base,
    )
    .ok_or_else(|| error!(CompanionError::NoQuote))
}

/// The side the price calls for has nothing to trade (without flat trades): the reference moves to
/// the price.
fn follow<'info>(
    ctx: Context<'info, SnowballStep<'info>>,
    reference: u128,
    spot: u128,
    now: i64,
) -> Result<()> {
    let companion = ctx.accounts.companion.key();
    let s = &mut ctx.accounts.snowball;
    s.reference_price = spot;
    s.last_at = now;
    emit_cpi!(SnowballFollowed {
        companion,
        reference_before: reference,
        price: spot,
    });
    Ok(())
}
