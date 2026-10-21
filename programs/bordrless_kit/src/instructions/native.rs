//! Native kits: holder rewards held as lamports on the [`KitConfig`] itself instead of bridged SOL
//! in its reward vault holding.
//!
//! The reward accounting is the kit's own and is untouched: it only ever reads the vault's amount,
//! `seen = vault + total_claimed`. A native kit's vault amount is the config's lamports above its
//! `lamports_base` (its rent, and whatever it held when it turned native), so turning a kit native
//! (the holding's bridged SOL unwrapped onto the config, the base set to the lamports it had
//! before) leaves the vault amount, `seen`, `acc_per_share` and every holder's bytes exactly as
//! they were, and so does turning it back (`set_native(false)`: the lamports above the base
//! wrapped into the holding). Only the launchpad's kit-caller PDA for the mint may do either
//! (its admin's migration, or a native launch at creation).
//!
//! On a native kit, the launch pool's holder fees arrive as lamports on the config (the pool hook
//! names it), `claim_sol` pays a holder SOL (`claim_sol_for` the same, pushed to the holder by
//! anyone) and `share_sol` takes SOL; `claim` and `share` are refused (`NativeKit`), and on a
//! bridged kit `claim_sol`, `claim_sol_for` and `share_sol` are (`NotNativeKit`).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::{invoke, invoke_signed};
use anchor_lang::system_program;
use bordrless_hook::HOOK_AUTHORITY_SEED;
use bordrless_token::client as token_client;
use bordrless_token::state::Holding;

use crate::constants::*;
use crate::error::KitError;
use crate::events::{KitNativeSet, RewardsClaimed, RewardsShared};
use crate::instructions::callbacks::holding_amount;
use crate::legs::LegPools;
use crate::math::settle;
use crate::state::{HolderData, KitConfig};

/// The bridge's `unwrap_sol` discriminator.
pub const UNWRAP_SOL_DISCRIMINATOR: [u8; 8] = [99, 40, 14, 105, 45, 107, 172, 201];
/// The bridge's `wrap_sol` discriminator.
pub const WRAP_SOL_DISCRIMINATOR: [u8; 8] = [47, 62, 155, 172, 131, 205, 37, 201];

/// Accounts of `set_native`. The bridge's accounts are needed only when there is something to
/// move (a kit created native has an empty vault).
#[event_cpi]
#[derive(Accounts)]
pub struct SetNative<'info> {
    /// The launch program's kit-caller PDA for the config's mint (checked in the handler).
    pub kit_caller: Signer<'info>,
    /// A plain wallet that signed the transaction: wraps the rewards back on `set_native(false)`
    /// (credited them by the config first, so it ends where it started).
    #[account(mut)]
    pub wallet: Signer<'info>,
    #[account(mut)]
    pub kit_config: Account<'info, KitConfig>,
    /// CHECK: the reward vault (address-checked in the handler); the kit's id without holder
    /// rewards.
    #[account(mut)]
    pub reward_vault: Option<UncheckedAccount<'info>>,
    /// CHECK: the bridge (address-checked when used).
    pub bridge_program: Option<UncheckedAccount<'info>>,
    /// CHECK: the bridge's config (the bridge checks it).
    pub bridge_config: Option<UncheckedAccount<'info>>,
    /// CHECK: the SOL wrapper (the bridge checks it).
    #[account(mut)]
    pub sol_wrapper: Option<UncheckedAccount<'info>>,
    /// CHECK: the bridge's SOL vault (the bridge checks it).
    #[account(mut)]
    pub sol_vault: Option<UncheckedAccount<'info>>,
    /// CHECK: the reward mint, bridged SOL (address-checked when used).
    #[account(mut)]
    pub reward_mint: Option<UncheckedAccount<'info>>,
    /// CHECK: the bridge's event authority (the bridge checks it).
    pub bridge_event_authority: Option<UncheckedAccount<'info>>,
    /// CHECK: the token program.
    #[account(address = TOKEN_ID @ KitError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    #[account(address = TOKEN_EVENT_AUTHORITY @ KitError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// One call of the bridge's `wrap_sol` or `unwrap_sol` with `user` and `holding`; `trailing` is an
