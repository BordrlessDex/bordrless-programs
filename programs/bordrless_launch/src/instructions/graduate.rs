//! Graduation (`docs/hooks-v2.md` §5.5): the reserve tops the pool up so the price is continuous,
//! the DEX finalizes the curve and mints the LP to the launch, the rest of the reserve is burned,
//! and for a launch with a kit the kit is told (which lifts max wallet). The top-up moves between
//! excluded holdings and the burn is from one, so the kit's accounting does not change. A launch
//! with a custom hook (§5.8) passes the hook's slice as remaining accounts for the same two
//! token instructions.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use bordrless_core::graduation_topup;
use bordrless_hook::HOOK_AUTHORITY_SEED;
use bordrless_swap::client as swap_client;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::cpi;
use crate::error::LaunchError;
use crate::events::Graduated;
use crate::instructions::launch::{KitCallerSeeds, LaunchSeeds};
use crate::state::*;

/// Accounts of `graduate`.
#[event_cpi]
#[derive(Accounts)]
pub struct Graduate<'info> {
    /// Whoever cranks it; pays only the network fee.
    pub cranker: Signer<'info>,
    /// The launch (only `create_launch` creates one, at its PDA).
    #[account(mut, constraint = launch.status == STATUS_CURVE @ LaunchError::AlreadyGraduated)]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: address-checked.
    #[account(mut, address = launch.mint @ LaunchError::WrongHolding)]
    pub mint: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(address = launch.quote_mint @ LaunchError::WrongHolding)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = launch.pool @ LaunchError::WrongPool)]
    pub pool: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = launch.reserve_holding @ LaunchError::WrongHolding)]
    pub launch_reserve: UncheckedAccount<'info>,
    /// CHECK: the pool's base vault (checked against the pool in the handler).
    #[account(mut)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: the pool's quote vault (checked against the pool in the handler).
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: the pool's LP mint (checked against the pool in the handler).
    #[account(mut)]
    pub lp_mint: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = launch.lp_holding @ LaunchError::WrongHolding)]
    pub launch_lp: UncheckedAccount<'info>,
    /// CHECK: this program's hook authority (address-checked).
    #[account(address = LAUNCH_HOOK_AUTHORITY @ LaunchError::WrongProgram)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: the DEX.
    #[account(address = bordrless_swap::ID @ LaunchError::WrongProgram)]
    pub swap_program: UncheckedAccount<'info>,
    /// CHECK: the DEX's event authority (address-checked).
    #[account(address = DEX_EVENT_AUTHORITY @ LaunchError::WrongProgram)]
    pub dex_event_authority: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = bordrless_token::ID @ LaunchError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's signer of the kit's callbacks (address-checked), which the
    /// reserve's transfer and burn take as their hook signer; unused without a kit.
    #[account(address = TOKEN_HOOK_AUTHORITY @ LaunchError::WrongProgram)]
    pub token_hook_signer: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority (address-checked).
    #[account(address = TOKEN_EVENT_AUTHORITY @ LaunchError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
    /// CHECK: the kit; required exactly for a launch with a kit (`KitAccountsMissing`), absent
    /// (this program's id) otherwise, as are the kit accounts below. Checked in the handler.
    pub kit_program: Option<UncheckedAccount<'info>>,
    /// CHECK: `launch.kit_config`.
    #[account(mut)]
    pub kit_config: Option<UncheckedAccount<'info>>,
    /// CHECK: `launch.holder_vault` with holder rewards, else the kit's id (the kit hook's
    /// extras on the reserve's transfer and burn).
    pub reward_vault: Option<UncheckedAccount<'info>>,
    /// CHECK: `PDA(["kit-caller", mint], LAUNCH_ID)` at `launch.kit_caller_bump`, which signs the
    /// kit's `graduate`.
    pub kit_caller: Option<UncheckedAccount<'info>>,
    /// CHECK: the kit's event authority.
    pub kit_event_authority: Option<UncheckedAccount<'info>>,
}

/// The kit's accounts at graduation, checked.
struct GraduateKit<'info> {
    program: AccountInfo<'info>,
    config: AccountInfo<'info>,
    vault: AccountInfo<'info>,
    caller: AccountInfo<'info>,
    events: AccountInfo<'info>,
}

fn required<'info>(account: &Option<UncheckedAccount<'info>>) -> Result<AccountInfo<'info>> {
    account
        .as_ref()
        .map(|a| a.to_account_info())
        .ok_or_else(|| error!(LaunchError::KitAccountsMissing))
}

