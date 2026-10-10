//! An open vault's steps, each permissionless: `execute` (a slot's burn, or its sell), `execute_buy`
//! (a `SellBuyBurn` slot's buy and burn of the other token) and `retire` (a slot left for 60 days).
//! Every step is a few calls into Bordrless programs (the DEX through the launchpad's client, the
//! token program, the bridge's `unwrap_sol`, and a system transfer for a payment), built here with
//! each program's own client (`invoke.rs`), and signed by the one slot owner the step runs.
//!
//! The guards of the sells and buys are a port of the companion's buyback guards
//! (`bordrless_companion::instructions::steps::process_buyback`), whose economics were audited:
//!
//! - one trade moves at most a small slice of the pool (`vault_share_bps`, at most
//!   `max_sell_bps`), smaller than the fees a sandwich around it pays, so a sandwich costs more than
//!   it makes. The slice counts only the fees nobody gets back: the creator fee returns to the
//!   coin's creator, so it is counted as Bordrless's share of it alone, and the creator's own
//!   sandwich does not pay either;
//! - the slice is the vault's, not each slot's: each of the `n` selling slots sells at most `1/n` of
//!   it a sale, and a slot sells at most once an interval, so the vault's sales in any interval (or
//!   in one bundle) move no more than one slice, and no slot's part can be taken by another (crank
//!   order decides nothing); a vault has at most one buy slot a pool;
//! - trades are at least `interval` apart, and none runs in the launch's first minute;
//! - a trade takes no less than the pool's own quote less the fees (the hooks' declared cuts among
//!   them) and 2% (`min_out`);
//! - a reference price: a buy waits while the price is more than 3% above it, a sell (the mirror)
//!   while the price is more than 3% below it. After a wait the slot's next attempt is a minute
//!   later (`MIN_INTERVAL`, `waited_at`): a wait and a trade never share a transaction or a block,
//!   and a one-block dip costs the slot a minute, not an interval. A wait moves the reference
//!   toward the price by 5% for each interval since its clock last restarted. Every trade restarts
//!   that clock, and moves the reference only away from where the trade pushes the price (a buy
//!   only lowers it, a sell only raises it), since the price a trade leaves is partly the trade
//!   itself, or a front-runner's.
//!
//! Residual (not closed here): any number of vaults may buy the same token X on its pool, and the
//! companion's buyback of X trades there too. A bundle that cranks every due buy on one pool stacks
//! their slices; each vault's slice alone is safe, `k` of them together pay a sandwich once
//! `k` slices exceed half the round-trip fees. Closing it needs a protocol-wide per-pool limiter.
//!
//! Depth: vault (1) → DEX (2) → token (3) → the coin's hook (4), and DEX (2) → launch hook (3); the
//! buy reaches the other token's hook (the kit, or its own) at 4; `unwrap_sol` reaches 3.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_core::swap_out;
use bordrless_launch::client as launch_client;
use bordrless_launch::state::Launch;
use bordrless_token::client::{self as token_client, Hook};

use crate::constants::*;
use crate::error::VaultError;
use crate::events::*;
use crate::instructions::common::*;
use crate::invoke::invoke_built;
use crate::state::*;

/// Accounts of the steps. Remaining: the accounts of the instructions the step invokes (the
/// client's builders list them), the coin's hook registry, the pools.
#[event_cpi]
#[derive(Accounts)]
pub struct Step<'info> {
    /// Whoever sends it; paid the bounty (as SOL), and pays any holding a step creates.
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(mut, seeds = [VAULT_SEED, vault.mint.as_ref()], bump = vault.bump)]
    pub vault: Box<Account<'info, Vault>>,
    /// CHECK: the slot's owner, `PDA(["slot", mint, [i]])` (checked against slot `i`'s seeds).
    #[account(mut)]
    pub slot_owner: UncheckedAccount<'info>,
    #[account(address = launch_client::launch_address(&vault.mint))]
    pub launch: Box<Account<'info, Launch>>,
    pub system_program: Program<'info, System>,
}

