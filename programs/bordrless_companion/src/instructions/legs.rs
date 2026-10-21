//! Preset companions on a coin quoted in tokens (`docs/token-quotes.md` §7.4, 21 Oct 2026, the
//! owner: "DIY configs should be available for all quotes: if the quote token is SPY, it should pay
//! in SPY"). A buyback, the launcher's part and a Snowball run on such a coin as on a SOL coin, each
//! leg in its own token, nothing converted:
//!
//! - **The parts are per leg, in the leg's token** (`CompanionLegs`, made by `launch_token`): every
//!   leg's creator fee is claimed into the creator address's associated token account of the leg's
//!   token, where the leg's parts are set aside (`LegPot`), never as lamports.
//! - `claim_fees_leg(i)` (anyone): the launchpad's `claim_creator_fees_token(i)` into that account,
//!   then what the account holds above the leg's set-aside is split as `claim_fees` splits SOL: the
//!   bounty first (to the sender's token account of the leg's token when it passes one: none, no
//!   bounty), then the holders', the launcher's, the buyback's and the pot's parts by the
//!   companion's split, the rounding to the pot, else the buyback, else the launcher.
//! - `withdraw_leg(i)` (anyone): the leg's launcher's part to the launcher's associated token
//!   account of the leg's token (made when missing, the sender paying).
//! - `buyback_leg(i)` (anyone): `buyback` on leg `i`: at least `buyback_interval` after the leg's
//!   last, a minute after the launch; it waits while the leg's price is more than
//!   `MAX_PREMIUM_BPS` above the leg's reference (the leg's opening price at the launch; a wait
//!   raises it toward the price by `REFERENCE_STEP_BPS` an interval, a buy only lowers it); it spends
//!   at most the pool share `pool_share_bps` of the leg's quote side (real and virtual), with a
//!   minimum out of the leg's own curve less the fees and `BUYBACK_SLIPPAGE_BPS`, by `swap_spl`
//!   with the creator address as trader, then burns what it bought. `max_buyback` (lamports) is a SOL
//!   coin's bound and is not read here.
//! - `snowball_step_leg(i)` (anyone): `snowball_step` on leg `i` with the leg's pot and the coins
//!   bought with it (`LegPot.tokens_held`); `max_trade` (lamports) is not read, the pool share
//!   (`pool_bps` of the leg's quote side) bounds a trade.
//! - `share_leg(i)` (anyone): the leg's holders' part shared with the coin's holders through the
//!   kit's `share_token` (`docs/token-quotes.md` §7.5.1: streamed over the hour on a one-leg coin,
//!   counted at the next round on a coin of several legs), the bounty in the leg's token.
//! - **The currency** of a leg's paid parts (the launcher's and the holders') is decided in one
//!   place, `CompanionLegs::pays_in_token`, set at the launch from its SOL switch (§7.5.2). With the
//!   switch, `swap_bucket_to_sol(i, part)` (anyone) sells the leg's launcher's or holders' bucket
//!   through the launchpad's `swap_to_sol` (the creator address signing as the tokens' owner) into
//!   the companion's SOL `pending_beneficiary` or `pending_holders`, which `withdraw` and `share`
//!   pay as on a SOL coin; `withdraw_leg` and `share_leg` refuse those buckets (`SolSwitchOn`). The
//!   buyback's and the Snowball's pots stay in the leg's token either way (they buy the coin, which
//!   trades only against the legs' tokens).
//! - **Payees** (`set_payees`) share the launcher's part in the leg's token: `withdraw_leg` pays each
//!   its share into its associated token account of the leg's token (made when missing and the
//!   payee's wallet is passed, the sender paying), the launcher the rest; a party whose account is
//!   missing, frozen or asks for a memo is owed its due on that leg (`PayeesLegs`), paid at a later
//!   `withdraw_leg`, so no one blocks the others.
//! - **The buyback and the Snowball keep apart** (review3 F1): `buyback_leg` moves only the leg's
//!   `reference_price`, `reference_at` and `last_at`; `snowball_step_leg` only its
//!   `snowball_reference` and `snowball_last_at`, as on a SOL coin the two have their own.
//!
//! A step's token accounts are the leg's: its quote mint and program from `CompanionLegs`, the
//! creator address's associated token account of it, the leg's pool and fee account derived from
//! them. Every coin transfer carries the coin's token hook (the kit with the `Legs`, a custom hook
//! with its extras resolved from its registry, or none).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use bordrless_core::swap_out;
use bordrless_launch::client::{self as launch_client, LegKeys};
use bordrless_launch::state::{Launch, Legs};
use bordrless_swap::spl;
use bordrless_swap::state::Pool;
use bordrless_token::client::{self as token_client, Hook};

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::*;
use crate::instructions::snowball::{base_for_quote, sell_fee_bound};
use crate::instructions::steps::{
    available, balance, buy_fee_bound, catch_up, custom_extras, ensure_holding, find,
    pool_share_bps, read, OpKeys,
};
use crate::instructions::CreatorSeeds;
use crate::invoke::invoke_built;
use crate::state::*;
use anchor_lang::InstructionData;

/// Accounts of `claim_fees_leg` and `buyback_leg`. Remaining: the launchpad's
/// `claim_creator_fees_token` accounts (a claim), or the leg's `swap_spl` accounts, the creator's
/// coin holding's `create_holding` accounts and the token `burn`'s (a buyback: the mint writable);
/// the sender's token account of the leg's token for the bounty, when it wants one.
#[event_cpi]
#[derive(Accounts)]
pub struct LegStep<'info> {
    /// Whoever sends it; paid the bounty in the leg's token.
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    #[account(address = launch_client::launch_address(&companion.mint))]
    pub launch: Box<Account<'info, Launch>>,
    #[account(mut, seeds = [COMPANION_LEGS_SEED, companion.mint.as_ref()], bump = companion_legs.bump)]
    pub companion_legs: Box<Account<'info, CompanionLegs>>,
    pub system_program: Program<'info, System>,
}