/// account whose lamports this program moved before the CPI (passed on so the runtime sees both
/// sides of that move; the bridge ignores it).
fn bridge_call<'info>(
    a: &SetNative<'info>,
    discriminator: [u8; 8],
    user: &AccountInfo<'info>,
    holding: &AccountInfo<'info>,
    amount: u64,
    signer_seeds: &[&[&[u8]]],
    trailing: Option<&AccountInfo<'info>>,
) -> Result<()> {
    let get = |x: &Option<UncheckedAccount<'info>>| -> Result<AccountInfo<'info>> {
        x.as_ref()
            .map(|x| x.to_account_info())
            .ok_or_else(|| error!(KitError::BridgeAccountsMissing))
    };
    let program = get(&a.bridge_program)?;
    let mint = get(&a.reward_mint)?;
    require_keys_eq!(program.key(), BRIDGE_ID, KitError::BridgeAccountsMissing);
    require_keys_eq!(
        mint.key(),
        a.kit_config.reward_mint,
        KitError::BridgeAccountsMissing
    );
    let config = get(&a.bridge_config)?;
    let wrapper = get(&a.sol_wrapper)?;
    let sol_vault = get(&a.sol_vault)?;
    let events = get(&a.bridge_event_authority)?;
    let mut data = discriminator.to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    let ro = |i: &AccountInfo| AccountMeta::new_readonly(i.key(), false);
    let rw = |i: &AccountInfo| AccountMeta::new(i.key(), false);
    let mut metas = vec![
        AccountMeta::new(user.key(), true),
        ro(&config),
        rw(&wrapper),
        rw(&sol_vault),
        rw(&mint),
        rw(holding),
        ro(&a.token_program),
        ro(&a.token_event_authority),
        ro(&a.system_program),
        ro(&events),
        ro(&program),
    ];
    let mut infos = vec![
        user.clone(),
        config,
        wrapper,
        sol_vault,
        mint,
        holding.clone(),
        a.token_program.to_account_info(),
        a.token_event_authority.to_account_info(),
        a.system_program.to_account_info(),
        events,
        program.clone(),
    ];
    if let Some(t) = trailing {
        metas.push(AccountMeta::new(t.key(), false));
        infos.push(t.clone());
    }
    let ix = anchor_lang::solana_program::instruction::Instruction {
        program_id: BRIDGE_ID,
        accounts: metas,
        data,
    };
    if signer_seeds.is_empty() {
        invoke(&ix, &infos)?;
    } else {
        invoke_signed(&ix, &infos, signer_seeds)?;
    }
    Ok(())
}

