//! A launched companion's steps. `dev_buy` and `withdraw` serve the beneficiary; `claim_fees`,
//! `buyback`, `share` and `release` anyone may send, each paying its sender `bounty_bps` of what it
//! moves (as SOL). Every step is one call into a Bordrless program (plus the bridge's `unwrap_sol`
//! and a system transfer for a bounty), built here with that program's own client (`invoke.rs`).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::system_program;
use bordrless_core::swap_out;
use bordrless_kit::state::KitConfig;
use bordrless_launch::client as launch_client;
use bordrless_launch::state::Launch;
use bordrless_swap::state::Pool;
use bordrless_token::client::{self as token_client, Hook};

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::*;
use crate::instructions::CreatorSeeds;
use crate::invoke::invoke_built;
use crate::state::*;

/// The launch's mint as token instructions take it: the kit and its two extras with kit rules,
/// nothing without.
fn mint_hook(launch: &Launch) -> (Option<Hook>, Vec<AccountMeta>) {
    if launch.modules == 0 {
        return (None, vec![]);
    }
    let vault = launch.rules.rewards_on().then_some(launch.holder_vault);
    (
        Some(Hook::of(KIT_ID)),
        bordrless_launch::cpi::kit_extras(launch.kit_config, vault).to_vec(),
    )
}

/// An account of `T`'s program, deserialized (owner and discriminator checked).
fn read<T: AccountDeserialize + Owner>(info: &AccountInfo) -> Result<T> {
    require_keys_eq!(*info.owner, T::owner(), CompanionError::MissingAccount);
    let data = info.try_borrow_data()?;
    T::try_deserialize(&mut &data[..])
}

fn find<'a, 'info>(
    available: &'a [AccountInfo<'info>],
    key: &Pubkey,
) -> Result<&'a AccountInfo<'info>> {
    available
        .iter()
        .find(|a| a.key == key)
        .ok_or(error!(CompanionError::MissingAccount))
}

/// The launch's kit config, when it has kit rules.
fn kit_config(available: &[AccountInfo], launch: &Launch) -> Result<Option<KitConfig>> {
    if launch.modules == 0 {
        return Ok(None);
    }
    Ok(Some(read::<KitConfig>(find(
        available,
        &launch.kit_config,
    )?)?))
}

/// What the kit's max wallet still lets `holder` (now holding `held`) receive, while it applies.
fn max_wallet_room(kit: &Option<KitConfig>, held: u64) -> Option<u64> {
    let kit = kit.as_ref()?;
    (kit.has(bordrless_kit::constants::modules::MAX_WALLET) && !kit.graduated)
        .then(|| kit.max_wallet_amount.saturating_sub(held))
}

/// A holding's balance; 0 for one that does not exist yet.
fn balance(available: &[AccountInfo], mint: &Pubkey, owner: &Pubkey) -> Result<u64> {
    let info = find(available, &token_client::holding_address(mint, owner))?;
    if info.owner != &TOKEN_ID {
        return Ok(0);
    }
    Ok(token_client::read_holding(info)?.amount)
}

/// Creates `owner`'s holding of `mint` when it does not exist, `payer` (a signer) paying.
fn ensure_holding(
    available: &[AccountInfo],
    payer: Pubkey,
    mint: Pubkey,
    owner: Pubkey,
) -> Result<()> {
    let info = find(available, &token_client::holding_address(&mint, &owner))?;
    if info.owner == &TOKEN_ID {
        return Ok(());
    }
    invoke_built(
        &token_client::create_holding(payer, mint, owner),
        available,
        &[],
    )
}