/// Burns `amount` of the slot's coin, the slot's owner signing, with the coin's hook and its extras
/// for the burn resolved from its registry.
fn burn_coin(
    all: &[AccountInfo],
    hook: Pubkey,
    mint: &Pubkey,
    seeds: &SlotSeeds,
    owner: Pubkey,
    amount: u64,
) -> Result<()> {
    let op = OpKeys::burn(mint, owner);
    let extras = custom_extras(all, &hook, mint, &op)?;
    let ix = token_client::burn_with(
        owner,
        op.source,
        *mint,
        Some(Hook::of(hook)),
        extras,
        amount,
    );
    invoke_built(&ix, all, &[&seeds.seeds()])
}

/// A sell of the slot's coin on the coin's launch pool, delivered to the slot's bridged-SOL
/// holding: the launchpad client's swap (direction 0) with the coin's hook slice on the coin's
/// side, resolved for the transfer from the slot's holding to the pool's vault (the mirror of the
/// companion's `creator_buy`).
fn slot_sell(
    all: &[AccountInfo],
    launch: &Launch,
    hook: Pubkey,
    owner: Pubkey,
    amount_in: u64,
    min_out: u64,
) -> Result<Instruction> {
    let keys = launch_client::LaunchKeys::of(launch);
    let op = OpKeys {
        source: token_client::holding_address(&launch.mint, &owner),
        destination: bordrless_swap::client::vault_address(&launch.pool, &launch.mint),
        authority: owner,
        source_owner: owner,
        destination_owner: launch.pool,
    };
    let extras = custom_extras(all, &hook, &launch.mint, &op)?;
    Ok(launch_client::swap_with_base_slice(
        &keys,
        owner,
        owner,
        0,
        amount_in,
        min_out,
        launch_client::custom_hook_slice(hook, extras),
    ))
}

/// A buy of the other token on its launch pool by the slot's owner, delivered to it (the
/// companion's `creator_buy`): the launchpad client's `swap` for a kit (or hook-less) token; for a
/// custom-hook token, with its hook's slice resolved for the pool vault's transfer to the owner.
fn slot_buy(
    all: &[AccountInfo],
    launch: &Launch,
    owner: Pubkey,
    amount_in: u64,
    min_out: u64,
) -> Result<Instruction> {
    let keys = launch_client::LaunchKeys::of(launch);
    let Some(hook) = launch.custom_hook else {
        return Ok(launch_client::swap(
            &keys, owner, owner, 1, amount_in, min_out,
        ));
    };
    let op = OpKeys {
        source: bordrless_swap::client::vault_address(&launch.pool, &launch.mint),
        destination: token_client::holding_address(&launch.mint, &owner),
        authority: launch.pool,
        source_owner: launch.pool,
        destination_owner: owner,
    };
    let extras = custom_extras(all, &hook, &launch.mint, &op)?;
    Ok(launch_client::swap_with_base_slice(
        &keys,
        owner,
        owner,
        1,
        amount_in,
        min_out,
        launch_client::custom_hook_slice(hook, extras),
    ))
}

/// Whether `lamports` can be paid to `to`: a wallet that exists, or a payment that opens it (at
/// least a wallet's rent-exempt minimum). A smaller payment to an empty address would fail the
/// whole transaction, so it waits.
fn payable(to: &AccountInfo, lamports: u64, rent_min: u64) -> bool {
    to.lamports() > 0 || lamports >= rent_min
}

// ---- execute ----------------------------------------------------------------------------------------