/// `set_native(native)`: idempotent both ways (see the module).
pub fn process_set_native(ctx: Context<SetNative>, native: bool) -> Result<()> {
    let config = &ctx.accounts.kit_config;
    let expected = Pubkey::create_program_address(
        &[
            KIT_CALLER_SEED,
            config.mint.as_ref(),
            &[config.kit_caller_bump],
        ],
        &LAUNCH_ID,
    )
    .map_err(|_| KitError::NotKitCaller)?;
    require_keys_eq!(
        ctx.accounts.kit_caller.key(),
        expected,
        KitError::NotKitCaller
    );
    if config.is_native() == native {
        return Ok(());
    }
    let config_info = ctx.accounts.kit_config.to_account_info();
    // The vault holding, with holder rewards.
    let vault = if config.rewards_on() {
        let vault = ctx
            .accounts
            .reward_vault
            .as_ref()
            .map(|v| v.to_account_info())
            .ok_or(KitError::MissingRewardVault)?;
        require_keys_eq!(vault.key(), config.reward_vault, KitError::WrongRewardVault);
        Some(vault)
    } else {
        None
    };
    let mint = config.mint;
    let bump = [config.bump];
    let config_seeds: &[&[u8]] = &[KIT_SEED, mint.as_ref(), &bump];
    let moved;
    if native {
        let base = config_info.lamports();
        moved = match &vault {
            Some(v) => holding_amount(v)?,
            None => 0,
        };
        if moved > 0 {
            bridge_call(
                ctx.accounts,
                UNWRAP_SOL_DISCRIMINATOR,
                &config_info,
                vault.as_ref().expect("rewards on"),
                moved,
                &[config_seeds],
                None,
            )?;
        }
        ctx.accounts.kit_config.set_native(Some(base));
    } else {
        // Without holder rewards there is no vault and nothing to wrap: any lamports above the
        // base are donations, left on the config uncounted (audit N-1: a one-lamport donation
        // must not block the rollback).
        moved = if vault.is_some() {
            ctx.accounts.kit_config.native_vault(config_info.lamports())
        } else {
            0
        };
        if moved > 0 {
            let wallet = ctx.accounts.wallet.to_account_info();
            let from = config_info.lamports();
            **config_info.try_borrow_mut_lamports()? = from - moved;
            let to = wallet.lamports();
            **wallet.try_borrow_mut_lamports()? =
                to.checked_add(moved).ok_or(KitError::MathOverflow)?;
            bridge_call(
                ctx.accounts,
                WRAP_SOL_DISCRIMINATOR,
                &wallet,
                vault.as_ref().ok_or(KitError::MissingRewardVault)?,
                moved,
                &[],
                Some(&config_info),
            )?;
        }
        ctx.accounts.kit_config.set_native(None);
    }
    let lamports_base = ctx.accounts.kit_config.lamports_base();
    emit_cpi!(KitNativeSet {
        mint,
        native,
        moved,
        lamports_base,
        ts: Clock::get()?.unix_timestamp,
    });
    Ok(())
}

/// Accounts of `claim_sol`: as `claim`'s, the owner's wallet receiving SOL instead of a holding.
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimSol<'info> {
    /// The holder: neither the pool nor the launch (checked in the handler); receives the SOL.
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(
        mut,
        constraint = kit_config.rewards_on() @ KitError::RewardsOff,
        constraint = kit_config.is_native() @ KitError::NotNativeKit
    )]
    pub kit_config: Account<'info, KitConfig>,
    /// CHECK: the config's mint (address-checked).
    #[account(address = kit_config.mint @ KitError::WrongMint)]
    pub mint: UncheckedAccount<'info>,
    /// The owner's holding of the token.
    #[account(
        mut,
        constraint = holding.mint == kit_config.mint @ KitError::WrongHolding,
        constraint = holding.owner == owner.key() @ KitError::WrongHolding
    )]
    pub holding: Account<'info, Holding>,
    /// CHECK: this program's `["hook-authority"]` PDA, which signs `write_hook_data`.
    #[account(address = HOOK_AUTHORITY @ KitError::WrongProgram)]
    pub kit_hook_authority: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = TOKEN_ID @ KitError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    #[account(address = TOKEN_EVENT_AUTHORITY @ KitError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
}

/// `claim_sol`: `claim` on a native kit, paid in SOL from the config's lamports above its base.
/// An owner whose wallet would be left below the rent-exempt minimum is refused
/// (`RecipientBelowRent`).
pub fn process_claim_sol(ctx: Context<ClaimSol>) -> Result<()> {
    let legs = claim_legs(&ctx.accounts.kit_config, ctx.remaining_accounts)?;
    let a = &mut *ctx.accounts;
    let event = pay_sol(
        &mut a.kit_config,
        &a.mint,
        &a.holding,
        &a.owner.to_account_info(),
        &a.kit_hook_authority,
        &a.token_program,
        &a.token_event_authority,
        legs.as_ref(),
    )?;
    emit_cpi!(event);
    Ok(())
}