/// Pays `to` `lamports` of the creator's bridged SOL as SOL: unwrapped to the creator address,
/// then sent on (the creator address ends with what it had).
fn pay_sol<'info>(
    available: &[AccountInfo<'info>],
    seeds: &CreatorSeeds,
    creator: Pubkey,
    to: &AccountInfo<'info>,
    lamports: u64,
) -> Result<()> {
    if lamports == 0 {
        return Ok(());
    }
    invoke_built(
        &bordrless_bridge::client::unwrap_sol(creator, lamports),
        available,
        &[&seeds.seeds()],
    )?;
    let from = find(available, &creator)?.clone();
    let system = find(available, &system_program::ID)?.clone();
    system_program::transfer(
        CpiContext::new_with_signer(
            *system.key,
            system_program::Transfer {
                from,
                to: to.clone(),
            },
            &[&seeds.seeds()],
        ),
        lamports,
    )
}

fn available<'info>(
    named: Vec<AccountInfo<'info>>,
    remaining: &[AccountInfo<'info>],
) -> Vec<AccountInfo<'info>> {
    let mut all = named;
    all.extend_from_slice(remaining);
    all
}

// ---- dev_buy -----------------------------------------------------------------------------------

/// Accounts of `dev_buy`. Remaining: the bridge's `wrap_sol` accounts and the DEX `swap`'s, the
/// creator's token holding (created here when missing), the token program's.
#[event_cpi]
#[derive(Accounts)]
pub struct DevBuy<'info> {
    #[account(mut, address = companion.beneficiary)]
    pub beneficiary: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    #[account(address = launch_client::launch_address(&companion.mint))]
    pub launch: Box<Account<'info, Launch>>,
    pub system_program: Program<'info, System>,
}

/// `dev_buy(lamports, min_out)`: the beneficiary's buy, made by the companion and held by it,
/// vesting with the dev bag; within `DEV_BUY_WINDOW` of the launch, and never above what max wallet
/// allows a wallet before graduation.
pub fn process_dev_buy<'info>(
    ctx: Context<'info, DevBuy<'info>>,
    lamports: u64,
    min_out: u64,
) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let c = &ctx.accounts.companion;
    require!(c.launched, CompanionError::NotLaunched);
    require!(lamports > 0, CompanionError::NothingToDo);
    require!(
        now <= c.launched_at.saturating_add(DEV_BUY_WINDOW),
        CompanionError::DevBuyWindowClosed
    );
    let (mint, creator) = (c.mint, ctx.accounts.creator.key());
    let seeds = CreatorSeeds::new(mint, c.creator_bump);
    system_program::transfer(
        CpiContext::new(
            ctx.accounts.system_program.key(),
            system_program::Transfer {
                from: ctx.accounts.beneficiary.to_account_info(),
                to: ctx.accounts.creator.to_account_info(),
            },
        ),
        lamports,
    )?;
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    ensure_holding(&all, ctx.accounts.beneficiary.key(), mint, creator)?;
    invoke_built(
        &bordrless_bridge::client::wrap_sol(creator, lamports),
        &all,
        &[&seeds.seeds()],
    )?;
    let before = balance(&all, &mint, &creator)?;
    let keys = launch_client::LaunchKeys::of(&ctx.accounts.launch);
    invoke_built(
        &launch_client::swap(&keys, creator, creator, 1, lamports, min_out),
        &all,
        &[&seeds.seeds()],
    )?;
    let got = balance(&all, &mint, &creator)?
        .checked_sub(before)
        .ok_or(CompanionError::MathOverflow)?;
    let dev_tokens = c
        .dev_tokens
        .checked_add(got)
        .ok_or(CompanionError::MathOverflow)?;
    // The companion is uncapped (the kit excludes it): the bag is held to what one wallet could hold.
    let kit = kit_config(&all, &ctx.accounts.launch)?;
    if let Some(room) = max_wallet_room(&kit, 0) {
        require!(dev_tokens <= room, CompanionError::DevBagOverMaxWallet);
    }
    let c = &mut ctx.accounts.companion;
    c.dev_tokens = dev_tokens;
    emit_cpi!(DevBought {
        companion: c.key(),
        lamports,
        tokens: got,
        dev_tokens
    });
    Ok(())
}

// ---- claim_fees ----------------------------------------------------------------------------------