/// Accounts of `withdraw_leg`. Remaining: the leg's quote mint, the creator address's token account
/// of it (w), the launcher's associated token account of it (w), its token program, the associated
/// token account program.
#[event_cpi]
#[derive(Accounts)]
pub struct LegWithdraw<'info> {
    /// Whoever sends it; pays the launcher's token account's rent when it is made.
    #[account(mut)]
    pub sender: Signer<'info>,
    #[account(seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: the launcher (address-checked).
    #[account(address = companion.beneficiary)]
    pub beneficiary: UncheckedAccount<'info>,
    #[account(address = launch_client::launch_address(&companion.mint))]
    pub launch: Box<Account<'info, Launch>>,
    #[account(mut, seeds = [COMPANION_LEGS_SEED, companion.mint.as_ref()], bump = companion_legs.bump)]
    pub companion_legs: Box<Account<'info, CompanionLegs>>,
    pub system_program: Program<'info, System>,
}

/// Accounts of `snowball_step_leg`: `LegStep`'s and the Snowball's terms. Remaining: the leg's
/// `swap_spl` accounts, the creator's coin holding's `create_holding` accounts, the sender's token
/// account of the leg's token for the bounty when it wants one, and the coin's `Legs` while its
/// early-buyer window may run.
#[event_cpi]
#[derive(Accounts)]
pub struct LegSnowballStep<'info> {
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked).
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    #[account(address = launch_client::launch_address(&companion.mint))]
    pub launch: Box<Account<'info, Launch>>,
    #[account(mut, seeds = [COMPANION_LEGS_SEED, companion.mint.as_ref()], bump = companion_legs.bump)]
    pub companion_legs: Box<Account<'info, CompanionLegs>>,
    #[account(seeds = [SNOWBALL_SEED, companion.mint.as_ref()], bump = snowball.bump)]
    pub snowball: Box<Account<'info, Snowball>>,
    pub system_program: Program<'info, System>,
}

/// Leg `index` of a step: its pot as it was, its keys, and its token's accounts among the step's.
struct Leg<'a, 'info> {
    i: usize,
    pot: LegPot,
    keys: LegKeys,
    program: &'a AccountInfo<'info>,
    quote_mint: &'a AccountInfo<'info>,
    /// The creator address's associated token account of the leg's token.
    creator_quote: &'a AccountInfo<'info>,
}

/// Leg `index` of `launch` (`NotALegStep` for a launch not quoted in tokens, `NoSuchLeg` past its
/// legs), its token's accounts found among `all` (`MissingAccount`).
fn leg_of<'a, 'info>(
    all: &'a [AccountInfo<'info>],
    launch: &Launch,
    legs: &CompanionLegs,
    creator: &Pubkey,
    index: u8,
) -> Result<Leg<'a, 'info>> {
    require!(launch.is_token_quoted(), CompanionError::NotALegStep);
    let i = usize::from(index);
    require!(i < usize::from(legs.count), CompanionError::NoSuchLeg);
    let pot = legs.legs[i];
    let keys = LegKeys {
        mint: launch.mint,
        quote_mint: pot.quote_mint,
        quote_program: spl::program_id(pot.quote_program),
        lp_fee_bps: launch.lp_fee_bps,
    };
    Ok(Leg {
        i,
        pot,
        program: find(all, &keys.quote_program)?,
        quote_mint: find(all, &pot.quote_mint)?,
        creator_quote: find(all, &keys.quote_account(creator))?,
        keys,
    })
}

/// A token account's balance under `program` (0 for one that does not exist).
fn token_amount(info: &AccountInfo, program: &Pubkey) -> u64 {
    spl::read_token_account(info, program).map_or(0, |t| t.amount)
}

/// The sender's token account of the leg's token, writable, when it passed one: where its bounty
/// goes. None: no bounty (the part keeps it).
fn bounty_account<'a, 'info>(
    all: &'a [AccountInfo<'info>],
    leg: &Leg,
    cranker: &Pubkey,
) -> Option<&'a AccountInfo<'info>> {
    let key = leg.keys.quote_account(cranker);
    all.iter().find(|a| *a.key == key).filter(|a| {
        a.is_writable
            && spl::read_token_account(a, &leg.keys.quote_program)
                .is_some_and(|t| t.owner == *cranker && t.mint == leg.pot.quote_mint)
    })
}

/// Pays `to` `amount` of the leg's token from the creator address's token account of it, the
/// creator address signing.
fn pay_token<'info>(
    leg: &Leg<'_, 'info>,
    creator: &AccountInfo<'info>,
    seeds: &CreatorSeeds,
    to: &AccountInfo<'info>,
    amount: u64,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    spl::transfer_checked(
        leg.program,
        leg.creator_quote,
        leg.quote_mint,
        to,
        creator,
        amount,
        leg.pot.decimals,
        &[&seeds.seeds()],
    )
}

/// The coin's token hook for an operation `op`: a custom hook with its extras resolved from its
/// registry, the kit with its three extras for a coin quoted in tokens (the config, the kit's id,
/// the `Legs`; a one-leg coin paying holder rewards in its token names its reward vault in place of
/// the kit's id, `docs/token-quotes.md` §7.5.1), or none.
fn coin_hook(
    all: &[AccountInfo],
    launch: &Launch,
    keys: &LegKeys,
    count: u8,
    op: &OpKeys,
) -> Result<(Option<Hook>, Vec<AccountMeta>)> {
    match launch.custom_hook {
        Some(hook) => Ok((
            Some(Hook::of(hook)),
            custom_extras(all, &hook, &launch.mint, op)?,
        )),
        None if launch.has_kit() => Ok((
            Some(Hook::of(KIT_ID)),
            if launch.reward_kind(count) == bordrless_kit::constants::REWARD_KIND_TOKEN {
                bordrless_kit::client::token_rewards_hook_extras(
                    &launch.mint,
                    launch_client::kit_vault_address(keys),
                )
            } else {
                bordrless_kit::client::legs_hook_extras(&launch.mint)
            },
        )),
        None => Ok((None, vec![])),
    }
}