/// `execute(i)`: runs slot `i`'s policy.
///
/// - `Burn`: burns the slot's whole coin balance (no bounty, no interval: burning is never a trade
///   anyone can sandwich).
/// - `SellForSol`, `SellBuyBurn`: sells at most a slice of the slot's coin on the coin's launch
///   pool, under the guards (see the module's documentation), and waits instead while the price is
///   more than 3% below the sell's reference. The proceeds land in the slot's bridged-SOL holding;
///   the bounty (`bounty_bps` of them) is paid as SOL. `SellForSol` pays the rest (and anything held
///   back before) to its wallet as SOL, or holds it back (`pending_sol`) while the wallet is empty
///   and the payment can't open it. `SellBuyBurn` keeps it for `execute_buy`.
///
/// Remaining: the coin's hook registry; for a burn, the token `burn`'s accounts; for a sell, the
/// DEX `swap`'s, the coin's pool, the bridge's `unwrap_sol`'s, and the slot's wallet
/// (`SellForSol`).
pub fn process_execute<'info>(ctx: Context<'info, Step<'info>>, i: u8) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let v = &ctx.accounts.vault;
    require!(v.opened, VaultError::NotOpen);
    let (slot, seeds) = slot_of(v, i, ctx.accounts.slot_owner.key)?;
    let (mint, hook, owner) = (v.mint, v.hook, ctx.accounts.slot_owner.key());
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let held = balance(&all, &mint, &owner)?;
    let at = usize::from(i);

    if slot.policy == policy::BURN {
        require!(held > 0, VaultError::NothingToDo);
        burn_coin(&all, hook, &mint, &seeds, owner, held)?;
        let v = &mut ctx.accounts.vault;
        let s = &mut v.slots[at];
        s.burned = s.burned.saturating_add(held);
        s.last_at = now;
        v.last_activity_at = now;
        emit_cpi!(SlotBurned {
            vault: v.key(),
            slot: i,
            amount: held,
            cranker: ctx.accounts.cranker.key(),
        });
        return Ok(());
    }

    let launch = &ctx.accounts.launch;
    let due_at = slot
        .last_at
        .checked_add(v.interval)
        .ok_or(VaultError::MathOverflow)?
        .max(slot.waited_at.saturating_add(MIN_INTERVAL));
    require!(
        now >= launch.created_at.saturating_add(AFTER_LAUNCH) && now >= due_at,
        VaultError::NotDue
    );
    let rent_min = Rent::get()?.minimum_balance(0);
    let cranker = ctx.accounts.cranker.to_account_info();

    if held == 0 {
        // Nothing to sell: only a payment held back earlier, once the wallet can take it.
        require!(
            slot.policy == policy::SELL_FOR_SOL && slot.pending_sol > 0,
            VaultError::NothingToDo
        );
        let to = find(&all, &slot.target)?;
        require!(
            payable(to, slot.pending_sol, rent_min),
            VaultError::NothingToDo
        );
        let paid = slot.pending_sol;
        pay_sol(&all, &seeds, owner, &[(to, paid)])?;
        let v = &mut ctx.accounts.vault;
        let s = &mut v.slots[at];
        s.pending_sol = 0;
        v.last_activity_at = now;
        emit_cpi!(PendingPaid {
            vault: v.key(),
            slot: i,
            paid,
        });
        return Ok(());
    }

    let pool = read_pool(&all, &launch.pool)?;
    let price = pool_price(&pool)?;
    let floor = slot
        .reference_price
        .saturating_mul(u128::from(BPS - MAX_DISCOUNT_BPS))
        / u128::from(BPS);
    if price < floor {
        // Below the reference: no sell. The next attempt is a minute later (`waited_at`), so no sell
        // follows a wait in the same transaction or block. The reference comes down toward the
        // price, a step for each interval since its clock last restarted (a sale, or a wait that
        // stepped it): retries within an interval don't step it again.
        let interval = v.interval;
        let v = &mut ctx.accounts.vault;
        let s = &mut v.slots[at];
        let reference = s.reference_price;
        if now >= s.reference_at.saturating_add(interval) {
            s.reference_price = catch_up(reference, price, s.reference_at, interval, now);
            s.reference_at = now;
        }
        s.waited_at = now;
        let new_reference = s.reference_price;
        v.last_activity_at = now;
        emit_cpi!(SellWaited {
            vault: v.key(),
            slot: i,
            price,
            reference,
            new_reference,
        });
        return Ok(());
    }

    // At most this slot's part of the vault's slice: the slice split evenly among the selling
    // slots, valued at the spot price. Each slot sells at most once an interval, so the vault's
    // sales in any interval (or one bundle) move at most one slice, and no slot's part is another's
    // to take (audit 3a vault r1 F1, r2 V1).
    let share =
        u64::from(v.max_sell_bps).min(vault_share_bps(launch, u64::from(pool.protocol_share_bps)));
    let part = quote_cap(&pool, share) / selling_slots(v);
    let amount_in = held.min(base_worth(&pool, part));
    require!(amount_in > 0, VaultError::NothingToDo);
    // No less than the pool's own quote now, less the fees (the hook's declared cut among them)
    // and the slippage: never at any price.
    let bound = sell_fee_bound(
        launch,
        u64::from(pool.protocol_share_bps),
        v.max_hook_cut_bps,
    );
    let net = bps_of(amount_in, BPS - bound);
    let quote = swap_out(
        net,
        pool.base_reserve,
        pool.virtual_base,
        pool.quote_reserve,
        pool.virtual_quote,
    )
    .ok_or(VaultError::NoQuote)?;
    let min_out = bps_of(quote, BPS - SLIPPAGE_BPS);
    require!(min_out > 0, VaultError::NoQuote);
    let before = balance(&all, &BRIDGED_SOL_MINT, &owner)?;
    let sell = slot_sell(&all, launch, hook, owner, amount_in, min_out)?;
    invoke_built(&sell, &all, &[&seeds.seeds()])?;
    let got = balance(&all, &BRIDGED_SOL_MINT, &owner)?
        .checked_sub(before)
        .ok_or(VaultError::MathOverflow)?;
    let bounty = bps_of(got, u64::from(v.bounty_bps));
    let kept = got - bounty;
    let (paid, pending) = if slot.policy == policy::SELL_FOR_SOL {
        let owed = slot
            .pending_sol
            .checked_add(kept)
            .ok_or(VaultError::MathOverflow)?;
        let to = find(&all, &slot.target)?;
        if payable(to, owed, rent_min) {
            pay_sol(&all, &seeds, owner, &[(&cranker, bounty), (to, owed)])?;
            (owed, 0)
        } else {
            pay_sol(&all, &seeds, owner, &[(&cranker, bounty)])?;
            (0, owed)
        }
    } else {
        pay_sol(&all, &seeds, owner, &[(&cranker, bounty)])?;
        let pending = slot
            .pending_sol
            .checked_add(kept)
            .ok_or(VaultError::MathOverflow)?;
        (0, pending)
    };
    let after = read_pool(&all, &launch.pool)?;
    let v = &mut ctx.accounts.vault;
    let s = &mut v.slots[at];
    if let Ok(price_after) = pool_price(&after) {
        // Up only: the price this sell leaves is partly the sell itself (or a front-runner's).
        s.reference_price = s
            .reference_price
            .max(step_toward(s.reference_price, price_after));
    }
    // Every sale restarts the reference's clock: the price was acceptable now, so the intervals
    // before it never count toward a later wait's catch-up (stricter than the companion's buy,
    // whose wait catches up from the reference's last wait).
    s.reference_at = now;
    s.pending_sol = pending;
    s.last_at = now;
    s.sold = s.sold.saturating_add(amount_in);
    s.sol_out = s.sol_out.saturating_add(got);
    s.bounties = s.bounties.saturating_add(bounty);
    let reference = s.reference_price;
    v.last_activity_at = now;
    emit_cpi!(SlotSold {
        vault: v.key(),
        slot: i,
        sold: amount_in,
        got,
        bounty,
        paid,
        pending_sol: pending,
        price,
        reference,
        cranker: ctx.accounts.cranker.key(),
    });
    Ok(())
}

