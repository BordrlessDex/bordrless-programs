//! `create_vault` and `open_vault`: a vault made for a coin that does not exist yet, its policies
//! fixed there for good, then opened once the coin is launched.

use anchor_lang::prelude::*;
use anchor_lang::system_program;
use bordrless_launch::client as launch_client;
use bordrless_launch::state::Launch;
use bordrless_swap::state::Pool;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::VaultError;
use crate::events::{SlotPolicy, VaultCreated, VaultOpened};
use crate::instructions::common::*;
use crate::state::*;

/// A slot's policy as `create_vault` takes it.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotArgs {
    /// `policy::BURN`, `SELL_FOR_SOL` or `SELL_BUY_BURN`.
    pub policy: u8,
    /// `SellForSol`: the wallet paid. `SellBuyBurn`: the pool of the token bought and burned. The
    /// default key for `Burn`.
    pub target: Pubkey,
    /// `SellBuyBurn` of a token with its own custom hook: the most that hook may cut from the
    /// slot's buy (basis points, at most 5,000), included in the buy's `min_out`. 0 otherwise.
    pub max_cut_bps: u16,
}

/// Arguments of `create_vault`. None of them can change afterwards: no instruction writes them.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct CreateVaultArgs {
    /// The coin's own token hook: the program its `LaunchConfig` names (`custom_hook`). It sends the
    /// cuts; `open_vault` checks the launch and the mint run it.
    pub hook: Pubkey,
    /// One to three slots, in the order the hook names them.
    pub slots: Vec<SlotArgs>,
    /// The crank's pay: at most 100 basis points of the SOL a step moves.
    pub bounty_bps: u16,
    /// The most one sell takes of the pool's quote side: 1 to 100 basis points (and never more
    /// than the launch's `pool_share_bps`).
    pub max_sell_bps: u16,
    /// The least time between two sells of a slot (and two buys): a minute to 30 days.
    pub interval: i64,
    /// The most the coin's own hook may cut from the vault's sell: at most 5,000 basis points.
    pub max_hook_cut_bps: u16,
}

/// Accounts of `create_vault`. Remaining: for each `SellBuyBurn` slot, its pool and the pool's
/// launch (the token bought, `launch_address(pool.base_mint)`); for each `SellForSol` slot, its
/// wallet.
#[event_cpi]
#[derive(Accounts)]
pub struct CreateVault<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The coin's mint, a fresh keypair: it signs here, so only whoever holds it can make its vault
    /// (nobody can make one first and route the coin's cuts to their own wallet), and its launch
    /// later (exactly as the companion's `create`). It must not exist yet (no data, the system
    /// program's): the vault, and so every slot's policy, is fixed before the coin is launched.
    pub mint: Signer<'info>,
    #[account(init, payer = payer, space = Vault::LEN, seeds = [VAULT_SEED, mint.key().as_ref()], bump)]
    pub vault: Box<Account<'info, Vault>>,
    pub system_program: Program<'info, System>,
}

/// Programs a vault's hook may not be: the protocol's (the launchpad refuses them as a custom hook
/// anyway: `PROTOCOL_PROGRAMS`), and this one.
fn protocol_program(key: &Pubkey) -> bool {
    bordrless_launch::constants::PROTOCOL_PROGRAMS.contains(key) || *key == crate::ID
}

/// The token a `SellBuyBurn` slot buys: its pool must be the DEX's, a launchpad pool (its hook the
/// launch program) quoted in bridged SOL, not the coin's own; its launch must be that pool's and
/// keep no holder rewards (the kit lets only wallets hold such a token, and the slot's owner is a
/// program address).
fn check_buy_pool(available: &[AccountInfo], pool_key: &Pubkey, mint: &Pubkey) -> Result<Launch> {
    let info = find(available, pool_key)?;
    require_keys_eq!(*info.owner, SWAP_ID, VaultError::BadPool);
    let pool = read::<Pool>(info)?;
    require!(
        pool.quote_mint == BRIDGED_SOL_MINT
            && pool.base_mint != *mint
            && pool.hook_program == Some(LAUNCH_ID),
        VaultError::BadPool
    );
    let launch_info = find(available, &launch_client::launch_address(&pool.base_mint))?;
    require_keys_eq!(*launch_info.owner, LAUNCH_ID, VaultError::BadPool);
    let launch = read::<Launch>(launch_info)?;
    require!(
        launch.pool == *pool_key && !launch.rules.rewards_on(),
        VaultError::BadPool
    );
    Ok(launch)
}