/// The creator address's trade on the leg (`swap_spl`, `direction` 1 a buy paid from its token
/// account of the leg's token into its coin holding, 0 a sell back into it), the coin's hook slice
/// on the coin's side; with holder rewards the kit's vault of the leg after the leg's extras (the
/// leg hook pays the holder fee there).
#[allow(clippy::too_many_arguments)]
fn leg_trade(
    all: &[AccountInfo],
    launch: &Launch,
    keys: &LegKeys,
    count: u8,
    creator: Pubkey,
    direction: u8,
    amount_in: u64,
    min_out: u64,
) -> Result<Instruction> {
    let pool = keys.pool();
    let vault = bordrless_swap::client::vault_address(&pool, &launch.mint);
    let holding = token_client::holding_address(&launch.mint, &creator);
    let op = if direction == 1 {
        OpKeys {
            source: vault,
            destination: holding,
            authority: pool,
            source_owner: pool,
            destination_owner: creator,
        }
    } else {
        OpKeys {
            source: holding,
            destination: vault,
            authority: creator,
            source_owner: creator,
            destination_owner: pool,
        }
    };
    let slice = match coin_hook(all, launch, keys, count, &op)? {
        (Some(hook), extras) => launch_client::custom_hook_slice(hook.program, extras),
        (None, _) => vec![],
    };
    let mut ix = launch_client::leg_swap_with_slice(
        keys,
        creator,
        holding,
        keys.quote_account(&creator),
        direction,
        amount_in,
        min_out,
        slice,
        launch.rules.burns(),
    );
    if launch.rewards_on() {
        ix.accounts.push(AccountMeta::new(
            launch_client::kit_vault_address(keys),
            false,
        ));
    }
    Ok(ix)
}

/// The leg's pool, read (owner checked), and its price.
fn leg_pool(all: &[AccountInfo], keys: &LegKeys) -> Result<(Pool, u128)> {
    let info = find(all, &keys.pool())?;
    require_keys_eq!(*info.owner, SWAP_ID, CompanionError::MissingAccount);
    let pool = read::<Pool>(info)?;
    let price = spot_price(
        pool.quote_reserve,
        pool.virtual_quote,
        pool.base_reserve,
        pool.virtual_base,
    )
    .ok_or(CompanionError::NoQuote)?;
    Ok((pool, price))
}

/// How a claim of `rest` (after the bounty) splits by `c`'s split: `(buyback, holders, launcher,
/// pot)`, the rounding to the pot when there is one, else to the buyback when there is one, else to
/// the launcher (as `claim_fees`).
fn split_of(c: &Companion, rest: u64) -> Result<(u64, u64, u64, u64)> {
    let to_holders = bps_of(rest, u64::from(c.split.holders_bps));
    let to_beneficiary = bps_of(rest, u64::from(c.split.beneficiary_bps));
    if c.pot_bps > 0 {
        let to_buyback = bps_of(rest, u64::from(c.split.buyback_bps));
        let to_pot = rest
            .checked_sub(to_holders)
            .and_then(|v| v.checked_sub(to_beneficiary))
            .and_then(|v| v.checked_sub(to_buyback))
            .ok_or(CompanionError::MathOverflow)?;
        Ok((to_buyback, to_holders, to_beneficiary, to_pot))
    } else if c.split.buyback_bps > 0 {
        Ok((
            rest - to_holders - to_beneficiary,
            to_holders,
            to_beneficiary,
            0,
        ))
    } else {
        Ok((0, to_holders, rest - to_holders, 0))
    }
}

fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b)
        .ok_or_else(|| error!(CompanionError::MathOverflow))
}

/// `claim_fees_leg(index)`: see the module.
pub fn process_claim_fees_leg<'info>(ctx: Context<'info, LegStep<'info>>, index: u8) -> Result<()> {
    let c = &ctx.accounts.companion;
    require!(c.launched, CompanionError::NotLaunched);
    let launch = &ctx.accounts.launch;
    let creator = ctx.accounts.creator.to_account_info();
    let cranker = ctx.accounts.cranker.key();
    let seeds = CreatorSeeds::of(c);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let leg = leg_of(
        &all,
        launch,
        &ctx.accounts.companion_legs,
        creator.key,
        index,
    )?;
    // The leg's fee account into the creator address's token account (made the first time, the
    // sender paying), when it holds any.
    let fees = find(&all, &leg.keys.fee_account())?;
    if token_amount(fees, &leg.keys.quote_program) > 0 {
        let claim =
            launch_client::claim_creator_fees_token(cranker, *creator.key, &leg.keys, index);
        invoke_built(&claim, &all, &[])?;
    }
    let held = token_amount(leg.creator_quote, &leg.keys.quote_program);
    let set_aside = leg.pot.set_aside().ok_or(CompanionError::MathOverflow)?;
    let got = held.saturating_sub(set_aside);
    require!(got > 0, CompanionError::NothingToDo);
    let bounty_to = bounty_account(&all, &leg, &cranker);
    let bounty = match bounty_to {
        Some(_) => bps_of(got, u64::from(c.bounty_bps)),
        None => 0,
    };
    let (to_buyback, to_holders, to_beneficiary, to_pot) = split_of(c, got - bounty)?;
    if let Some(to) = bounty_to {
        pay_token(&leg, &creator, &seeds, to, bounty)?;
    }
    let companion = c.key();
    let pot = &mut ctx.accounts.companion_legs.legs[leg.i];
    pot.pending_buyback = add(pot.pending_buyback, to_buyback)?;
    pot.pending_holders = add(pot.pending_holders, to_holders)?;
    pot.pending_beneficiary = add(pot.pending_beneficiary, to_beneficiary)?;
    pot.pending_pot = add(pot.pending_pot, to_pot)?;
    pot.claimed_total = pot.claimed_total.saturating_add(got);
    pot.bounties_total = pot.bounties_total.saturating_add(bounty);
    emit_cpi!(LegFeesClaimed {
        companion,
        index,
        quote_mint: leg.pot.quote_mint,
        claimed: got,
        bounty,
        to_buyback,
        to_holders,
        to_beneficiary,
        to_pot,
        cranker,
    });
    Ok(())
}