/// Accounts of `claim_sol_for`: `claim_sol`'s, the owner a writable account that need not sign
/// and a separate `payer` signing. The SOL can only go to the holding's owner.
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimSolFor<'info> {
    /// Signs and pays the transaction fee; nothing else (it receives nothing, pays nothing).
    pub payer: Signer<'info>,
    /// CHECK: the holder, the holding's owner (constrained by `holding`): neither the pool nor the
    /// launch (checked in the handler); receives the SOL.
    #[account(mut)]
    pub owner: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = kit_config.rewards_on() @ KitError::RewardsOff,
        constraint = kit_config.is_native() @ KitError::NotNativeKit
    )]
    pub kit_config: Account<'info, KitConfig>,
    /// CHECK: the config's mint (address-checked).
    #[account(address = kit_config.mint @ KitError::WrongMint)]
    pub mint: UncheckedAccount<'info>,
    /// The owner's holding of the token.
    #[account(
        mut,
        constraint = holding.mint == kit_config.mint @ KitError::WrongHolding,
        constraint = holding.owner == owner.key() @ KitError::WrongHolding
    )]
    pub holding: Account<'info, Holding>,
    /// CHECK: this program's `["hook-authority"]` PDA, which signs `write_hook_data`.
    #[account(address = HOOK_AUTHORITY @ KitError::WrongProgram)]
    pub kit_hook_authority: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = TOKEN_ID @ KitError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    #[account(address = TOKEN_EVENT_AUTHORITY @ KitError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
}

/// `claim_sol_for`: `claim_sol` pushed by anyone, paid to the holding's owner exactly as its own
/// `claim_sol` would pay it (same checks, same bytes written, same event).
pub fn process_claim_sol_for(ctx: Context<ClaimSolFor>) -> Result<()> {
    let legs = claim_legs(&ctx.accounts.kit_config, ctx.remaining_accounts)?;
    let a = &mut *ctx.accounts;
    let event = pay_sol(
        &mut a.kit_config,
        &a.mint,
        &a.holding,
        &a.owner.to_account_info(),
        &a.kit_hook_authority,
        &a.token_program,
        &a.token_event_authority,
        legs.as_ref(),
    )?;
    emit_cpi!(event);
    Ok(())
}

/// The `Legs` a claim of a coin quoted in tokens excludes every leg's pool by
/// (`docs/token-quotes.md` §7.5.1): the first remaining account, required exactly when the config is
/// such a coin's (`LegsMissing`); `None` for every other kit, whose remaining accounts are ignored as
/// before. Without it a leg's pool, never settled (excluded by the callbacks), could be paid as if
/// it had held its coins since the first reward.
fn claim_legs(config: &KitConfig, remaining: &[AccountInfo]) -> Result<Option<LegPools>> {
    if !config.is_token_quoted() {
        return Ok(None);
    }
    let info = remaining.first().ok_or(KitError::LegsMissing)?;
    LegPools::read(info, &config.mint).map(Some)
}

