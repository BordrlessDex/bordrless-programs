//! `init`: installs the kit on a launch's mint (`docs/hooks-v2.md` §4.10). Called by the launch
//! program inside `create_launch`, signed by its kit-caller PDA for the mint.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke;
use bordrless_hook::{
    write_registry, AccountSource, ExtraAccount, HookAccountList, HOOK_ACCOUNTS_SEED,
};
use bordrless_token::client as token_client;
use bordrless_token::state::{Holding, Mint as TokenMint};

use crate::constants::*;
use crate::error::KitError;
use crate::events::KitInstalled;
use crate::state::{KitConfig, KitInitArgs};

/// Accounts of `init`.
#[event_cpi]
#[derive(Accounts)]
pub struct Init<'info> {
    /// `create_program_address(["kit-caller", mint, [args.kit_caller_bump]], LAUNCH_ID)`, signing
    /// by CPI from the launch program (checked in the handler).
    pub kit_caller: Signer<'info>,
    /// Pays the rent: the creator.
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The launch's new mint: the kit as hook with the flags of the modules, no hook authority,
    /// no mint authority, the whole supply minted (checked in the handler).
    pub mint: Account<'info, TokenMint>,
    /// The launch's holding of the mint, holding the whole supply.
    pub launch_reserve: Account<'info, Holding>,
    #[account(init, payer = payer, space = KitConfig::LEN, seeds = [KIT_SEED, mint.key().as_ref()], bump)]
    pub kit_config: Account<'info, KitConfig>,
    /// CHECK: the kit's registry for the mint, written here (an address someone already funded is
    /// accepted).
    #[account(mut, seeds = [HOOK_ACCOUNTS_SEED, mint.key().as_ref()], bump)]
    pub registry: UncheckedAccount<'info>,
    /// The launch's quote mint: no hook, no hook authority, no freeze authority.
    pub reward_mint: Account<'info, TokenMint>,
    /// CHECK: with holder rewards, `holding(reward_mint, kit_config)`, created here through the
    /// token program; absent (the kit's id) otherwise.
    #[account(mut)]
    pub reward_vault: Option<UncheckedAccount<'info>>,
    /// CHECK: the token program.
    #[account(address = TOKEN_ID @ KitError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    #[account(address = TOKEN_EVENT_AUTHORITY @ KitError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// The kit's registry for a mint: after the token program's five prefix accounts, always two
/// extras: the config (writable), then the reward vault (read-only) with holder rewards, or the
/// kit's own id (Anchor's `None` for the callbacks' optional account) without.
pub fn registry_list(kit_config: Pubkey, reward_vault: Option<Pubkey>) -> HookAccountList {
    HookAccountList::new(vec![
        ExtraAccount {
            writable: true,
            source: AccountSource::Key(kit_config),
        },
        ExtraAccount {
            writable: false,
            source: AccountSource::Key(reward_vault.unwrap_or(crate::ID)),
        },
    ])
}

/// `init`. The arguments and accounts are checked before the caller, so every refusal can be
/// told apart; nothing is written before all checks pass.
pub fn process_init(ctx: Context<Init>, args: KitInitArgs) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let mint_key = ctx.accounts.mint.key();
    let config_key = ctx.accounts.kit_config.key();
    let mint = &ctx.accounts.mint;
    let supply = mint.supply;
    let rewards = args.modules & modules::HOLDER_REWARDS != 0;

    // The parameters' hard bounds and the supply's.
    let vault_key = ctx.accounts.reward_vault.as_ref().map(|v| v.key());
    let config = KitConfig::install(
        &args,
        mint_key,
        supply,
        ctx.bumps.kit_config,
        vault_key,
        now,
    )?;

    // The mint: the kit as hook from its first instruction, nobody able to change it or mint.
    require!(
        mint.hook_program == Some(crate::ID)
            && mint.hook_authority.is_none()
            && mint.mint_authority.is_none()
            && mint.max_supply == supply
            && mint.hook_flags == mint_flags(args.modules),
        KitError::WrongMintSetup
    );
    // The whole supply in the launch's reserve, so nobody is eligible yet.
    let reserve = &ctx.accounts.launch_reserve;
    require!(
        reserve.mint == mint_key && reserve.owner == args.launch && reserve.amount == supply,
        KitError::WrongReserve
    );
    // Rewards are paid in a mint whose transfers no hook can ever join, and that nobody can
    // freeze.
    let reward_mint = &ctx.accounts.reward_mint;
    require_keys_eq!(
        reward_mint.key(),
        args.reward_mint,
        KitError::WrongRewardMint
    );
    require!(
        reward_mint.hook_program.is_none()
            && reward_mint.hook_authority.is_none()
            && reward_mint.freeze_authority.is_none(),
        KitError::BadRewardMint
    );
    if let Some(vault) = vault_key {
        require_keys_eq!(
            vault,
            token_client::holding_address(&args.reward_mint, &config_key),
            KitError::WrongRewardVault
        );
    }

    // The caller: the launch program's kit-caller PDA for this mint, at the bump it gave.
    let expected = Pubkey::create_program_address(
        &[KIT_CALLER_SEED, mint_key.as_ref(), &[args.kit_caller_bump]],
        &LAUNCH_ID,
    )
    .map_err(|_| KitError::NotKitCaller)?;
    require_keys_eq!(
        ctx.accounts.kit_caller.key(),
        expected,
        KitError::NotKitCaller
    );

    // The reward vault: the config's holding of the reward mint.
    if rewards {
        let vault = ctx
            .accounts
            .reward_vault
            .as_ref()
            .ok_or(KitError::MissingRewardVault)?;
        let ix =
            token_client::create_holding(ctx.accounts.payer.key(), args.reward_mint, config_key);
        invoke(
            &ix,
            &[
                ctx.accounts.payer.to_account_info(),
                ctx.accounts.reward_mint.to_account_info(),
                ctx.accounts.kit_config.to_account_info(),
                vault.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
                ctx.accounts.token_event_authority.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
            ],
        )?;
    }

    // The registry clients resolve the callbacks' extra accounts from.
    write_registry(
        &ctx.accounts.payer.to_account_info(),
        &ctx.accounts.registry.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        &crate::ID,
        &mint_key,
        ctx.bumps.registry,
        &registry_list(config_key, vault_key),
    )?;

    emit_cpi!(KitInstalled {
        mint: mint_key,
        kit_config: config_key,
        launch: config.launch,
        pool: config.pool,
        creator: config.creator,
        reward_mint: config.reward_mint,
        reward_vault: config.reward_vault,
        modules: config.modules,
        supply,
        min_eligible: config.min_eligible,
        max_wallet_bps: config.max_wallet_bps,
        max_wallet_amount: config.max_wallet_amount,
        creator_unlock_at: config.creator_unlock_at,
        early_window_end: config.early_window_end,
        early_unlock_at: config.early_unlock_at,
        ts: now,
    });
    ctx.accounts.kit_config.set_inner(config);
    Ok(())
}