/// Accounts of the permissionless steps.
#[event_cpi]
#[derive(Accounts)]
pub struct Step<'info> {
    /// Whoever sends it; paid the bounty.
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    #[account(address = launch_client::launch_address(&companion.mint))]
    pub launch: Box<Account<'info, Launch>>,
    pub system_program: Program<'info, System>,
}

/// `claim_fees`: the launch's creator fees claimed into the creator's holding (when it holds any),
/// the bounty paid, the rest split. What is split is everything the holding holds above what is
/// already set aside, so bridged SOL that reached it any other way is split too, never stranded.
/// Remaining: `claim_creator_fees`'s accounts and the bridge's `unwrap_sol` accounts.
pub fn process_claim_fees<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
    let c = &ctx.accounts.companion;
    require!(c.launched, CompanionError::NotLaunched);
    let (mint, creator) = (c.mint, ctx.accounts.creator.key());
    let seeds = CreatorSeeds::new(mint, c.creator_bump);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let launch_key = launch_client::launch_address(&mint);
    if balance(&all, &BRIDGED_SOL_MINT, &launch_key)? > 0 {
        invoke_built(
            &launch_client::claim_creator_fees(creator, mint, BRIDGED_SOL_MINT),
            &all,
            &[&seeds.seeds()],
        )?;
    }
    let set_aside = c
        .pending_buyback
        .checked_add(c.pending_holders)
        .and_then(|v| v.checked_add(c.pending_beneficiary))
        .ok_or(CompanionError::MathOverflow)?;
    let got = balance(&all, &BRIDGED_SOL_MINT, &creator)?.saturating_sub(set_aside);
    require!(got > 0, CompanionError::NothingToDo);
    let bounty = bps_of(got, u64::from(c.bounty_bps));
    let rest = got - bounty;
    let to_holders = bps_of(rest, u64::from(c.split.holders_bps));
    let to_beneficiary = bps_of(rest, u64::from(c.split.beneficiary_bps));
    // The rounding goes to buybacks when there are any, else to the beneficiary.
    let (to_buyback, to_beneficiary) = if c.split.buyback_bps > 0 {
        (rest - to_holders - to_beneficiary, to_beneficiary)
    } else {
        (0, rest - to_holders)
    };
    pay_sol(
        &all,
        &seeds,
        creator,
        &ctx.accounts.cranker.to_account_info(),
        bounty,
    )?;
    let c = &mut ctx.accounts.companion;
    c.pending_buyback = c
        .pending_buyback
        .checked_add(to_buyback)
        .ok_or(CompanionError::MathOverflow)?;
    c.pending_holders = c
        .pending_holders
        .checked_add(to_holders)
        .ok_or(CompanionError::MathOverflow)?;
    c.pending_beneficiary = c
        .pending_beneficiary
        .checked_add(to_beneficiary)
        .ok_or(CompanionError::MathOverflow)?;
    c.claimed_total = c.claimed_total.saturating_add(got);
    c.bounties_total = c.bounties_total.saturating_add(bounty);
    emit_cpi!(FeesClaimed {
        companion: c.key(),
        claimed: got,
        bounty,
        to_buyback,
        to_holders,
        to_beneficiary,
        cranker: ctx.accounts.cranker.key()
    });
    Ok(())
}

// ---- buyback ----------------------------------------------------------------------------------------

/// The fee bound of a buy on the launch's pool, in basis points of the input: the LP fee, the
/// creator and holder fees, Bordrless's share of them (rounded up), the burn on buys.
fn buy_fee_bound(launch: &Launch) -> u64 {
    let r = &launch.rules;
    let cuts = u64::from(launch.creator_fee_bps) + u64::from(r.holder_fee_buy_bps);
    u64::from(launch.lp_fee_bps) + cuts + cuts.div_ceil(4) + u64::from(r.burn_buy_bps)
}