/// A `SellForSol` wallet must be able to take SOL for good: not one of the vault's own addresses
/// (the mint, the vault, its event authority, any of the `MAX_SLOTS` slot owners, used or not: PDAs
/// nobody can spend from), not a reserved key (a transaction demotes those to read-only, so every
/// payment would fail), and, as passed, not a program or an account of a loader or of the sysvar
/// program. An address with nothing on it yet is a wallet a payment can open.
fn check_wallet(
    available: &[AccountInfo],
    to: &Pubkey,
    mint: &Pubkey,
    vault: &Pubkey,
    slot_owners: &[Pubkey],
) -> Result<()> {
    require_keys_neq!(*to, Pubkey::default(), VaultError::BadTarget);
    let own = to == mint
        || to == vault
        || *to == crate::ID
        || *to == crate::EVENT_AUTHORITY_AND_BUMP.0
        || slot_owners.contains(to);
    require!(!own && !RESERVED_KEYS.contains(to), VaultError::BadWallet);
    let info = find(available, to)?;
    require!(
        !info.executable && !LOADER_OWNERS.contains(info.owner),
        VaultError::BadWallet
    );
    Ok(())
}

pub fn process_create_vault(ctx: Context<CreateVault>, args: CreateVaultArgs) -> Result<()> {
    require!(args.bounty_bps <= MAX_BOUNTY_BPS, VaultError::BountyTooHigh);
    require!(
        (1..=MAX_SELL_BPS).contains(&args.max_sell_bps),
        VaultError::BadMaxSell
    );
    require!(
        (MIN_INTERVAL..=MAX_INTERVAL).contains(&args.interval),
        VaultError::BadInterval
    );
    require!(
        args.max_hook_cut_bps <= MAX_HOOK_CUT_BPS,
        VaultError::BadHookCut
    );
    require!(!protocol_program(&args.hook), VaultError::BadHook);
    require!(
        (1..=MAX_SLOTS).contains(&args.slots.len()),
        VaultError::BadSlots
    );
    // The policy is fixed before the launch: the mint does not exist yet (audit 3a vault r1, F7).
    let mint_info = ctx.accounts.mint.to_account_info();
    require!(
        mint_info.data_is_empty() && *mint_info.owner == system_program::ID,
        VaultError::MintExists
    );
    let mint = mint_info.key();
    let vault_key = ctx.accounts.vault.key();
    // Every slot index's owner, used or not.
    let all_owners: Vec<(Pubkey, u8)> = (0..MAX_SLOTS as u8)
        .map(|i| Vault::slot_owner(&mint, i))
        .collect();
    let owner_keys: Vec<Pubkey> = all_owners.iter().map(|o| o.0).collect();
    let mut slots = [Slot::default(); MAX_SLOTS];
    for (i, a) in args.slots.iter().enumerate() {
        let mut max_cut_bps = 0;
        match a.policy {
            policy::BURN => require_keys_eq!(a.target, Pubkey::default(), VaultError::BadTarget),
            policy::SELL_FOR_SOL => check_wallet(
                ctx.remaining_accounts,
                &a.target,
                &mint,
                &vault_key,
                &owner_keys,
            )?,
            policy::SELL_BUY_BURN => {
                let x = check_buy_pool(ctx.remaining_accounts, &a.target, &mint)?;
                // One slot a pool: two would only stack their slices in one bundle (F1).
                require!(
                    !args.slots[..i]
                        .iter()
                        .any(|b| b.policy == policy::SELL_BUY_BURN && b.target == a.target),
                    VaultError::DuplicatePool
                );
                // A declared cut only for a token whose own hook can take one (F6).
                require!(
                    a.max_cut_bps <= MAX_HOOK_CUT_BPS
                        && (x.custom_hook.is_some() || a.max_cut_bps == 0),
                    VaultError::BadHookCut
                );
                max_cut_bps = a.max_cut_bps;
            }
            _ => return err!(VaultError::BadSlots),
        }
        if a.policy != policy::SELL_BUY_BURN {
            require!(a.max_cut_bps == 0, VaultError::BadHookCut);
        }
        slots[i] = Slot {
            policy: a.policy,
            target: a.target,
            owner_bump: all_owners[i].1,
            max_cut_bps,
            ..Slot::default()
        };
    }
    let owners = owner_keys[..args.slots.len()].to_vec();
    let now = Clock::get()?.unix_timestamp;
    let v = &mut ctx.accounts.vault;
    v.version = VERSION;
    v.bump = ctx.bumps.vault;
    v.mint = mint;
    v.hook = args.hook;
    v.creator = ctx.accounts.payer.key();
    v.n_slots = args.slots.len() as u8;
    v.slots = slots;
    v.bounty_bps = args.bounty_bps;
    v.max_sell_bps = args.max_sell_bps;
    v.interval = args.interval;
    v.max_hook_cut_bps = args.max_hook_cut_bps;
    v.opened = false;
    v.opened_at = 0;
    v.created_at = now;
    v.last_activity_at = now;
    v.reserved = [0; 64];
    emit_cpi!(VaultCreated {
        vault: v.key(),
        mint,
        hook: v.hook,
        creator: v.creator,
        slots: args
            .slots
            .iter()
            .map(|a| SlotPolicy {
                policy: a.policy,
                target: a.target,
                max_cut_bps: a.max_cut_bps,
            })
            .collect(),
        slot_owners: owners,
        bounty_bps: v.bounty_bps,
        max_sell_bps: v.max_sell_bps,
        interval: v.interval,
        max_hook_cut_bps: v.max_hook_cut_bps,
    });
    Ok(())
}