// ---- execute_buy ------------------------------------------------------------------------------------

/// `execute_buy(i)`: a `SellBuyBurn` slot's buy, the companion's buyback as is, on the other token's
/// launch pool (`slot.target`): at least `interval` after the slot's last buy and the token's first
/// minute (and its early-buyer window, whose buys the kit would lock); waits while the token's price
/// is more than 3% above the buy's reference (the next attempt is then a minute later); spends at
/// most `vault_share_bps` of that pool's quote side of the SOL the sells left (`pending_sol`), the
/// bounty paid from it, and, for a token whose kit caps wallets until it graduates, no more than
/// what keeps the slot's owner within the cap; takes no less than the pool's quote less the token's
/// buy fees, its hook's declared cut (`max_cut_bps`) and 2%; then burns all it bought. The reference only
/// comes down after a buy. Remaining: the DEX `swap`'s accounts, the pool, the token's launch, the
/// slot owner's holding of the token (created when missing, the sender paying), the token `burn`'s
/// and the bridge's `unwrap_sol`'s; for a custom-hook token, its registry.
pub fn process_execute_buy<'info>(ctx: Context<'info, Step<'info>>, i: u8) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let v = &ctx.accounts.vault;
    require!(v.opened, VaultError::NotOpen);
    let (slot, seeds) = slot_of(v, i, ctx.accounts.slot_owner.key)?;
    require!(
        slot.policy == policy::SELL_BUY_BURN,
        VaultError::NotSellBuyBurn
    );
    let owner = ctx.accounts.slot_owner.key();
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let pool = read_pool(&all, &slot.target)?;
    let x_launch = Box::new(read::<Launch>(find(
        &all,
        &launch_client::launch_address(&pool.base_mint),
    )?)?);
    require_keys_eq!(x_launch.pool, slot.target, VaultError::BadPool);
    let x_mint = x_launch.mint;
    let due_at = slot
        .buy_last_at
        .checked_add(v.interval)
        .ok_or(VaultError::MathOverflow)?
        .max(slot.buy_waited_at.saturating_add(MIN_INTERVAL));
    require!(
        now >= x_launch.created_at.saturating_add(AFTER_LAUNCH)
            && now >= x_launch.early_window_end
            && now >= due_at,
        VaultError::NotDue
    );
    require!(slot.pending_sol > 0, VaultError::NothingToDo);
    let at = usize::from(i);
    let price = pool_price(&pool)?;
    let ceiling = slot
        .buy_reference
        .saturating_mul(u128::from(BPS + MAX_PREMIUM_BPS))
        / u128::from(BPS);
    if price > ceiling {
        // Above the reference: no buy. The next attempt is a minute later (no buy follows a wait in
        // the same transaction or block). The reference catches up toward the price, a step for
        // each interval since its clock last restarted.
        let interval = v.interval;
        let v = &mut ctx.accounts.vault;
        let s = &mut v.slots[at];
        let reference = s.buy_reference;
        if now >= s.buy_reference_at.saturating_add(interval) {
            s.buy_reference = catch_up(reference, price, s.buy_reference_at, interval, now);
            s.buy_reference_at = now;
        }
        s.buy_waited_at = now;
        let new_reference = s.buy_reference;
        v.last_activity_at = now;
        emit_cpi!(BuyWaited {
            vault: v.key(),
            slot: i,
            price,
            reference,
            new_reference,
        });
        return Ok(());
    }
    let share_bps = u64::from(pool.protocol_share_bps);
    let mut total = slot
        .pending_sol
        .min(quote_cap(&pool, vault_share_bps(&x_launch, share_bps)));
    require!(total > 0, VaultError::NothingToDo);
    let mut bounty = bps_of(total, u64::from(v.bounty_bps));
    let mut spend = total - bounty;
    ensure_holding(&all, ctx.accounts.cranker.key(), x_mint, owner)?;
    let op = OpKeys::burn(&x_mint, owner);
    let (x_hook, x_extras) = token_hook(&all, &x_launch, &op)?;
    let burn_x = |amount: u64| {
        token_client::burn_with(owner, op.source, x_mint, x_hook, x_extras.clone(), amount)
    };
    // The slot never keeps the token: whatever someone sent its owner is burned first (it would
    // only take the room a wallet cap leaves the buy).
    let donated = balance(&all, &x_mint, &owner)?;
    if donated > 0 {
        invoke_built(&burn_x(donated), &all, &[&seeds.seeds()])?;
    }
    // A token whose kit caps wallets (until it graduates): the slot's owner is a holder like any
    // other, so the buy is cut to what the cap leaves room for (F5), never refused by the kit.
    if let Some(room) = wallet_room(&all, &x_launch)? {
        let most = spend_for_at_most(&pool, room).unwrap_or(u64::MAX);
        if spend > most {
            spend = most;
            bounty = bps_of(spend, u64::from(v.bounty_bps));
            total = spend + bounty;
        }
        require!(spend > 0, VaultError::NoRoom);
    }
    // No less than the pool's own quote now, less the fees (the token's own hook's declared cut
    // among them) and the slippage: never at any price.
    let net = bps_of(
        spend,
        BPS.saturating_sub(buy_bound_with_cut(&x_launch, share_bps, slot.max_cut_bps)),
    );
    let quote = swap_out(
        net,
        pool.quote_reserve,
        pool.virtual_quote,
        pool.base_reserve,
        pool.virtual_base,
    )
    .ok_or(VaultError::NoQuote)?;
    let min_out = bps_of(quote, BPS - SLIPPAGE_BPS);
    require!(min_out > 0, VaultError::NoQuote);
    let buy = slot_buy(&all, &x_launch, owner, spend, min_out)?;
    invoke_built(&buy, &all, &[&seeds.seeds()])?;
    let bought = balance(&all, &x_mint, &owner)?;
    invoke_built(&burn_x(bought), &all, &[&seeds.seeds()])?;
    let burned = bought.saturating_add(donated);
    let cranker = ctx.accounts.cranker.to_account_info();
    pay_sol(&all, &seeds, owner, &[(&cranker, bounty)])?;
    let after = read_pool(&all, &slot.target)?;
    let v = &mut ctx.accounts.vault;
    let s = &mut v.slots[at];
    if let Ok(price_after) = pool_price(&after) {
        // Down only: the price this buy leaves is partly the buy itself (or a front-runner's).
        s.buy_reference = s
            .buy_reference
            .min(step_toward(s.buy_reference, price_after));
    }
    // Every buy restarts the buy reference's clock (as a sale does the sell's).
    s.buy_reference_at = now;
    s.pending_sol -= total;
    s.buy_last_at = now;
    s.x_burned = s.x_burned.saturating_add(burned);
    s.bounties = s.bounties.saturating_add(bounty);
    let pending_sol = s.pending_sol;
    v.last_activity_at = now;
    emit_cpi!(SlotBought {
        vault: v.key(),
        slot: i,
        spent: spend,
        burned,
        bounty,
        pending_sol,
        cranker: ctx.accounts.cranker.key(),
    });
    Ok(())
}

