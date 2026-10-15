//! Native launches: a launch whose pool holds SOL (`bordrless_swap::native`) and whose fees are
//! lamports. The pool hook pays a native launch's creator fees (and any custom hook's SOL cut,
//! which joins them) to the launch account itself and its holder fees to the kit config, as
//! lamports; the `_sol` claims pay them out of the launch's lamports above its rent-exempt
//! minimum; the kit's `claim_sol` pays holders. `creator_fees_accrued`,
//! `creator_fees_claimed`, `author_fees_paid`, the partner's `paid_total` and `quote_cut_accrued`
//! keep their meaning and their place: what was taken, and what was paid.
//!
//! New launches are native when the admin turned it on (`set_native_launches`), a companion coin's
//! too (its companion turns native at its launch). `migrate_launch_to_native` turns a live launch
//! native: only the admin, only while the DEX is paused, only once its pool is native
//! (`migrate_pool_to_native`; a companion coin's companion follows it); its
//! creator-fee holding is unwrapped onto the launch account and its kit's reward vault onto the kit
//! config (the kit's accounting untouched). `revert_launch_to_bridged` undoes it (revert the launch
//! before its pool). Both are idempotent. The old claims keep paying whatever sits in the holding.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use bordrless_swap::native::BridgeSol;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::cpi;
use crate::error::LaunchError;
use crate::events::{LaunchNativeSet, NativeLaunchesSet};
use crate::instructions::launch::{KitCallerSeeds, LaunchSeeds};
use crate::state::*;

/// Accounts of `set_native_launches`.
#[event_cpi]
#[derive(Accounts)]
pub struct SetNativeLaunches<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ LaunchError::NotAdmin)]
    pub config: Account<'info, Config>,
}

/// `set_native_launches(on)`.
pub fn process_set_native_launches(ctx: Context<SetNativeLaunches>, on: bool) -> Result<()> {
    ctx.accounts.config.set_native_launches(on);
    emit_cpi!(NativeLaunchesSet {
        on,
        ts: Clock::get()?.unix_timestamp,
    });
    Ok(())
}

/// Accounts of `migrate_launch_to_native` and `revert_launch_to_bridged`. The kit's accounts are
/// required exactly for a launch with a kit.
#[event_cpi]
#[derive(Accounts)]
pub struct LaunchForm<'info> {
    /// The admin; a plain wallet (it wraps the launch's lamports on the way back, credited them
    /// first, so it ends where it started).
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(address = CONFIG_ADDRESS, has_one = admin @ LaunchError::NotAdmin)]
    pub config: Box<Account<'info, Config>>,
    /// CHECK: the DEX config (address-checked), read for its pause.
    #[account(address = DEX_CONFIG @ LaunchError::WrongProgram)]
    pub dex_config: UncheckedAccount<'info>,
    #[account(mut, seeds = [LAUNCH_SEED, launch.mint.as_ref()], bump = launch.bump)]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: address-checked: the launch's pool (read).
    #[account(address = launch.pool @ LaunchError::WrongPool)]
    pub pool: UncheckedAccount<'info>,
    /// CHECK: address-checked: the launch's holding of bridged SOL.
    #[account(mut, address = launch.quote_holding @ LaunchError::WrongHolding)]
    pub launch_quote: UncheckedAccount<'info>,
    /// CHECK: address-checked: bridged SOL.
    #[account(mut, address = launch.quote_mint @ LaunchError::WrongHolding)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: address-checked: the bridge.
    #[account(address = BRIDGE_ID @ LaunchError::WrongProgram)]
    pub bridge_program: UncheckedAccount<'info>,
    /// CHECK: the bridge's config (the bridge checks it).
    pub bridge_config: UncheckedAccount<'info>,
    /// CHECK: the SOL wrapper (the bridge checks it).
    #[account(mut)]
    pub sol_wrapper: UncheckedAccount<'info>,
    /// CHECK: the bridge's SOL vault (the bridge checks it).
    #[account(mut)]
    pub sol_vault: UncheckedAccount<'info>,
    /// CHECK: the bridge's event authority (the bridge checks it).
    pub bridge_event_authority: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = bordrless_token::ID @ LaunchError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    #[account(address = TOKEN_EVENT_AUTHORITY @ LaunchError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    /// CHECK: the kit (address-checked in the handler); this program's id without a kit, as are
    /// the kit accounts below.
    pub kit_program: Option<UncheckedAccount<'info>>,
    /// CHECK: `launch.kit_config`.
    #[account(mut)]
    pub kit_config: Option<UncheckedAccount<'info>>,
    /// CHECK: `launch.holder_vault` with holder rewards, else the kit's id, read-only (no `mut`, as in `graduate`).
    pub reward_vault: Option<UncheckedAccount<'info>>,
    /// CHECK: the launch's kit-caller PDA.
    pub kit_caller: Option<UncheckedAccount<'info>>,
    /// CHECK: the kit's event authority.
    pub kit_event_authority: Option<UncheckedAccount<'info>>,
}

fn bridge<'info>(a: &LaunchForm<'info>) -> BridgeSol<'info> {
    BridgeSol {
        program: a.bridge_program.to_account_info(),
        config: a.bridge_config.to_account_info(),
        wrapper: a.sol_wrapper.to_account_info(),
        sol_vault: a.sol_vault.to_account_info(),
        wrapped_mint: a.quote_mint.to_account_info(),
        event_authority: a.bridge_event_authority.to_account_info(),
        token_program: a.token_program.to_account_info(),
        token_event_authority: a.token_event_authority.to_account_info(),
        system_program: a.system_program.to_account_info(),
    }
}