/// The most a buyback may spend, in basis points of the pool's quote side: a quarter of what a
/// trader pays in fees for a buy and a sell (LP, creator, holder and burn fees; Bordrless's share not
/// counted), and never more than `BUYBACK_POOL_SHARE_BPS`. Someone who pumps the price by `p` before
/// a buyback of `v` and sells after it gains about `p * v` and pays about `p * Q * (buy + sell) / 2`
/// in fees (`Q` the quote side), so at half that bound the sandwich always costs more than it makes.
pub fn pool_share_bps(launch: &Launch) -> u64 {
    let r = &launch.rules;
    let side = u64::from(launch.lp_fee_bps) + u64::from(launch.creator_fee_bps);
    let buy = side + u64::from(r.holder_fee_buy_bps) + u64::from(r.burn_buy_bps);
    let sell = side + u64::from(r.holder_fee_sell_bps) + u64::from(r.burn_sell_bps);
    ((buy + sell) / 4).min(BUYBACK_POOL_SHARE_BPS)
}

/// `reference` moved toward `spot` once for every `interval` since `reference_at` (at least once,
/// at most `MAX_REFERENCE_STEPS`), so one crank after a long quiet catches up.
fn catch_up(reference: u128, spot: u128, reference_at: i64, interval: i64, now: i64) -> u128 {
    let steps = (now.saturating_sub(reference_at) / interval.max(1)).clamp(1, MAX_REFERENCE_STEPS);
    let mut r = reference;
    for _ in 0..steps {
        let next = step_toward(r, spot);
        if next == r {
            break;
        }
        r = next;
    }
    r
}

/// `buyback`: at most `max_buyback` of the pending buyback, and at most `pool_share_bps` of the
/// pool's quote side, at least `buyback_interval` after the last, bought on the launch's pool and
/// burned. It waits (without buying) while the pool's price is more than `MAX_PREMIUM_BPS` above the
/// reference price. The reference is set at the launch; a wait raises it toward the price by
/// `REFERENCE_STEP_BPS` for each interval since it last moved, and a buy only ever lowers it (so the
/// price a buy leaves never raises it). Remaining: the DEX `swap`'s accounts, the pool, the
/// creator's token holding (created when missing, the sender paying), the token `burn`'s and the
/// bridge's `unwrap_sol`'s.
pub fn process_buyback<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let c = &ctx.accounts.companion;
    require!(c.launched, CompanionError::NotLaunched);
    let due_at = c
        .last_buyback_at
        .checked_add(c.buyback_interval)
        .ok_or(CompanionError::MathOverflow)?;
    require!(
        now >= c.launched_at.saturating_add(BUYBACK_AFTER_LAUNCH) && now >= due_at,
        CompanionError::BuybackNotDue
    );
    require!(c.pending_buyback > 0, CompanionError::NothingToDo);
    let (mint, creator) = (c.mint, ctx.accounts.creator.key());
    let seeds = CreatorSeeds::new(mint, c.creator_bump);
    let launch = &ctx.accounts.launch;
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
    let ceiling = c
        .reference_price
        .saturating_mul(u128::from(BPS + MAX_PREMIUM_BPS))
        / u128::from(BPS);
    if spot > ceiling {
        // Above the reference: no buy. The reference catches up toward the price, a step for each
        // interval since it last moved.
        let reference = c.reference_price;
        let c = &mut ctx.accounts.companion;
        if now >= c.reference_at.saturating_add(c.buyback_interval) {
            c.reference_price = catch_up(
                c.reference_price,
                spot,
                c.reference_at,
                c.buyback_interval,
                now,
            );
            c.reference_at = now;
        }
        emit_cpi!(BuybackWaited {
            companion: c.key(),
            price: spot,
            reference
        });
        return Ok(());
    }
    let pool_side = u128::from(pool.quote_reserve) + u128::from(pool.virtual_quote);
    let pool_cap = (pool_side * u128::from(pool_share_bps(launch)) / u128::from(BPS))
        .min(u128::from(u64::MAX)) as u64;
    let total = c.pending_buyback.min(c.max_buyback).min(pool_cap);
    require!(total > 0, CompanionError::NothingToDo);
    let bounty = bps_of(total, u64::from(c.bounty_bps));
    let spend = total - bounty;
    // No less than the pool's own quote now, less the fees and the slippage: never at any price.
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
    let keys = launch_client::LaunchKeys::of(launch);
    invoke_built(
        &launch_client::swap(&keys, creator, creator, 1, spend, min_out),
        &all,
        &[&seeds.seeds()],
    )?;
    let bought = balance(&all, &mint, &creator)?
        .checked_sub(before)
        .ok_or(CompanionError::MathOverflow)?;
    let (hook, extras) = mint_hook(launch);
    let holding = token_client::holding_address(&mint, &creator);
    invoke_built(
        &token_client::burn_with(creator, holding, mint, hook, extras, bought),
        &all,
        &[&seeds.seeds()],
    )?;
    pay_sol(
        &all,
        &seeds,
        creator,
        &ctx.accounts.cranker.to_account_info(),
        bounty,
    )?;
    let after = read::<Pool>(find(&all, &launch.pool)?)?;
    let c = &mut ctx.accounts.companion;
    c.pending_buyback -= total;
    c.last_buyback_at = now;
    if let Some(price) = spot_price(
        after.quote_reserve,
        after.virtual_quote,
        after.base_reserve,
        after.virtual_base,
    ) {
        // Down only: the price this buy leaves is partly the buy itself (or a front-runner's).
        c.reference_price = c.reference_price.min(step_toward(c.reference_price, price));
    }
    c.spent_total = c.spent_total.saturating_add(spend);
    c.burned_total = c.burned_total.saturating_add(bought);
    c.bounties_total = c.bounties_total.saturating_add(bounty);
    emit_cpi!(BoughtBack {
        companion: c.key(),
        spent: spend,
        burned: bought,
        bounty,
        cranker: ctx.accounts.cranker.key()
    });
    Ok(())
}