/// `withdraw_leg(index)`: see the module.
pub fn process_withdraw_leg<'info>(
    ctx: Context<'info, LegWithdraw<'info>>,
    index: u8,
) -> Result<()> {
    let c = &ctx.accounts.companion;
    require!(c.launched, CompanionError::NotLaunched);
    let creator = ctx.accounts.creator.to_account_info();
    let seeds = CreatorSeeds::of(c);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let legs = &ctx.accounts.companion_legs;
    let leg = leg_of(&all, &ctx.accounts.launch, legs, creator.key, index)?;
    // With the SOL switch the launcher's part is sold to SOL first and paid by `withdraw`.
    require!(legs.pays_in_token(leg.i), CompanionError::SolSwitchOn);
    let amount = leg.pot.pending_beneficiary;
    require!(amount > 0, CompanionError::NothingToDo);
    let beneficiary = ctx.accounts.beneficiary.to_account_info();
    if c.version == VERSION_WITH_PAYEES {
        let sender = ctx.accounts.sender.to_account_info();
        let system = ctx.accounts.system_program.to_account_info();
        let out = pay_leg_payees(
            &all,
            &leg,
            &creator,
            &seeds,
            &sender,
            &beneficiary,
            &system,
            c.key(),
            &c.mint,
            amount,
        )?;
        require!(
            out.to_beneficiary > 0 || out.paid > 0,
            CompanionError::NothingToDo
        );
        let (companion, quote_mint, destination) = (
            c.key(),
            leg.pot.quote_mint,
            leg.keys.quote_account(beneficiary.key),
        );
        let pot = &mut ctx.accounts.companion_legs.legs[leg.i];
        pot.pending_beneficiary = out.owed;
        pot.paid_beneficiary_total = pot
            .paid_beneficiary_total
            .saturating_add(out.to_beneficiary.saturating_add(out.paid));
        for event in out.events {
            emit_cpi!(event);
        }
        if out.to_beneficiary > 0 {
            emit_cpi!(LegBeneficiaryPaid {
                companion,
                index,
                quote_mint,
                amount: out.to_beneficiary,
                destination,
            });
        }
        return Ok(());
    }
    let to = find(&all, &leg.keys.quote_account(beneficiary.key))?;
    spl::create_ata_idempotent(
        &ctx.accounts.sender.to_account_info(),
        to,
        &beneficiary,
        leg.quote_mint,
        &ctx.accounts.system_program.to_account_info(),
        leg.program,
        find(&all, &spl::ATA_PROGRAM_ID)?,
    )?;
    pay_token(&leg, &creator, &seeds, to, amount)?;
    let companion = c.key();
    let destination = *to.key;
    let pot = &mut ctx.accounts.companion_legs.legs[leg.i];
    pot.pending_beneficiary = 0;
    pot.paid_beneficiary_total = pot.paid_beneficiary_total.saturating_add(amount);
    emit_cpi!(LegBeneficiaryPaid {
        companion,
        index,
        quote_mint: leg.pot.quote_mint,
        amount,
        destination,
    });
    Ok(())
}

/// `buyback_leg(index)`: see the module.
pub fn process_buyback_leg<'info>(ctx: Context<'info, LegStep<'info>>, index: u8) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let c = &ctx.accounts.companion;
    require!(c.launched, CompanionError::NotLaunched);
    let launch = &ctx.accounts.launch;
    let creator = ctx.accounts.creator.to_account_info();
    let cranker = ctx.accounts.cranker.key();
    let seeds = CreatorSeeds::of(c);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let leg = leg_of(
        &all,
        launch,
        &ctx.accounts.companion_legs,
        creator.key,
        index,
    )?;
    let pot = leg.pot;
    require!(
        now >= c.launched_at.saturating_add(BUYBACK_AFTER_LAUNCH)
            && now >= pot.last_at.saturating_add(c.buyback_interval),
        CompanionError::BuybackNotDue
    );
    require!(pot.pending_buyback > 0, CompanionError::NothingToDo);
    let (pool, spot) = leg_pool(&all, &leg.keys)?;
    let ceiling = pot
        .reference_price
        .saturating_mul(u128::from(BPS + MAX_PREMIUM_BPS))
        / u128::from(BPS);
    if spot > ceiling {
        // Above the reference: no buy; the reference catches up toward the price.
        let companion = c.key();
        let interval = c.buyback_interval;
        let p = &mut ctx.accounts.companion_legs.legs[leg.i];
        if now >= p.reference_at.saturating_add(interval) {
            p.reference_price = catch_up(p.reference_price, spot, p.reference_at, interval, now);
            p.reference_at = now;
        }
        emit_cpi!(LegBuybackWaited {
            companion,
            index,
            price: spot,
            reference: pot.reference_price,
        });
        return Ok(());
    }
    let pool_side = u128::from(pool.quote_reserve) + u128::from(pool.virtual_quote);
    let pool_cap = (pool_side * u128::from(pool_share_bps(launch)) / u128::from(BPS))
        .min(u128::from(u64::MAX)) as u64;
    let total = pot.pending_buyback.min(pool_cap);
    require!(total > 0, CompanionError::NothingToDo);
    let bounty_to = bounty_account(&all, &leg, &cranker);
    let bounty = match bounty_to {
        Some(_) => bps_of(total, u64::from(c.bounty_bps)),
        None => 0,
    };
    let spend = total - bounty;
    // No less than the leg's own curve gives now, less the fees and the slippage.
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
    let mint = launch.mint;
    ensure_holding(&all, cranker, mint, *creator.key)?;
    let before = balance(&all, &mint, creator.key)?;
    let count = ctx.accounts.companion_legs.count;
    let buy = leg_trade(
        &all,
        launch,
        &leg.keys,
        count,
        *creator.key,
        1,
        spend,
        min_out,
    )?;
    invoke_built(&buy, &all, &[&seeds.seeds()])?;
    let bought = balance(&all, &mint, creator.key)?
        .checked_sub(before)
        .ok_or(CompanionError::MathOverflow)?;
    let holding = token_client::holding_address(&mint, creator.key);
    let (hook, extras) = coin_hook(
        &all,
        launch,
        &leg.keys,
        count,
        &OpKeys {
            source: holding,
            destination: mint,
            authority: *creator.key,
            source_owner: *creator.key,
            destination_owner: Pubkey::default(),
        },
    )?;
    invoke_built(
        &token_client::burn_with(*creator.key, holding, mint, hook, extras, bought),
        &all,
        &[&seeds.seeds()],
    )?;
    if let Some(to) = bounty_to {
        pay_token(&leg, &creator, &seeds, to, bounty)?;
    }
    let (_, price_after) = leg_pool(&all, &leg.keys)?;
    let companion = c.key();
    let p = &mut ctx.accounts.companion_legs.legs[leg.i];
    p.pending_buyback -= total;
    p.last_at = now;
    // Down only: the price this buy leaves is partly the buy itself.
    p.reference_price = p
        .reference_price
        .min(step_toward(p.reference_price, price_after));
    p.spent_total = p.spent_total.saturating_add(spend);
    p.burned_total = p.burned_total.saturating_add(bought);
    p.bounties_total = p.bounties_total.saturating_add(bounty);
    emit_cpi!(LegBoughtBack {
        companion,
        index,
        spent: spend,
        burned: bought,
        bounty,
        price_after,
        cranker,
    });
    Ok(())
}