// ---- open_vault ----------------------------------------------------------------------------------

/// Accounts of `open_vault`. Remaining: each slot's owner and its holdings (the coin's; bridged
/// SOL's for a selling slot), the coin's pool, each `SellBuyBurn` slot's pool, the token program's
/// `create_holding` accounts.
#[event_cpi]
#[derive(Accounts)]
pub struct OpenVault<'info> {
    /// The vault's creator (`Vault::creator`, who paid for it), or anyone with the coin's mint
    /// keypair signing too; pays the holdings' rent (and each selling slot owner's).
    #[account(mut)]
    pub sender: Signer<'info>,
    #[account(mut, seeds = [VAULT_SEED, vault.mint.as_ref()], bump = vault.bump)]
    pub vault: Box<Account<'info, Vault>>,
    /// CHECK: the coin's launch (address-checked; read in the handler, which refuses it until the
    /// launchpad owns it).
    #[account(address = launch_client::launch_address(&vault.mint))]
    pub launch: UncheckedAccount<'info>,
    /// CHECK: the coin's mint (address-checked; read in the handler). It may sign (the mint's
    /// keypair, as at `create_vault`), which lets a sender other than the creator open.
    #[account(address = vault.mint)]
    pub mint: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `open_vault`: once the coin is launched with the vault's hook (its launch's `custom_hook`, and