// ---- share ------------------------------------------------------------------------------------------

/// `share`: the holders' part, streamed to holders through the kit's reward pool (the kit takes a
/// share only while holders hold enough, so this waits until then). Remaining: the kit `share`'s
/// accounts and the bridge's `unwrap_sol`'s.
pub fn process_share<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
    let c = &ctx.accounts.companion;
    require!(c.launched, CompanionError::NotLaunched);
    let total = c.pending_holders;
    let bounty = bps_of(total, u64::from(c.bounty_bps));
    let amount = total - bounty;
    require!(
        amount >= bordrless_kit::constants::MIN_SHARE_LAMPORTS,
        CompanionError::NothingToDo
    );
    let (mint, creator) = (c.mint, ctx.accounts.creator.key());
    let seeds = CreatorSeeds::new(mint, c.creator_bump);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let source = token_client::holding_address(&BRIDGED_SOL_MINT, &creator);
    invoke_built(
        &bordrless_kit::client::share(creator, mint, source, BRIDGED_SOL_MINT, amount),
        &all,
        &[&seeds.seeds()],
    )?;
    pay_sol(
        &all,
        &seeds,
        creator,
        &ctx.accounts.cranker.to_account_info(),
        bounty,
    )?;
    let c = &mut ctx.accounts.companion;
    c.pending_holders = 0;
    c.shared_total = c.shared_total.saturating_add(amount);
    c.bounties_total = c.bounties_total.saturating_add(bounty);
    emit_cpi!(SharedWithHolders {
        companion: c.key(),
        amount,
        bounty,
        cranker: ctx.accounts.cranker.key()
    });
    Ok(())
}

// ---- withdraw -----------------------------------------------------------------------------------------

/// Accounts of `withdraw`.
#[event_cpi]
#[derive(Accounts)]
pub struct Withdraw<'info> {
    pub sender: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: the beneficiary (address-checked); paid here.
    #[account(mut, address = companion.beneficiary)]
    pub beneficiary: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `withdraw`: the beneficiary's part paid to them as SOL, with whatever SOL the creator address