/// The side a Snowball's step on a leg trades.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Buy,
    Sell,
}

/// `snowball_step_leg(index)`: see the module. The decision is `snowball_step`'s: the side the
/// price calls for (a drop of `buy_step_bps` from the leg's reference, or a rise of
/// `sell_step_bps`) when it can trade; else, with `flat_trades`, the side that rebalances what the
/// leg holds; else a follow (the reference to the price) when a step was called, or `SnowballNotDue`.
pub fn process_snowball_step_leg<'info>(
    ctx: Context<'info, LegSnowballStep<'info>>,
    index: u8,
) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let c = &ctx.accounts.companion;
    require!(c.is_snowball(), CompanionError::NotASnowball);
    require!(c.launched, CompanionError::NotLaunched);
    let s = &ctx.accounts.snowball;
    let launch = &ctx.accounts.launch;
    let creator = ctx.accounts.creator.to_account_info();
    let cranker = ctx.accounts.cranker.key();
    let seeds = CreatorSeeds::of(c);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let leg = leg_of(
        &all,
        launch,
        &ctx.accounts.companion_legs,
        creator.key,
        index,
    )?;
    let pot = leg.pot;
    // From a minute after the launch and the end of an early-buyer window, which runs from when
    // trading opened (the `Legs`).
    let mut opens = c.launched_at.saturating_add(BUYBACK_AFTER_LAUNCH);
    if launch.early_window_end > 0 {
        let legs = read::<Legs>(find(&all, &launch_client::legs_address(&launch.mint))?)?;
        let delay = legs.lock_delay(launch.created_at, now);
        opens = opens.max(launch.early_window_end.saturating_add(delay));
    }
    require!(
        now >= opens && now >= pot.snowball_last_at.saturating_add(s.interval),
        CompanionError::SnowballNotDue
    );
    let (pool, spot) = leg_pool(&all, &leg.keys)?;
    // The Snowball's own reference (review3 F1: never the buyback's).
    let reference = pot.snowball_reference;
    // What each side could trade: a share of the pot or of its coins, within the pool share.
    let base_side = u128::from(pool.base_reserve) + u128::from(pool.virtual_base);
    let quote_side = u128::from(pool.quote_reserve) + u128::from(pool.virtual_quote);
    let cap =
        (quote_side * u128::from(s.pool_bps) / u128::from(BPS)).min(u128::from(u64::MAX)) as u64;
    let buy_total = bps_of(pot.pending_pot, u64::from(s.buy_bps)).min(cap);
    let sell_tokens = bps_of(pot.tokens_held, u64::from(s.sell_bps))
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
    let can = |side: Side| match side {
        Side::Buy => buy_total >= MIN_LEG_TRADE,
        Side::Sell => sell_gross >= MIN_LEG_TRADE,
    };
    let called = if spot <= Snowball::buy_below(reference, s.buy_step_bps) {
        Some(Side::Buy)
    } else if spot >= Snowball::sell_above(reference, s.sell_step_bps) {
        Some(Side::Sell)
    } else {
        None
    };
    let (side, on_step) = match called {
        Some(side) if can(side) => (side, true),
        _ if !s.flat_trades => {
            require!(called.is_some(), CompanionError::SnowballNotDue);
            // The side the price called for has nothing to trade: the reference follows the price.
            let companion = c.key();
            let p = &mut ctx.accounts.companion_legs.legs[leg.i];
            p.snowball_reference = spot;
            p.snowball_last_at = now;
            emit_cpi!(LegSnowballFollowed {
                companion,
                index,
                reference_before: reference,
                price: spot,
            });
            return Ok(());
        }
        _ => {
            // Rebalance: sell while the leg's coins are worth more than its pot, else buy.
            let value = u128::from(pot.tokens_held) * spot / PRICE_SCALE;
            let preferred = if value > u128::from(pot.pending_pot) {
                Side::Sell
            } else {
                Side::Buy
            };
            let other = if preferred == Side::Buy {
                Side::Sell
            } else {
                Side::Buy
            };
            if can(preferred) {
                (preferred, false)
            } else if can(other) {
                (other, false)
            } else {
                return err!(CompanionError::NothingToDo);
            }
        }
    };
    let bounty_to = bounty_account(&all, &leg, &cranker);
    let mint = launch.mint;
    let companion = c.key();
    let bounty_bps = u64::from(c.bounty_bps);
    match side {
        Side::Buy => {
            let total = buy_total;
            let bounty = bounty_to.map_or(0, |_| bps_of(total, bounty_bps));
            let spend = total - bounty;
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
            ensure_holding(&all, cranker, mint, *creator.key)?;
            let before = balance(&all, &mint, creator.key)?;
            let count = ctx.accounts.companion_legs.count;
            let buy = leg_trade(
                &all,
                launch,
                &leg.keys,
                count,
                *creator.key,
                1,
                spend,
                min_out,
            )?;
            invoke_built(&buy, &all, &[&seeds.seeds()])?;
            let bought = balance(&all, &mint, creator.key)?
                .checked_sub(before)
                .ok_or(CompanionError::MathOverflow)?;
            if let Some(to) = bounty_to {
                pay_token(&leg, &creator, &seeds, to, bounty)?;
            }
            let (_, price_after) = leg_pool(&all, &leg.keys)?;
            let p = &mut ctx.accounts.companion_legs.legs[leg.i];
            p.pending_pot -= total;
            p.tokens_held = add(p.tokens_held, bought)?;
            p.snowball_reference = price_after;
            p.snowball_last_at = now;
            p.spent_total = p.spent_total.saturating_add(spend);
            p.bounties_total = p.bounties_total.saturating_add(bounty);
            emit_cpi!(LegSnowballTraded {
                companion,
                index,
                side: 1,
                on_step,
                quote: spend,
                tokens: bought,
                bounty,
                price_before: spot,
                price_after,
                pot_after: p.pending_pot,
                tokens_after: p.tokens_held,
                cranker,
            });
        }
        Side::Sell => {
            let tokens = sell_tokens;
            let net = bps_of(sell_gross, BPS.saturating_sub(sell_fee_bound(launch)));
            let min_out = bps_of(net, BPS - BUYBACK_SLIPPAGE_BPS);
            require!(min_out > 0, CompanionError::NoQuote);
            let before = token_amount(leg.creator_quote, &leg.keys.quote_program);
            let count = ctx.accounts.companion_legs.count;
            let sell = leg_trade(
                &all,
                launch,
                &leg.keys,
                count,
                *creator.key,
                0,
                tokens,
                min_out,
            )?;
            invoke_built(&sell, &all, &[&seeds.seeds()])?;
            let got = token_amount(leg.creator_quote, &leg.keys.quote_program)
                .checked_sub(before)
                .ok_or(CompanionError::MathOverflow)?;
            let bounty = bounty_to.map_or(0, |_| bps_of(got, bounty_bps));
            if let Some(to) = bounty_to {
                pay_token(&leg, &creator, &seeds, to, bounty)?;
            }
            let (_, price_after) = leg_pool(&all, &leg.keys)?;
            let p = &mut ctx.accounts.companion_legs.legs[leg.i];
            p.pending_pot = add(p.pending_pot, got - bounty)?;
            p.tokens_held = p
                .tokens_held
                .checked_sub(tokens)
                .ok_or(CompanionError::MathOverflow)?;
            p.snowball_reference = price_after;
            p.snowball_last_at = now;
            p.bounties_total = p.bounties_total.saturating_add(bounty);
            emit_cpi!(LegSnowballTraded {
                companion,
                index,
                side: 0,
                on_step,
                quote: got - bounty,
                tokens,
                bounty,
                price_before: spot,
                price_after,
                pot_after: p.pending_pot,
                tokens_after: p.tokens_held,
                cranker,
            });
        }
    }
    Ok(())
}