/// The kit's accounts: required, each at its address, exactly for a launch with a kit.
fn kit_accounts<'info>(a: &Graduate<'info>) -> Result<Option<GraduateKit<'info>>> {
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
        return Ok(None);
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
    let expected_caller = Pubkey::create_program_address(
        &[
            KIT_CALLER_SEED,
            launch.mint.as_ref(),
            &[launch.kit_caller_bump],
        ],
        &crate::ID,
    )
    .map_err(|_| LaunchError::WrongKitAccount)?;
    require_keys_eq!(caller.key(), expected_caller, LaunchError::WrongKitAccount);
    let events = required(&a.kit_event_authority)?;
    require_keys_eq!(
        events.key(),
        KIT_EVENT_AUTHORITY,
        LaunchError::WrongKitAccount
    );
    Ok(Some(GraduateKit {
        program,
        config,
        vault,
        caller,
        events,
    }))
}

/// The custom hook's accounts at graduation (§5.8), from the remaining accounts: the hook program
/// (the launch's), the token program's signer for it (the token program checks it) and the
/// registry's extras; required exactly for a launch with a custom hook.
fn custom_hook_accounts<'info>(
    launch: &Launch,
    remaining: &[AccountInfo<'info>],
) -> Result<
    Option<(
        AccountInfo<'info>,
        AccountInfo<'info>,
        Vec<AccountInfo<'info>>,
    )>,
> {
    let Some(hook) = launch.custom_hook else {
        require!(
            remaining.is_empty(),
            LaunchError::UnexpectedCustomHookAccounts
        );
        return Ok(None);
    };
    require!(remaining.len() >= 2, LaunchError::CustomHookAccountsMissing);
    require_keys_eq!(*remaining[0].key, hook, LaunchError::WrongProgram);
    Ok(Some((
        remaining[0].clone(),
        remaining[1].clone(),
        remaining[2..].to_vec(),
    )))
}