// ---- retire -----------------------------------------------------------------------------------------

/// `retire(i, burn_coin)`: anyone, once slot `i` has not run for `RETIRE_SECS` (60 days, counted from
/// the latest of the vault's opening, the slot's last sell, burn, buy, retirement or guard wait: a
/// slot cranked while its guard waits is live, and is never retired) and holds
/// something: its `pending_sol` is unwrapped and sent to the incinerator, and with `burn_coin` its
/// coin is burned. Nobody receives anything, so retiring can only ever destroy what the slot would
/// have sold or bought, never redirect it.
///
/// Why a slot gets stuck: the coin's hook refuses the vault's transfer (every sell fails: a failed
/// step changes nothing, so it is no run), the other token's pool or hook refuses the buy, or
/// nobody cranks. A guard that waits is a run: its reference reaches the price at 5% an interval. `burn_coin` exists
/// because a hook may refuse burns too: then a burn would fail the whole call, and the SOL could
/// never be retired. With `burn_coin = false` the coin stays in the slot's holding, which no
/// instruction but this program's can move.
///
/// Remaining: the bridge's `unwrap_sol` accounts and the incinerator (with SOL to burn); the token
/// `burn`'s accounts and the coin's hook registry (with `burn_coin`).
pub fn process_retire<'info>(
    ctx: Context<'info, Step<'info>>,
    i: u8,
    burn_coin_too: bool,
) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let v = &ctx.accounts.vault;
    require!(v.opened, VaultError::NotOpen);
    let (slot, seeds) = slot_of(v, i, ctx.accounts.slot_owner.key)?;
    require!(
        now >= slot.last_run(v.opened_at).saturating_add(RETIRE_SECS),
        VaultError::NotRetirable
    );
    let (mint, hook, owner) = (v.mint, v.hook, ctx.accounts.slot_owner.key());
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let sol = slot.pending_sol;
    let coin = if burn_coin_too {
        balance(&all, &mint, &owner)?
    } else {
        0
    };
    require!(sol > 0 || coin > 0, VaultError::NothingToDo);
    if sol > 0 {
        let incinerator = find(&all, &INCINERATOR)?;
        pay_sol(&all, &seeds, owner, &[(incinerator, sol)])?;
    }
    if coin > 0 {
        burn_coin(&all, hook, &mint, &seeds, owner, coin)?;
    }
    let v = &mut ctx.accounts.vault;
    let s = &mut v.slots[usize::from(i)];
    s.pending_sol = 0;
    s.burned = s.burned.saturating_add(coin);
    s.last_at = now;
    v.last_activity_at = now;
    emit_cpi!(SlotRetired {
        vault: v.key(),
        slot: i,
        sol_burned: sol,
        coin_burned: coin,
    });
    Ok(())
}