/// The checks both share: the DEX paused. A companion coin's companion migrates right after its
/// launch (`migrate_companion_to_native`) and reverts right before it.
fn check_form(a: &LaunchForm) -> Result<()> {
    let data = a.dex_config.try_borrow_data()?;
    let dex = bordrless_swap::state::Config::try_deserialize(&mut &data[..])?;
    require!(dex.paused, LaunchError::DexNotPaused);
    Ok(())
}

fn required<'info>(x: &Option<UncheckedAccount<'info>>) -> Result<AccountInfo<'info>> {
    x.as_ref()
        .map(|x| x.to_account_info())
        .ok_or_else(|| error!(LaunchError::KitAccountsMissing))
}

/// The kit's `set_native(native)`, for a launch with a kit (signed by its kit-caller PDA, the
/// admin wrapping on the way back).
fn kit_set_native<'info>(a: &LaunchForm<'info>, native: bool) -> Result<()> {
    let launch = &a.launch;
    if !launch.has_kit() {
        require!(
            a.kit_program.is_none()
                && a.kit_config.is_none()
                && a.reward_vault.is_none()
                && a.kit_caller.is_none()
                && a.kit_event_authority.is_none(),
            LaunchError::UnexpectedKitAccounts
        );
        return Ok(());
    }
    let program = required(&a.kit_program)?;
    require_keys_eq!(program.key(), KIT_ID, LaunchError::WrongKitAccount);
    let config = required(&a.kit_config)?;
    require_keys_eq!(
        config.key(),
        launch.kit_config,
        LaunchError::WrongKitAccount
    );
    let vault = required(&a.reward_vault)?;
    let expected_vault = if launch.rewards_on() {
        launch.holder_vault
    } else {
        KIT_ID
    };
    require_keys_eq!(vault.key(), expected_vault, LaunchError::WrongKitAccount);
    let caller = required(&a.kit_caller)?;
    let caller_seeds = KitCallerSeeds::new(launch.mint, launch.kit_caller_bump);
    let events = required(&a.kit_event_authority)?;
    require_keys_eq!(
        events.key(),
        KIT_EVENT_AUTHORITY,
        LaunchError::WrongKitAccount
    );
    let bridge_metas = [
        AccountMeta::new_readonly(a.bridge_program.key(), false),
        AccountMeta::new_readonly(a.bridge_config.key(), false),
        AccountMeta::new(a.sol_wrapper.key(), false),
        AccountMeta::new(a.sol_vault.key(), false),
        AccountMeta::new(a.quote_mint.key(), false),
        AccountMeta::new_readonly(a.bridge_event_authority.key(), false),
    ];
    let ix = cpi::kit_set_native(
        caller.key(),
        a.admin.key(),
        config.key(),
        launch.rewards_on().then_some(vault.key()),
        Some(bridge_metas),
        native,
    );
    invoke_signed(
        &ix,
        &[
            caller,
            a.admin.to_account_info(),
            config,
            vault,
            a.bridge_program.to_account_info(),
            a.bridge_config.to_account_info(),
            a.sol_wrapper.to_account_info(),
            a.sol_vault.to_account_info(),
            a.quote_mint.to_account_info(),
            a.bridge_event_authority.to_account_info(),
            a.token_program.to_account_info(),
            a.token_event_authority.to_account_info(),
            a.system_program.to_account_info(),
            events,
            program,
        ],
        &[&caller_seeds.seeds()],
    )?;
    Ok(())
}

/// `migrate_launch_to_native`.
pub fn process_migrate_launch_to_native(ctx: Context<LaunchForm>) -> Result<()> {
    check_form(ctx.accounts)?;
    if ctx.accounts.launch.is_native() {
        return Ok(());
    }
    let pool = bordrless_swap::client::read_pool(&ctx.accounts.pool)?;
    require!(pool.quote_native(), LaunchError::PoolNotNative);
    // The creator fees in the holding, unwrapped onto the launch (the launch the bridge's user).
    let moved = token_client::read_holding(&ctx.accounts.launch_quote)?.amount;
    let launch_info = ctx.accounts.launch.to_account_info();
    let launch_seeds = LaunchSeeds::new(ctx.accounts.launch.mint, ctx.accounts.launch.bump);
    bridge(ctx.accounts).unwrap_to_pool(
        &launch_info,
        &ctx.accounts.launch_quote.to_account_info(),
        moved,
        &launch_seeds.seeds(),
    )?;
    kit_set_native(ctx.accounts, true)?;
    let launch = &mut ctx.accounts.launch;
    launch.set_native(true);
    emit_cpi!(LaunchNativeSet {
        launch: launch.key(),
        mint: launch.mint,
        native: true,
        moved,
        ts: Clock::get()?.unix_timestamp,
    });
    Ok(())
}

/// `revert_launch_to_bridged`.
pub fn process_revert_launch_to_bridged(ctx: Context<LaunchForm>) -> Result<()> {
    check_form(ctx.accounts)?;
    if !ctx.accounts.launch.is_native() {
        return Ok(());
    }
    // The launch's lamports above its rent, wrapped back into its holding by the admin's wallet.
    let launch_info = ctx.accounts.launch.to_account_info();
    let rent = Rent::get()?.minimum_balance(launch_info.data_len());
    let moved = launch_info.lamports().saturating_sub(rent);
    let b = bridge(ctx.accounts);
    b.check()?;
    b.wrap_from_pool(
        &launch_info,
        &ctx.accounts.admin.to_account_info(),
        &ctx.accounts.launch_quote.to_account_info(),
        moved,
    )?;
    kit_set_native(ctx.accounts, false)?;
    let launch = &mut ctx.accounts.launch;
    launch.set_native(false);
    emit_cpi!(LaunchNativeSet {
        launch: launch.key(),
        mint: launch.mint,
        native: false,
        moved,
        ts: Clock::get()?.unix_timestamp,
    });
    Ok(())
}