// ---- Payees in the legs' tokens (`withdraw_leg` of a companion with payees) -------------------------

/// Whether `info` can receive the leg's token for `owner` now: a token account of `program` of that
/// mint and owner, not frozen, and (Token-2022) not asking incoming transfers for a memo.
pub(crate) fn receivable(
    info: &AccountInfo,
    program: &Pubkey,
    mint: &Pubkey,
    owner: &Pubkey,
) -> bool {
    let Some(t) = spl::read_token_account(info, program) else {
        return false;
    };
    if t.mint != *mint || t.owner != *owner || !info.is_writable {
        return false;
    }
    let Ok(data) = info.try_borrow_data() else {
        return false;
    };
    // `AccountState`: 1 initialized, 2 frozen.
    if data[108] != 1 {
        return false;
    }
    // Token-2022 extensions from 166: `MemoTransfer` (8) asking for a memo on incoming transfers.
    let mut at = spl::ACCOUNT_LEN + 1;
    while at + 4 <= data.len() {
        let kind = u16::from_le_bytes([data[at], data[at + 1]]);
        let len = usize::from(u16::from_le_bytes([data[at + 2], data[at + 3]]));
        if kind == 8 && len >= 1 && data.get(at + 4).is_some_and(|b| *b != 0) {
            return false;
        }
        if kind == 0 && len == 0 {
            break;
        }
        at += 4 + len;
    }
    true
}

/// Pays `amount` of the leg's token to `wallet`'s associated token account of it, found among
/// `all`: made first when missing and the wallet itself is passed (the sender paying); paid when it
/// can receive. `false` when nothing was paid: the amount stays owed.
#[allow(clippy::too_many_arguments)]
fn pay_to<'info>(
    all: &[AccountInfo<'info>],
    leg: &Leg<'_, 'info>,
    creator: &AccountInfo<'info>,
    seeds: &CreatorSeeds,
    sender: &AccountInfo<'info>,
    system: &AccountInfo<'info>,
    wallet: &Pubkey,
    amount: u64,
) -> Result<bool> {
    let key = leg.keys.quote_account(wallet);
    let Some(dest) = all.iter().find(|a| *a.key == key) else {
        return Ok(false);
    };
    if *dest.owner == anchor_lang::system_program::ID && dest.data_is_empty() {
        let Some(owner) = all.iter().find(|a| a.key == wallet) else {
            return Ok(false);
        };
        spl::create_ata_idempotent(
            sender,
            dest,
            owner,
            leg.quote_mint,
            system,
            leg.program,
            find(all, &spl::ATA_PROGRAM_ID)?,
        )?;
    }
    if !receivable(dest, &leg.keys.quote_program, &leg.pot.quote_mint, wallet) {
        return Ok(false);
    }
    pay_token(leg, creator, seeds, dest, amount)?;
    Ok(true)
}