/// The SOL claim of `claim_sol` and `claim_sol_for`, `owner` being the holding's owner (both
/// contexts constrain it): sync; settle the holder at its balance; pay `min(owed, vault)`
/// (`NothingToClaim` when that is 0) to `owner` from the config's lamports above its base; write
/// the holder's reward bytes, the rest as read. Answers the event to emit. A coin quoted in tokens
/// (SOL rewards, `docs/token-quotes.md` §7.5.1) also excludes every leg's pool (`legs`).
#[allow(clippy::too_many_arguments)]
fn pay_sol<'info>(
    kit_config: &mut Account<'info, KitConfig>,
    mint_account: &UncheckedAccount<'info>,
    holding: &Account<'info, Holding>,
    owner_info: &AccountInfo<'info>,
    kit_hook_authority: &UncheckedAccount<'info>,
    token_program: &UncheckedAccount<'info>,
    token_event_authority: &UncheckedAccount<'info>,
    legs: Option<&LegPools>,
) -> Result<RewardsClaimed> {
    let owner = owner_info.key();
    let now = Clock::get()?.unix_timestamp;
    let config_info = kit_config.to_account_info();
    let vault_amount = kit_config.native_vault(config_info.lamports());
    let balance = holding.amount;
    let read = holding.hook_data;

    let config = &mut **kit_config;
    require!(!config.is_excluded_with(&owner, legs), KitError::NotAHolder);
    config.sync(vault_amount, now)?;
    let mut data = HolderData::read(&read);
    settle(&mut data, balance, balance, config.acc_per_share)?;
    let pay = data.owed.min(vault_amount);
    require!(pay > 0, KitError::NothingToClaim);
    data.owed -= pay;
    config.total_claimed = config
        .total_claimed
        .checked_add(pay)
        .ok_or(KitError::MathOverflow)?;
    let total_claimed = config.total_claimed;
    let mint = config.mint;

    let rent = Rent::get()?.minimum_balance(owner_info.data_len());
    require!(
        owner_info.lamports().saturating_add(pay) >= rent,
        KitError::RecipientBelowRent
    );

    // The holder's reward bytes; the rest as read.
    let ix = token_client::write_hook_data(
        HOOK_AUTHORITY,
        mint,
        holding.key(),
        data.with_rewards_of(&read),
    );
    invoke_signed(
        &ix,
        &[
            kit_hook_authority.to_account_info(),
            mint_account.to_account_info(),
            holding.to_account_info(),
            token_event_authority.to_account_info(),
            token_program.to_account_info(),
        ],
        &[&[HOOK_AUTHORITY_SEED, &[HOOK_AUTHORITY_BUMP]]],
    )?;

    // The pay, from the config's lamports (the kit owns it), once the CPI has run.
    let from = config_info.lamports();
    **config_info.try_borrow_mut_lamports()? =
        from.checked_sub(pay).ok_or(KitError::MathOverflow)?;
    let to = owner_info.lamports();
    **owner_info.try_borrow_mut_lamports()? = to.checked_add(pay).ok_or(KitError::MathOverflow)?;

    Ok(RewardsClaimed {
        mint,
        owner,
        amount: pay,
        owed_left: data.owed,
        total_claimed,
    })
}

/// Accounts of `share_sol`.
#[event_cpi]
#[derive(Accounts)]
pub struct ShareSol<'info> {
    /// Pays the SOL.
    #[account(mut)]
    pub sharer: Signer<'info>,
    #[account(
        mut,
        constraint = kit_config.rewards_on() @ KitError::RewardsOff,
        constraint = kit_config.is_native() @ KitError::NotNativeKit
    )]
    pub kit_config: Account<'info, KitConfig>,
    pub system_program: Program<'info, System>,
}

/// `share_sol(amount)`: `share` on a native kit, the SOL sent to the config.
pub fn process_share_sol(ctx: Context<ShareSol>, amount: u64) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let config_info = ctx.accounts.kit_config.to_account_info();
    let before = ctx.accounts.kit_config.native_vault(config_info.lamports());
    {
        let config = &mut ctx.accounts.kit_config;
        require!(amount >= MIN_SHARE_LAMPORTS, KitError::ShareTooSmall);
        require!(config.divides(), KitError::NoEligibleHolders);
        config.sync(before, now)?;
    }
    system_program::transfer(
        CpiContext::new(
            ctx.accounts.system_program.key(),
            system_program::Transfer {
                from: ctx.accounts.sharer.to_account_info(),
                to: config_info.clone(),
            },
        ),
        amount,
    )?;
    let received = ctx
        .accounts
        .kit_config
        .native_vault(config_info.lamports())
        .checked_sub(before)
        .ok_or(KitError::MathOverflow)?;
    let config = &mut ctx.accounts.kit_config;
    config.add_share(received, now)?;
    let total_shared = config.total_shared;
    let mint = config.mint;
    emit_cpi!(RewardsShared {
        mint,
        from: ctx.accounts.sharer.key(),
        amount: received,
        total_shared,
    });
    Ok(())
}