/// holds above its rent-exempt minimum (what the launch did not spend of its funding). Anyone may
/// send it: it pays only the beneficiary. Before the launch, only with the mint's signature (as a
/// remaining account): the funding returned to a launch that never happened. Remaining: the
/// bridge's `unwrap_sol` accounts.
pub fn process_withdraw<'info>(ctx: Context<'info, Withdraw<'info>>) -> Result<()> {
    let c = &ctx.accounts.companion;
    // Before the launch the creator address's funding pays for it: only the mint's holder refunds it.
    let mint_signed = ctx
        .remaining_accounts
        .iter()
        .any(|a| a.key() == c.mint && a.is_signer);
    require!(c.launched || mint_signed, CompanionError::NotLaunched);
    let (mint, creator) = (c.mint, ctx.accounts.creator.key());
    let seeds = CreatorSeeds::new(mint, c.creator_bump);
    let pending = c.pending_beneficiary;
    let spare = ctx
        .accounts
        .creator
        .lamports()
        .saturating_sub(Rent::get()?.minimum_balance(0));
    require!(pending > 0 || spare > 0, CompanionError::NothingToDo);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let beneficiary = ctx.accounts.beneficiary.to_account_info();
    if spare > 0 {
        let from = ctx.accounts.creator.to_account_info();
        system_program::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.key(),
                system_program::Transfer {
                    from,
                    to: beneficiary.clone(),
                },
                &[&seeds.seeds()],
            ),
            spare,
        )?;
    }
    pay_sol(&all, &seeds, creator, &beneficiary, pending)?;
    let c = &mut ctx.accounts.companion;
    c.pending_beneficiary = 0;
    c.paid_beneficiary_total = c.paid_beneficiary_total.saturating_add(pending + spare);
    emit_cpi!(BeneficiaryPaid {
        companion: c.key(),
        lamports: pending + spare
    });
    Ok(())
}

// ---- release ------------------------------------------------------------------------------------------

/// `release`: the dev bag's vested tokens sent to the beneficiary (their holding created when
/// missing, the sender paying): not before the early-buyer lock ends (the dev's buy would have
/// been locked like anyone's), and no more than max wallet still lets them hold before graduation
/// (the rest waits). Remaining: the token `transfer`'s accounts, the beneficiary's holding.
pub fn process_release<'info>(ctx: Context<'info, Step<'info>>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let c = &ctx.accounts.companion;
    let vested = c.vested(now).saturating_sub(c.dev_released);
    require!(vested > 0, CompanionError::NothingToDo);
    let (mint, creator, beneficiary) = (c.mint, ctx.accounts.creator.key(), c.beneficiary);
    let seeds = CreatorSeeds::new(mint, c.creator_bump);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let kit = kit_config(&all, &ctx.accounts.launch)?;
    if let Some(k) = &kit {
        if k.has(bordrless_kit::constants::modules::EARLY_BUYER_LOCK) {
            require!(now >= k.early_unlock_at, CompanionError::EarlyLocked);
        }
    }
    ensure_holding(&all, ctx.accounts.cranker.key(), mint, beneficiary)?;
    let held = balance(&all, &mint, &beneficiary)?;
    let amount = match max_wallet_room(&kit, held) {
        Some(room) => vested.min(room),
        None => vested,
    };
    require!(amount > 0, CompanionError::NothingToDo);
    let (hook, extras) = mint_hook(&ctx.accounts.launch);
    let ix: Instruction = token_client::transfer_with(
        creator,
        token_client::holding_address(&mint, &creator),
        token_client::holding_address(&mint, &beneficiary),
        mint,
        hook,
        extras,
        amount,
    );
    invoke_built(&ix, &all, &[&seeds.seeds()])?;
    let c = &mut ctx.accounts.companion;
    c.dev_released = c
        .dev_released
        .checked_add(amount)
        .ok_or(CompanionError::MathOverflow)?;
    emit_cpi!(DevReleased {
        companion: c.key(),
        tokens: amount,
        released: c.dev_released
    });
    Ok(())
}