/// What `withdraw_leg` pays out of a companion with payees.
struct LegPayeesOutcome {
    /// Of the leg's launcher's part, what the beneficiary is paid now.
    to_beneficiary: u64,
    /// What the payees are paid now.
    paid: u64,
    /// What stays owed on the leg, payees' and beneficiary's: the leg's new `pending_beneficiary`.
    owed: u64,
    events: Vec<LegPayeePaid>,
}

/// `withdraw_leg` of a companion with payees (`docs/token-quotes.md` §7.5): what the leg's launcher's
/// part (`pending`) gained since the leg's last withdrawal is split as `withdraw` splits SOL, each
/// payee its basis points (rounded down), the beneficiary the rest; each party is paid its due
/// (its share and what it was owed on this leg) into its associated token account of the leg's
/// token ([`pay_to`]), or owed it in the companion's `PayeesLegs`. The `Payees` and the `PayeesLegs`
/// (writable) must be among `all` (`PayeesMissing`, `PayeesLegsMissing`).
#[allow(clippy::too_many_arguments)]
fn pay_leg_payees<'info>(
    all: &[AccountInfo<'info>],
    leg: &Leg<'_, 'info>,
    creator: &AccountInfo<'info>,
    seeds: &CreatorSeeds,
    sender: &AccountInfo<'info>,
    beneficiary: &AccountInfo<'info>,
    system: &AccountInfo<'info>,
    companion: Pubkey,
    mint: &Pubkey,
    pending: u64,
) -> Result<LegPayeesOutcome> {
    let payees_key = Payees::address(mint).0;
    let payees = all
        .iter()
        .find(|a| *a.key == payees_key && *a.owner == crate::ID)
        .ok_or(CompanionError::PayeesMissing)?;
    let p = read::<Payees>(payees)?;
    require!(p.mint == *mint, CompanionError::PayeesMissing);
    let legs_key = PayeesLegs::address(mint).0;
    let info = all
        .iter()
        .find(|a| *a.key == legs_key && a.is_writable && *a.owner == crate::ID)
        .ok_or(CompanionError::PayeesLegsMissing)?;
    let mut owed = read::<PayeesLegs>(info)?;
    let i = leg.i;
    let new = pending.saturating_sub(owed.owed_on(i));
    let count = usize::from(p.count).min(MAX_PAYEES);
    let (shares, rest) = payee_shares(new, &p.payees[..count]);
    let mut out = LegPayeesOutcome {
        to_beneficiary: 0,
        paid: 0,
        owed: 0,
        events: Vec::with_capacity(count),
    };
    for (k, (payee, share)) in p.payees[..count].iter().zip(shares).enumerate() {
        let due = owed.owed[i][k].saturating_add(share);
        if due == 0 {
            continue;
        }
        if pay_to(all, leg, creator, seeds, sender, system, &payee.wallet, due)? {
            owed.owed[i][k] = 0;
            out.paid = out.paid.saturating_add(due);
        } else {
            owed.owed[i][k] = due;
            out.owed = out.owed.saturating_add(due);
        }
        out.events.push(LegPayeePaid {
            companion,
            index: i as u8,
            payee: payee.wallet,
            amount: if owed.owed[i][k] == 0 { due } else { 0 },
            owed: owed.owed[i][k],
        });
    }
    let due = owed.beneficiary_owed[i].saturating_add(rest);
    if due > 0
        && pay_to(
            all,
            leg,
            creator,
            seeds,
            sender,
            system,
            beneficiary.key,
            due,
        )?
    {
        owed.beneficiary_owed[i] = 0;
        out.to_beneficiary = due;
    } else {
        owed.beneficiary_owed[i] = due;
        out.owed = out.owed.saturating_add(due);
    }
    let mut data = info.try_borrow_mut_data()?;
    let mut w: &mut [u8] = &mut data;
    owed.try_serialize(&mut w)?;
    Ok(out)
}

// ---- The holders' part in the leg's token (`share_leg`) --------------------------------------------

/// `share_leg(index)`: see the module. Remaining: the kit's `share_token` accounts for the creator
/// address (the kit, its config (w), the coin's `Legs`, the leg's quote mint, the creator address's
/// account of it (w), the kit's vault of the leg (w), the leg's token program, the kit's event
/// authority); the sender's token account of the leg's token for the bounty, when it wants one.
pub fn process_share_leg<'info>(ctx: Context<'info, LegStep<'info>>, index: u8) -> Result<()> {
    let c = &ctx.accounts.companion;
    require!(c.launched, CompanionError::NotLaunched);
    let launch = &ctx.accounts.launch;
    let creator = ctx.accounts.creator.to_account_info();
    let cranker = ctx.accounts.cranker.key();
    let seeds = CreatorSeeds::of(c);
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let legs = &ctx.accounts.companion_legs;
    let leg = leg_of(&all, launch, legs, creator.key, index)?;
    // With the SOL switch the holders' part is sold to SOL first and shared by `share`.
    require!(legs.pays_in_token(leg.i), CompanionError::SolSwitchOn);
    let total = leg.pot.pending_holders;
    require!(total > 0, CompanionError::NothingToDo);
    let bounty_to = bounty_account(&all, &leg, &cranker);
    let bounty = bounty_to.map_or(0, |_| bps_of(total, u64::from(c.bounty_bps)));
    let amount = total - bounty;
    require!(amount > 0, CompanionError::NothingToDo);
    let ix = bordrless_kit::client::share_token(
        *creator.key,
        launch.mint,
        index,
        leg.pot.quote_mint,
        leg.keys.quote_program,
        leg.keys.quote_account(creator.key),
        amount,
    );
    invoke_built(&ix, &all, &[&seeds.seeds()])?;
    if let Some(to) = bounty_to {
        pay_token(&leg, &creator, &seeds, to, bounty)?;
    }
    let (companion, quote_mint) = (c.key(), leg.pot.quote_mint);
    let p = &mut ctx.accounts.companion_legs.legs[leg.i];
    p.pending_holders -= total;
    p.shared_total = p.shared_total.saturating_add(amount);
    p.bounties_total = p.bounties_total.saturating_add(bounty);
    emit_cpi!(LegSharedWithHolders {
        companion,
        index,
        quote_mint,
        amount,
        bounty,
        cranker,
    });
    Ok(())
}