/// `graduate`.
pub fn process_graduate<'info>(ctx: Context<'info, Graduate<'info>>) -> Result<()> {
    let clock = Clock::get()?;
    let launch_key = ctx.accounts.launch.key();
    let mint = ctx.accounts.launch.mint;
    let pool_key = ctx.accounts.launch.pool;
    // The pool is the launch's (address-checked) and the DEX's (owner-checked), so its vaults
    // and LP mint are what it says.
    let pool = swap_client::read_pool(&ctx.accounts.pool)?;
    require_keys_eq!(
        ctx.accounts.base_vault.key(),
        pool.base_vault,
        LaunchError::WrongHolding
    );
    require_keys_eq!(
        ctx.accounts.quote_vault.key(),
        pool.quote_vault,
        LaunchError::WrongHolding
    );
    require_keys_eq!(
        ctx.accounts.lp_mint.key(),
        pool.lp_mint,
        LaunchError::WrongHolding
    );
    require!(pool.curve, LaunchError::AlreadyGraduated);
    // Ready when the pool has raised the threshold, or when its curve has sold out: a launch
    // pool's LP fee is Bordrless's and does not compound, so the reserve reaches the threshold
    // exactly as the last curve token sells, and a hook's cut on buys (whose share leaves the
    // reserve) can keep it just below for good.
    require!(
        pool.quote_reserve >= ctx.accounts.launch.graduation_quote || pool.base_reserve == 0,
        LaunchError::NotReady
    );
    let kit = kit_accounts(ctx.accounts)?;
    let custom = custom_hook_accounts(&ctx.accounts.launch, ctx.remaining_accounts)?;

    let launch_info = ctx.accounts.launch.to_account_info();
    let launch_seeds = LaunchSeeds::new(mint, ctx.accounts.launch.bump);
    let seeds = launch_seeds.seeds();
    let token_program = ctx.accounts.token_program.to_account_info();
    let token_events = ctx.accounts.token_event_authority.to_account_info();
    let reserve_info = ctx.accounts.launch_reserve.to_account_info();
    let mint_info = ctx.accounts.mint.to_account_info();
    // The mint's hook on the reserve's transfer and burn: the kit, the token program's signer
    // for it and the kit's extras; or the custom hook, its signer and its extras; or the token
    // program's id for both hook slots without a hook.
    let (hook, hook_info, signer_info, extras, extra_infos) = match (&kit, &custom) {
        (Some(k), _) => (
            Some(cpi::KIT_HOOK),
            k.program.clone(),
            ctx.accounts.token_hook_signer.to_account_info(),
            cpi::kit_extras(k.config.key(), Some(k.vault.key())).to_vec(),
            vec![k.config.clone(), k.vault.clone()],
        ),
        (None, Some((program, signer, hook_extras))) => (
            Some((*program.key, *signer.key)),
            program.clone(),
            signer.clone(),
            hook_extras
                .iter()
                .map(|a| AccountMeta {
                    pubkey: *a.key,
                    is_signer: false,
                    is_writable: a.is_writable,
                })
                .collect(),
            hook_extras.clone(),
        ),
        (None, None) => (
            None,
            token_program.clone(),
            token_program.clone(),
            vec![],
            vec![],
        ),
    };

    // 1. Top the pool up from the reserve so the price is continuous without the virtual offsets.
    let reserve = token_client::read_holding(&ctx.accounts.launch_reserve)?.amount;
    let topup = graduation_topup(
        pool.base_reserve,
        pool.quote_reserve,
        pool.virtual_base,
        pool.virtual_quote,
    )
    .ok_or(LaunchError::MathOverflow)?
    .min(reserve);
    if topup > 0 {
        let ix = cpi::token_transfer(
            launch_key,
            reserve_info.key(),
            ctx.accounts.base_vault.key(),
            mint,
            hook,
            &extras,
            topup,
        );
        let mut infos = vec![
            launch_info.clone(),
            reserve_info.clone(),
            ctx.accounts.base_vault.to_account_info(),
            mint_info.clone(),
            hook_info.clone(),
            signer_info.clone(),
            token_events.clone(),
            token_program.clone(),
        ];
        infos.extend(extra_infos.iter().cloned());
        invoke_signed(&ix, &infos, &[&seeds])?;
    }

    // 2. The DEX finalizes the curve and mints the LP to the launch, where it stays.
    let hook_seeds: &[&[u8]] = &[HOOK_AUTHORITY_SEED, &[HOOK_AUTHORITY_BUMP]];
    let ix = cpi::dex_finalize_curve(
        pool_key,
        ctx.accounts.base_vault.key(),
        ctx.accounts.quote_vault.key(),
        ctx.accounts.lp_mint.key(),
        ctx.accounts.launch_lp.key(),
    );
    invoke_signed(
        &ix,
        &[
            ctx.accounts.hook_signer.to_account_info(),
            ctx.accounts.pool.to_account_info(),
            ctx.accounts.base_vault.to_account_info(),
            ctx.accounts.quote_vault.to_account_info(),
            ctx.accounts.lp_mint.to_account_info(),
            ctx.accounts.launch_lp.to_account_info(),
            token_program.clone(),
            token_events.clone(),
            ctx.accounts.dex_event_authority.to_account_info(),
            ctx.accounts.swap_program.to_account_info(),
        ],
        &[hook_seeds],
    )?;

    // 3. The rest of the reserve is burned.
    let burned = reserve - topup;
    if burned > 0 {
        let ix = cpi::token_burn(launch_key, reserve_info.key(), mint, hook, &extras, burned);
        let mut infos = vec![
            launch_info.clone(),
            reserve_info.clone(),
            mint_info.clone(),
            hook_info.clone(),
            signer_info.clone(),
            token_events.clone(),
            token_program.clone(),
        ];
        infos.extend(extra_infos.iter().cloned());
        invoke_signed(&ix, &infos, &[&seeds])?;
    }

    // 4. A launch with a kit always tells it, signed by the mint's kit-caller PDA: max wallet
    //    lifts.
    if let Some(k) = &kit {
        let ix = cpi::kit_graduate(k.caller.key(), k.config.key());
        let caller_seeds = KitCallerSeeds::new(mint, ctx.accounts.launch.kit_caller_bump);
        invoke_signed(
            &ix,
            &[
                k.caller.clone(),
                k.config.clone(),
                k.events.clone(),
                k.program.clone(),
            ],
            &[&caller_seeds.seeds()],
        )?;
    }

    let pool_after = swap_client::read_pool(&ctx.accounts.pool)?;
    let lp_minted = token_client::read_holding(&ctx.accounts.launch_lp)?.amount;
    let supply = token_client::read_mint(&ctx.accounts.mint)?.supply;
    let launch = &mut ctx.accounts.launch;
    launch.status = STATUS_GRADUATED;
    launch.graduated_at = clock.unix_timestamp;
    launch.graduation_topup = topup;
    launch.graduation_burned = burned;
    emit_cpi!(Graduated {
        launch: launch_key,
        mint,
        pool: pool_key,
        cranker: ctx.accounts.cranker.key(),
        topup,
        burned,
        base_reserve: pool_after.base_reserve,
        quote_reserve: pool_after.quote_reserve,
        lp_minted,
        supply,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}