/// its mint's hook program), creates each slot's holdings, the sender paying, and sets the
/// references to the pools' prices.
///
/// Until it runs, the hook must not name the slots: a delta to a holding that does not exist fails
/// the transfer (`InvalidDeltaAccount`), so a hook answers no cut while the slot's holding has no
/// data (as `tax_hook` waits for its collector's holding, and Half-Life for `light`).
///
/// Only the vault's creator, or a sender with the mint's keypair signing too, opens it (independent
/// audit X3): a stranger can't pick the moment the references are read. The sells' reference is the
/// coin's pool price, or its launch's opening price (the curve's start, `virtual_quote` over
/// `curve_tokens + virtual_base`) when the pool is above it: a price pumped for the open (by
/// anyone, a sandwich of the creator's own transaction included) never becomes a reference the
/// sells must wait under. On the launchpad's curve the price never falls meaningfully below the
/// opening price, so a sell's guard starts out open and tightens as sales raise the reference (5%
/// a sale at most). The launch and `open_vault` do not fit one transaction, so the site sends
/// `open_vault` in the transaction after the launch's. A buy slot's reference is the other pool's
/// price at the open (residual: a creator, or a sandwich of the creator's open, can start it low;
/// the buys then wait while it catches up, 5% an interval).
///
/// A selling slot's owner is funded to a wallet's rent-exempt minimum here: each sell unwraps its
/// SOL to the owner before paying it on, and the owner keeps that minimum for good.
pub fn process_open_vault<'info>(ctx: Context<'info, OpenVault<'info>>) -> Result<()> {
    let v = &ctx.accounts.vault;
    require!(!v.opened, VaultError::AlreadyOpen);
    require!(
        ctx.accounts.sender.key() == v.creator || ctx.accounts.mint.is_signer,
        VaultError::NotOpener
    );
    let launch_info = ctx.accounts.launch.to_account_info();
    require!(
        *launch_info.owner == LAUNCH_ID && !launch_info.data_is_empty(),
        VaultError::LaunchMissing
    );
    let launch = read::<Launch>(&launch_info)?;
    require!(launch.custom_hook == Some(v.hook), VaultError::WrongHook);
    let mint = token_client::read_mint(&ctx.accounts.mint.to_account_info())?;
    require!(mint.hook_program == Some(v.hook), VaultError::WrongHook);
    let (key, sender) = (v.mint, ctx.accounts.sender.key());
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    let opening = spot_price(
        0,
        launch.virtual_quote,
        launch.curve_tokens,
        launch.virtual_base,
    )
    .ok_or(VaultError::NoQuote)?;
    let price = pool_price(&read_pool(&all, &launch.pool)?)?.min(opening);
    let now = Clock::get()?.unix_timestamp;
    let rent_min = Rent::get()?.minimum_balance(0);
    let mut slots = v.slots;
    for (i, slot) in slots.iter_mut().enumerate().take(usize::from(v.n_slots)) {
        let seeds = SlotSeeds::new(key, i as u8, slot.owner_bump);
        let owner = seeds.address()?;
        ensure_holding(&all, sender, key, owner)?;
        if !slot.sells() {
            continue;
        }
        ensure_holding(&all, sender, BRIDGED_SOL_MINT, owner)?;
        let owner_info = find(&all, &owner)?;
        let short = rent_min.saturating_sub(owner_info.lamports());
        if short > 0 {
            system_program::transfer(
                CpiContext::new(
                    system_program::ID,
                    system_program::Transfer {
                        from: ctx.accounts.sender.to_account_info(),
                        to: owner_info.clone(),
                    },
                ),
                short,
            )?;
        }
        slot.reference_price = price;
        slot.reference_at = now;
        if slot.policy == policy::SELL_BUY_BURN {
            slot.buy_reference = pool_price(&read_pool(&all, &slot.target)?)?;
            slot.buy_reference_at = now;
        }
    }
    let v = &mut ctx.accounts.vault;
    v.slots = slots;
    v.opened = true;
    v.opened_at = now;
    v.last_activity_at = now;
    emit_cpi!(VaultOpened {
        vault: v.key(),
        mint: key,
        price,
        ts: now,
    });
    Ok(())
}