// ---- The SOL switch (`swap_bucket_to_sol`) ---------------------------------------------------------

/// Accounts of `swap_bucket_to_sol(index, part)`. Remaining: the launchpad's `swap_to_sol` accounts
/// from its fourth (the coin's `Legs`) on: the `Legs`, the leg's quote mint, the creator address's
/// account of it (w), then the rest as `swap_to_sol` takes them (`docs/token-quotes.md` §7.5.5).
#[event_cpi]
#[derive(Accounts)]
pub struct LegSwapToSol<'info> {
    /// Sends it; pays the wrapped-SOL account's rent for the swap (returned by it).
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked): the tokens' owner, which receives the SOL.
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    #[account(address = launch_client::launch_address(&companion.mint))]
    pub launch: Box<Account<'info, Launch>>,
    #[account(mut, seeds = [COMPANION_LEGS_SEED, companion.mint.as_ref()], bump = companion_legs.bump)]
    pub companion_legs: Box<Account<'info, CompanionLegs>>,
    pub system_program: Program<'info, System>,
    /// CHECK: the launchpad.
    #[account(address = LAUNCH_ID)]
    pub launch_program: UncheckedAccount<'info>,
}

/// `swap_bucket_to_sol(index, part)`: with the coin's SOL switch, leg `index`'s launcher's (`part`
/// 0) or holders' (`part` 1) bucket sold to SOL through the launchpad's `swap_to_sol`, the creator
/// address signing as the tokens' owner (the launchpad takes at most what the token's reference
/// allows this window, at no less than its minimum from the reference pool's price). What the
/// bucket's tokens brought is credited to the companion's SOL part of the same name
/// (`pending_beneficiary`, `pending_holders`), which `withdraw` (with any payees) and `share` pay;
/// the bucket keeps what was not sold.
pub fn process_swap_bucket_to_sol<'info>(
    ctx: Context<'info, LegSwapToSol<'info>>,
    index: u8,
    part: u8,
) -> Result<()> {
    let c = &ctx.accounts.companion;
    require!(c.launched, CompanionError::NotLaunched);
    // A game's pot is sold too (`docs/token-quotes.md` §7.6): with the switch, a game pays as a SOL
    // coin's does. A script's and a Snowball's pots stay in the legs' tokens.
    require!(
        part == PART_LAUNCHER || part == PART_HOLDERS || (part == PART_POT && c.is_game()),
        CompanionError::NotSwitchedPart
    );
    let creator = ctx.accounts.creator.to_account_info();
    let seeds = CreatorSeeds::of(c);
    let rest = ctx.remaining_accounts;
    let all = available(ctx.accounts.to_account_infos(), rest);
    let legs = &ctx.accounts.companion_legs;
    let leg = leg_of(&all, &ctx.accounts.launch, legs, creator.key, index)?;
    require!(!legs.pays_in_token(leg.i), CompanionError::SolSwitchOff);
    let bucket = match part {
        PART_LAUNCHER => leg.pot.pending_beneficiary,
        PART_HOLDERS => leg.pot.pending_holders,
        _ => leg.pot.pending_pot,
    };
    require!(bucket > 0, CompanionError::NothingToDo);
    // The swap's `Legs`, quote mint and source: the coin's, the leg's, the creator address's.
    require!(
        rest.len() >= 3
            && *rest[0].key == launch_client::legs_address(&c.mint)
            && *rest[1].key == leg.pot.quote_mint
            && *rest[2].key == leg.keys.quote_account(creator.key),
        CompanionError::MissingAccount
    );
    let tokens_before = token_amount(leg.creator_quote, &leg.keys.quote_program);
    let lamports_before = creator.lamports();
    let a = &ctx.accounts;
    let meta = |i: &AccountInfo| AccountMeta {
        pubkey: *i.key,
        is_signer: false,
        is_writable: i.is_writable,
    };
    let mut metas = vec![
        AccountMeta::new(a.cranker.key(), true),
        AccountMeta::new(*creator.key, true),
        AccountMeta::new_readonly(a.launch.key(), false),
    ];
    metas.extend(rest.iter().map(meta));
    let mut infos = vec![
        a.cranker.to_account_info(),
        creator.clone(),
        a.launch.to_account_info(),
    ];
    infos.extend(rest.iter().cloned());
    infos.push(a.launch_program.to_account_info());
    anchor_lang::solana_program::program::invoke_signed(
        &Instruction {
            program_id: LAUNCH_ID,
            accounts: metas,
            data: bordrless_launch::instruction::SwapToSol { amount_in: bucket }.data(),
        },
        &infos,
        &[&seeds.seeds()],
    )?;
    let sold = tokens_before
        .checked_sub(token_amount(leg.creator_quote, &leg.keys.quote_program))
        .ok_or(CompanionError::MathOverflow)?;
    let lamports = creator
        .lamports()
        .checked_sub(lamports_before)
        .ok_or(CompanionError::MathOverflow)?;
    let (companion, quote_mint, cranker) = (c.key(), leg.pot.quote_mint, a.cranker.key());
    let p = &mut ctx.accounts.companion_legs.legs[leg.i];
    let bucket = match part {
        PART_LAUNCHER => &mut p.pending_beneficiary,
        PART_HOLDERS => &mut p.pending_holders,
        _ => &mut p.pending_pot,
    };
    *bucket = bucket
        .checked_sub(sold)
        .ok_or(CompanionError::MathOverflow)?;
    p.swapped_total = p.swapped_total.saturating_add(sold);
    let c = &mut ctx.accounts.companion;
    match part {
        PART_LAUNCHER => c.pending_beneficiary = add(c.pending_beneficiary, lamports)?,
        PART_HOLDERS => c.pending_holders = add(c.pending_holders, lamports)?,
        _ => c.pending_pot = add(c.pending_pot, lamports)?,
    }
    emit_cpi!(LegSwappedToSol {
        companion,
        index,
        quote_mint,
        part,
        amount_in: sold,
        lamports,
        cranker,
    });
    Ok(())
}
