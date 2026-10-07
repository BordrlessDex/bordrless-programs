//! Creating a launch (`docs/hooks-v2.md` §5.3, §5.7, §5.8): the mint (with the kit as its token
//! hook when the rules install a kit module, with the creator's own hook when a `LaunchConfig`
//! names one, else no hook), the supply to the launch, the kit, the curve pool, the pool hook's
//! registry and the `Launch`. The rules come inline (presets and the mix-and-match custom) or
//! from a `LaunchConfig` made with the SDK (then the inline rules must match it).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::system_program;
use bordrless_core::{curve_params, max_wallet_cap, CurveParams};
use bordrless_hook::{
    hook_accounts_address, write_registry, AccountSource, ExtraAccount, HookAccountList, Seed,
    HOOK_AUTHORITY_SEED,
};
use bordrless_kit::KitInitArgs;
use bordrless_swap::instructions::CreatePoolArgs;
use bordrless_token::client as token_client;
use bordrless_token::instructions::CreateMintArgs;
use bordrless_token::state::AuthorityKind;

use crate::constants::*;
use crate::cpi;
use crate::error::LaunchError;
use crate::events::LaunchCreated;
use crate::state::*;

/// Arguments of `create_launch`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct CreateLaunchArgs {
    /// Name.
    pub name: String,
    /// Symbol.
    pub symbol: String,
    /// Metadata URI.
    pub uri: String,
    /// Creator fee in basis points of the quote, at most the config's maximum. With a
    /// `LaunchConfig` it must equal the config's.
    pub creator_fee_bps: u16,
    /// Virtual quote reserve the curve opens with (sets the opening market cap), within the
    /// config's bounds.
    pub virtual_quote: u64,
    /// The token rules, within the config's bounds; fixed at launch. With a `LaunchConfig` they
    /// must equal the config's.
    pub rules: LaunchRules,
}

/// Accounts of `create_launch`. With a custom hook (a `LaunchConfig` that names one), the
/// remaining accounts are the hook program, the token program's signer for it, the hook's
/// registry for the mint (`["bordrless-hook-accounts", mint]` under the hook) and the registry's
/// extras, resolved by the client; there are none otherwise.
#[event_cpi]
#[derive(Accounts)]
pub struct CreateLaunch<'info> {
    /// The creator: pays the launch fee and the rent.
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(mut, address = CONFIG_ADDRESS)]
    pub config: Box<Account<'info, Config>>,
    /// CHECK: receives the launch fee; also the DEX config's treasury (address-checked).
    #[account(mut, address = config.treasury @ LaunchError::WrongHolding)]
    pub treasury: UncheckedAccount<'info>,
    /// CHECK: the new mint; signs, created through the token program.
    #[account(mut)]
    pub mint: Signer<'info>,
    #[account(init, payer = creator, space = Launch::LEN, seeds = [LAUNCH_SEED, mint.key().as_ref()], bump)]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: the launch's holding of the token, created here (the token program checks its
    /// address).
    #[account(mut)]
    pub launch_reserve: UncheckedAccount<'info>,
    /// CHECK: the launch's holding of the quote (creator fees), created here.
    #[account(mut)]
    pub launch_quote: UncheckedAccount<'info>,
    /// CHECK: the config's quote mint (address-checked).
    #[account(address = config.quote_mint @ LaunchError::WrongHolding)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: this hook's extra-accounts registry for the pool, created here (address checked
    /// when written).
    #[account(mut)]
    pub registry: UncheckedAccount<'info>,
    /// CHECK: the DEX config (address-checked).
    #[account(mut, address = DEX_CONFIG @ LaunchError::WrongProgram)]
    pub dex_config: UncheckedAccount<'info>,
    /// CHECK: the pool PDA (the DEX checks its seeds: this mint, the quote, the config's LP fee,
    /// this program as hook).
    #[account(mut)]
    pub pool: UncheckedAccount<'info>,
    /// CHECK: the pool's LP mint PDA (checked by the DEX).
    #[account(mut)]
    pub lp_mint: UncheckedAccount<'info>,
    /// CHECK: the pool's base vault (created by the DEX through the token program).
    #[account(mut)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: the pool's quote vault.
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: the launch's holding of the LP mint, created by the DEX.
    #[account(mut)]
    pub launch_lp: UncheckedAccount<'info>,
    /// CHECK: this program's hook authority (address-checked).
    #[account(address = LAUNCH_HOOK_AUTHORITY @ LaunchError::WrongProgram)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: the DEX's signer of this program's pool callbacks (address-checked), which the
    /// pool's creation takes as its hook signer.
    #[account(address = DEX_HOOK_AUTHORITY @ LaunchError::WrongProgram)]
    pub dex_hook_signer: UncheckedAccount<'info>,
    /// CHECK: the DEX's event authority (address-checked).
    #[account(address = DEX_EVENT_AUTHORITY @ LaunchError::WrongProgram)]
    pub dex_event_authority: UncheckedAccount<'info>,
    /// CHECK: the DEX.
    #[account(address = bordrless_swap::ID @ LaunchError::WrongProgram)]
    pub swap_program: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = bordrless_token::ID @ LaunchError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's signer of the kit's callbacks (address-checked), which the
    /// token instructions of a kit mint take as their hook signer; unused without a kit.
    #[account(address = TOKEN_HOOK_AUTHORITY @ LaunchError::WrongProgram)]
    pub token_hook_signer: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority (address-checked).
    #[account(address = TOKEN_EVENT_AUTHORITY @ LaunchError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
    /// CHECK: the kit (`KIT_ID`); present exactly when the rules install a kit module (this
    /// program's id stands for an absent one; the kit accounts below likewise). Checked in the
    /// handler.
    pub kit_program: Option<UncheckedAccount<'info>>,
    /// CHECK: `PDA(["kit", mint], KIT_ID)`, created by the kit.
    #[account(mut)]
    pub kit_config: Option<UncheckedAccount<'info>>,
    /// CHECK: `PDA(["bordrless-hook-accounts", mint], KIT_ID)`, written by the kit (which checks
    /// its seeds).
    #[account(mut)]
    pub kit_registry: Option<UncheckedAccount<'info>>,
    /// CHECK: `holding(quote_mint, kit_config)`, the reward vault the kit creates; with holder
    /// rewards only.
    #[account(mut)]
    pub reward_vault: Option<UncheckedAccount<'info>>,
    /// CHECK: `PDA(["kit-caller", mint], LAUNCH_ID)`, which signs the kit's `init`.
    pub kit_caller: Option<UncheckedAccount<'info>>,
    /// CHECK: the kit's event authority.
    pub kit_event_authority: Option<UncheckedAccount<'info>>,
    /// The `LaunchConfig` the launch is made from, when it is (this program's id for none): the
    /// rules, the creator fee and the custom hook come from it, and the arguments must match.
    pub launch_config: Option<Box<Account<'info, LaunchConfig>>>,
}

/// Signer seeds of a launch PDA.
pub struct LaunchSeeds {
    mint: Pubkey,
    bump: [u8; 1],
}

impl LaunchSeeds {
    /// The seeds of the launch of `mint`.
    pub fn new(mint: Pubkey, bump: u8) -> Self {
        Self { mint, bump: [bump] }
    }

    /// As signer seeds.
    pub fn seeds(&self) -> [&[u8]; 3] {
        [LAUNCH_SEED, self.mint.as_ref(), &self.bump]
    }
}

/// Signer seeds of the kit-caller PDA of a mint.
pub struct KitCallerSeeds {
    mint: Pubkey,
    bump: [u8; 1],
}

impl KitCallerSeeds {
    /// The seeds of the kit caller of `mint`.
    pub fn new(mint: Pubkey, bump: u8) -> Self {
        Self { mint, bump: [bump] }
    }

    /// As signer seeds.
    pub fn seeds(&self) -> [&[u8]; 3] {
        [KIT_CALLER_SEED, self.mint.as_ref(), &self.bump]
    }
}

/// The extra accounts a launch pool's callbacks take (§5.4), at indices 5 to 8: the launch, its
/// quote holding (creator fees), the holder vault (holder fees) and the kit config (the
/// holder-fee threshold). The last two are fixed keys, written whatever the modules.
pub fn registry_list(holder_vault: Pubkey, kit_config: Pubkey) -> HookAccountList {
    HookAccountList::new(vec![
        ExtraAccount {
            writable: true,
            source: AccountSource::Pda {
                program: crate::ID,
                seeds: vec![Seed::Literal(LAUNCH_SEED.to_vec()), Seed::Account(2)],
            },
        },
        ExtraAccount {
            writable: true,
            source: AccountSource::Pda {
                program: bordrless_token::ID,
                seeds: vec![
                    Seed::Literal(bordrless_token::constants::HOLDING_SEED.to_vec()),
                    Seed::Account(3),
                    Seed::Account(5),
                ],
            },
        },
        ExtraAccount {
            writable: true,
            source: AccountSource::Key(holder_vault),
        },
        ExtraAccount {
            writable: false,
            source: AccountSource::Key(kit_config),
        },
    ])
}

/// Why `create_launch` refuses `rules` with `creator_fee_bps` under `config` (§5.2), checked in
/// the order the launch form checks them.
pub fn check_rules(rules: &LaunchRules, creator_fee_bps: u16, config: &Config) -> Result<()> {
    let b = &config.rule_bounds;
    require!(
        rules.holder_fee_buy_bps <= b.max_holder_fee_bps
            && rules.holder_fee_sell_bps <= b.max_holder_fee_bps,
        LaunchError::HolderFeeTooHigh
    );
    require!(
        rules.burn_buy_bps <= b.max_burn_bps && rules.burn_sell_bps <= b.max_burn_bps,
        LaunchError::BurnTooHigh
    );
    for (holder, burn) in [
        (rules.holder_fee_buy_bps, rules.burn_buy_bps),
        (rules.holder_fee_sell_bps, rules.burn_sell_bps),
    ] {
        let total = u32::from(creator_fee_bps) + u32::from(holder) + u32::from(burn);
        require!(
            total <= u32::from(b.max_rules_fee_bps),
            LaunchError::RulesFeeTooHigh
        );
    }
    if rules.max_wallet_bps != 0 {
        require!(
            b.min_max_wallet_bps <= rules.max_wallet_bps
                && rules.max_wallet_bps <= b.max_max_wallet_bps,
            LaunchError::MaxWalletOutOfBounds
        );
        // A cap of nothing at this supply would refuse every buy until graduation.
        require!(
            max_wallet_cap(config.supply, rules.max_wallet_bps) > 0,
            LaunchError::MaxWalletOutOfBounds
        );
    }
    require!(
        rules.creator_lock_secs <= b.max_creator_lock_secs,
        LaunchError::CreatorLockTooLong
    );
    if rules.early_window_secs == 0 {
        require!(rules.early_lock_secs == 0, LaunchError::InvalidEarlyLock);
    } else {
        require!(
            rules.early_window_secs <= b.max_early_window_secs
                && rules.early_lock_secs > rules.early_window_secs
                && rules.early_lock_secs <= b.max_early_lock_secs,
            LaunchError::InvalidEarlyLock
        );
    }
    Ok(())
}

/// The creator's own token hook as the remaining accounts carry it (§5.8): the program, the
/// token program's signer for it and the registry's extras, each checked in [`plan`].
struct CustomHook<'info> {
    program: AccountInfo<'info>,
    signer: AccountInfo<'info>,
    extras: Vec<AccountInfo<'info>>,
    flags: u16,
}

impl<'info> CustomHook<'info> {
    fn hook(&self) -> cpi::TokenHook {
        (*self.program.key, *self.signer.key)
    }
}

/// What `create_launch` checked and derived, for the steps that follow. Each step is its own
/// function (`#[inline(never)]`), so no single frame holds every step's accounts.
struct Plan<'info> {
    now: i64,
    slot: u64,
    curve: CurveParams,
    rules: LaunchRules,
    creator_fee_bps: u16,
    modules: u8,
    mint: Pubkey,
    launch: Pubkey,
    quote_mint: Pubkey,
    pool: Pubkey,
    registry_bump: u8,
    kit_config: Pubkey,
    holder_vault: Pubkey,
    kit_caller_bump: u8,
    creator_unlock_at: i64,
    early_window_end: i64,
    early_unlock_at: i64,
    config: Option<Pubkey>,
    custom: Option<CustomHook<'info>>,
}

impl<'info> Plan<'info> {
    /// The mint's hook: the creator's own from the config, the kit with kit modules, none
    /// otherwise.
    fn hook_program(&self) -> Option<Pubkey> {
        match &self.custom {
            Some(c) => Some(*c.program.key),
            None => (self.modules != 0).then_some(KIT_ID),
        }
    }

    /// The mint's hook flags: the config's for a custom hook, the kit's for its modules.
    fn mint_flags(&self) -> u16 {
        match &self.custom {
            Some(c) => c.flags,
            None => bordrless_kit::mint_flags(self.modules),
        }
    }

    /// The reward vault, with holder rewards.
    fn reward_vault(&self) -> Option<Pubkey> {
        self.rules.rewards_on().then_some(self.holder_vault)
    }

    fn custom_hook(&self) -> Option<Pubkey> {
        self.custom.as_ref().map(|c| *c.program.key)
    }

    fn custom_hook_flags(&self) -> u16 {
        self.custom.as_ref().map_or(0, |c| c.flags)
    }
}

fn present<'info>(account: &Option<UncheckedAccount<'info>>) -> Result<AccountInfo<'info>> {
    account
        .as_ref()
        .map(|a| a.to_account_info())
        .ok_or_else(|| error!(LaunchError::KitAccountsMissing))
}

/// The kit's accounts: none for a launch without kit modules; for one with them, each at its
/// address (the registry is the kit's own to check), the reward vault exactly with holder
/// rewards. Answers the bump of the mint's kit-caller PDA (0 without a kit).
fn check_kit_accounts(
    a: &CreateLaunch,
    mint: &Pubkey,
    modules: u8,
    kit_config: &Pubkey,
    holder_vault: &Pubkey,
) -> Result<u8> {
    if modules == 0 {
        require!(
            a.kit_program.is_none()
                && a.kit_config.is_none()
                && a.kit_registry.is_none()
                && a.reward_vault.is_none()
                && a.kit_caller.is_none()
                && a.kit_event_authority.is_none(),
            LaunchError::UnexpectedKitAccounts
        );
        return Ok(0);
    }
    require_keys_eq!(
        present(&a.kit_program)?.key(),
        KIT_ID,
        LaunchError::WrongKitAccount
    );
    require_keys_eq!(
        present(&a.kit_config)?.key(),
        *kit_config,
        LaunchError::WrongKitAccount
    );
    present(&a.kit_registry)?;
    match (
        &a.reward_vault,
        modules & bordrless_kit::modules::HOLDER_REWARDS != 0,
    ) {
        (Some(vault), true) => {
            require_keys_eq!(vault.key(), *holder_vault, LaunchError::WrongKitAccount)
        }
        (None, true) => return err!(LaunchError::KitAccountsMissing),
        (Some(_), false) => return err!(LaunchError::UnexpectedKitAccounts),
        (None, false) => {}
    }
    let (caller, caller_bump) =
        Pubkey::find_program_address(&[KIT_CALLER_SEED, mint.as_ref()], &crate::ID);
    require_keys_eq!(
        present(&a.kit_caller)?.key(),
        caller,
        LaunchError::WrongKitAccount
    );
    require_keys_eq!(
        present(&a.kit_event_authority)?.key(),
        KIT_EVENT_AUTHORITY,
        LaunchError::WrongKitAccount
    );
    Ok(caller_bump)
}

/// The custom hook's accounts (§5.8), before anything is created: none without a custom hook;
/// with one, the remaining accounts are the hook program (the config's, executable), the token
/// program's signer for it, the hook's registry for this mint (owned by the hook, a registry that
/// decodes: the hook must have been prepared for the mint) and exactly the registry's number of
/// extras.
fn check_custom_hook_accounts<'info>(
    remaining: &[AccountInfo<'info>],
    hook: Option<(Pubkey, u16)>,
    mint: &Pubkey,
) -> Result<Option<CustomHook<'info>>> {
    let Some((hook, flags)) = hook else {
        require!(
            remaining.is_empty(),
            LaunchError::UnexpectedCustomHookAccounts
        );
        return Ok(None);
    };
    require!(remaining.len() >= 3, LaunchError::CustomHookAccountsMissing);
    let program = &remaining[0];
    require_keys_eq!(*program.key, hook, LaunchError::InvalidCustomHook);
    require!(program.executable, LaunchError::InvalidCustomHook);
    let signer = &remaining[1];
    require_keys_eq!(
        *signer.key,
        token_client::hook_signer(&hook),
        LaunchError::WrongHookSigner
    );
    let registry = &remaining[2];
    require_keys_eq!(
        *registry.key,
        hook_accounts_address(&hook, mint).0,
        LaunchError::HookRegistryMissing
    );
    require_keys_eq!(*registry.owner, hook, LaunchError::HookRegistryMissing);
    let list = HookAccountList::decode(&registry.try_borrow_data()?)
        .ok_or(LaunchError::HookRegistryMissing)?;
    let extras = &remaining[3..];
    require!(
        list.accounts.len() == extras.len(),
        LaunchError::HookExtrasMismatch
    );
    Ok(Some(CustomHook {
        program: program.clone(),
        signer: signer.clone(),
        extras: extras.to_vec(),
        flags,
    }))
}

/// The checks of `create_launch` (the config, the metadata, the creator fee, the rules, the
/// curve, the kit's accounts, the custom hook's) and the addresses and times the steps need.
#[inline(never)]
fn plan<'info>(
    ctx: &Context<'info, CreateLaunch<'info>>,
    args: &CreateLaunchArgs,
) -> Result<Box<Plan<'info>>> {
    let clock = Clock::get()?;
    let config = &ctx.accounts.config;
    require!(!config.paused, LaunchError::Paused);
    require!(
        !args.name.is_empty()
            && args.name.len() <= 32
            && !args.symbol.is_empty()
            && args.symbol.len() <= 10
            && args.uri.len() <= 200,
        LaunchError::InvalidMetadata
    );
    // The rules: the launch config's when there is one (the arguments must match it), else the
    // arguments'. Bounds and the creator fee are checked either way: the config they were made
    // under may have changed.
    let (rules, creator_fee_bps, hook, config_key) = match &ctx.accounts.launch_config {
        Some(lc) => {
            require!(
                args.rules == lc.rules && args.creator_fee_bps == lc.creator_fee_bps,
                LaunchError::ConfigMismatch
            );
            (lc.rules, lc.creator_fee_bps, lc.hook(), Some(lc.key()))
        }
        None => (args.rules, args.creator_fee_bps, None, None),
    };
    require!(
        creator_fee_bps <= config.max_creator_fee_bps,
        LaunchError::CreatorFeeTooHigh
    );
    check_rules(&rules, creator_fee_bps, config)?;
    let modules = rules.modules();
    if hook.is_some() {
        require!(modules == 0, LaunchError::CustomHookWithKitRules);
    }
    require!(
        args.virtual_quote >= config.min_virtual_quote
            && args.virtual_quote <= config.max_virtual_quote,
        LaunchError::VirtualQuoteOutOfBounds
    );
    let curve = curve_params(
        config.supply,
        u64::from(config.curve_bps),
        args.virtual_quote,
    )
    .ok_or(LaunchError::BadCurve)?;

    let mint = ctx.accounts.mint.key();
    let quote_mint = ctx.accounts.quote_mint.key();
    // The pool, its LP mint and the holdings are bound by the programs that create them, each
    // checking its seeds; the registry is this program's, at the pool's PDA.
    let pool = ctx.accounts.pool.key();
    let (registry, registry_bump) = hook_accounts_address(&crate::ID, &pool);
    require_keys_eq!(
        ctx.accounts.registry.key(),
        registry,
        LaunchError::WrongHolding
    );
    // The kit's addresses, derived whatever the modules: the pool hook names them as fixed keys.
    let (kit_config, _) = Pubkey::find_program_address(&[KIT_SEED, mint.as_ref()], &KIT_ID);
    let holder_vault = token_client::holding_address(&quote_mint, &kit_config);
    let kit_caller_bump =
        check_kit_accounts(ctx.accounts, &mint, modules, &kit_config, &holder_vault)?;
    let custom = check_custom_hook_accounts(ctx.remaining_accounts, hook, &mint)?;
    let now = clock.unix_timestamp;
    let at = |secs: u32| -> i64 {
        if secs > 0 {
            now + i64::from(secs)
        } else {
            0
        }
    };
    let (early_window_end, early_unlock_at) = if rules.early_window_secs > 0 {
        (at(rules.early_window_secs), at(rules.early_lock_secs))
    } else {
        (0, 0)
    };
    Ok(Box::new(Plan {
        now,
        slot: clock.slot,
        curve,
        rules,
        creator_fee_bps,
        modules,
        mint,
        launch: ctx.accounts.launch.key(),
        quote_mint,
        pool,
        registry_bump,
        kit_config,
        holder_vault,
        kit_caller_bump,
        creator_unlock_at: at(rules.creator_lock_secs),
        early_window_end,
        early_unlock_at,
        config: config_key,
        custom,
    }))
}

/// The mint's token hook on the instructions that move it: the hook program and the token
/// program's signer for it (the token program's id for both without a hook), the hook's extra
/// accounts, and the slice the DEX takes (`[hook, signer, extras...]`, empty without a hook).
struct Slice<'info> {
    hook: Option<(AccountInfo<'info>, AccountInfo<'info>)>,
    extras: Vec<AccountInfo<'info>>,
    extra_metas: Vec<AccountMeta>,
    metas: Vec<AccountMeta>,
    /// Whether the extras go along on `mint_to` (a custom hook may subscribe to mints; the kit
    /// does not).
    on_mint: bool,
}

impl<'info> Slice<'info> {
    fn token_hook(&self) -> Option<cpi::TokenHook> {
        self.hook.as_ref().map(|(p, s)| (*p.key, *s.key))
    }

    /// The two hook account infos of a token instruction (the token program for both without).
    fn hook_infos(&self, token_program: &AccountInfo<'info>) -> [AccountInfo<'info>; 2] {
        match &self.hook {
            Some((p, s)) => [p.clone(), s.clone()],
            None => [token_program.clone(), token_program.clone()],
        }
    }
}

/// The mint's slice: the creator's own hook with its registry extras as passed (§5.8), the kit
/// with its config and the reward vault (or the kit's id) (§5.3), or none.
fn slice_of<'info>(
    ctx: &Context<'info, CreateLaunch<'info>>,
    plan: &Plan<'info>,
) -> Result<Slice<'info>> {
    if let Some(c) = &plan.custom {
        let extra_metas: Vec<AccountMeta> = c
            .extras
            .iter()
            .map(|a| AccountMeta {
                pubkey: *a.key,
                is_signer: false,
                is_writable: a.is_writable,
            })
            .collect();
        return Ok(Slice {
            hook: Some((c.program.clone(), c.signer.clone())),
            extras: c.extras.clone(),
            metas: cpi::custom_slice(c.hook(), &extra_metas),
            extra_metas,
            on_mint: true,
        });
    }
    if plan.modules != 0 {
        let program = present(&ctx.accounts.kit_program)?;
        let vault = match plan.reward_vault() {
            Some(_) => present(&ctx.accounts.reward_vault)?,
            None => program.clone(),
        };
        return Ok(Slice {
            hook: Some((program, ctx.accounts.token_hook_signer.to_account_info())),
            extras: vec![present(&ctx.accounts.kit_config)?, vault],
            extra_metas: cpi::kit_extras(plan.kit_config, plan.reward_vault()).to_vec(),
            metas: cpi::kit_slice(plan.kit_config, plan.reward_vault()),
            on_mint: false,
        });
    }
    Ok(Slice {
        hook: None,
        extras: vec![],
        extra_metas: vec![],
        metas: vec![],
        on_mint: false,
    })
}

/// 1. The launch fee.
#[inline(never)]
fn pay_launch_fee<'info>(ctx: &Context<'info, CreateLaunch<'info>>) -> Result<()> {
    let fee = ctx.accounts.config.launch_fee_lamports;
    if fee > 0 {
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                system_program::Transfer {
                    from: ctx.accounts.creator.to_account_info(),
                    to: ctx.accounts.treasury.to_account_info(),
                },
            ),
            fee,
        )?;
    }
    Ok(())
}

/// 2. The mint, with fixed metadata and the launch as its only authority. With kit rules the kit
///    is its hook from this first instruction, with a config's custom hook that program is, with
///    nobody able to change it; without either it has no hook at all.
#[inline(never)]
fn create_mint<'info>(
    ctx: &Context<'info, CreateLaunch<'info>>,
    args: &CreateLaunchArgs,
    plan: &Plan<'info>,
) -> Result<()> {
    let config = &ctx.accounts.config;
    let ix = cpi::token_create_mint(
        ctx.accounts.creator.key(),
        plan.mint,
        CreateMintArgs {
            decimals: config.decimals,
            name: args.name.clone(),
            symbol: args.symbol.clone(),
            uri: args.uri.clone(),
            max_supply: config.supply,
            mint_authority: Some(plan.launch),
            freeze_authority: None,
            hook_program: plan.hook_program(),
            hook_flags: plan.mint_flags(),
            hook_authority: None,
            metadata_authority: None,
        },
    );
    invoke_signed(
        &ix,
        &[
            ctx.accounts.creator.to_account_info(),
            ctx.accounts.mint.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
            ctx.accounts.token_event_authority.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
        ],
        &[],
    )?;
    Ok(())
}

/// 3. A holding of the launch (of the token, or of the quote).
#[inline(never)]
fn create_launch_holding<'info>(
    ctx: &Context<'info, CreateLaunch<'info>>,
    holding: AccountInfo<'info>,
    holding_mint: AccountInfo<'info>,
) -> Result<()> {
    let ix = cpi::token_create_holding(
        ctx.accounts.creator.key(),
        holding_mint.key(),
        ctx.accounts.launch.key(),
        holding.key(),
    );
    invoke_signed(
        &ix,
        &[
            ctx.accounts.creator.to_account_info(),
            holding_mint,
            ctx.accounts.launch.to_account_info(),
            holding,
            ctx.accounts.system_program.to_account_info(),
            ctx.accounts.token_event_authority.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
        ],
        &[],
    )?;
    Ok(())
}

/// 4. The whole supply to the launch (the mint's hook passed; the kit does not subscribe to
///    mints, a custom hook may, so its extras go along), then no more minting ever.
#[inline(never)]
fn mint_supply<'info>(ctx: &Context<'info, CreateLaunch<'info>>, plan: &Plan<'info>) -> Result<()> {
    let launch_seeds = LaunchSeeds::new(plan.mint, ctx.bumps.launch);
    let seeds = launch_seeds.seeds();
    let token_program = ctx.accounts.token_program.to_account_info();
    let slice = slice_of(ctx, plan)?;
    let [hook_info, signer_info] = slice.hook_infos(&token_program);
    let extra_metas: &[AccountMeta] = if slice.on_mint {
        &slice.extra_metas
    } else {
        &[]
    };
    let ix = cpi::token_mint_to(
        plan.launch,
        plan.mint,
        ctx.accounts.launch_reserve.key(),
        slice.token_hook(),
        extra_metas,
        ctx.accounts.config.supply,
    );
    let mut infos = vec![
        ctx.accounts.launch.to_account_info(),
        ctx.accounts.mint.to_account_info(),
        ctx.accounts.launch_reserve.to_account_info(),
        hook_info,
        signer_info,
        ctx.accounts.token_event_authority.to_account_info(),
        token_program.clone(),
    ];
    if slice.on_mint {
        infos.extend(slice.extras.iter().cloned());
    }
    invoke_signed(&ix, &infos, &[&seeds])?;
    let ix = cpi::token_set_authority(plan.launch, plan.mint, AuthorityKind::Mint, None);
    invoke_signed(
        &ix,
        &[
            ctx.accounts.launch.to_account_info(),
            ctx.accounts.mint.to_account_info(),
            ctx.accounts.token_event_authority.to_account_info(),
            token_program,
        ],
        &[&seeds],
    )?;
    Ok(())
}

/// 5. The kit, installed by the kit-caller PDA of this mint (which signs nothing else), the
///    creator paying. Neither the launch PDA nor this program's hook authority is passed.
#[inline(never)]
fn install_kit<'info>(ctx: &Context<'info, CreateLaunch<'info>>, plan: &Plan<'info>) -> Result<()> {
    let program = present(&ctx.accounts.kit_program)?;
    let caller = present(&ctx.accounts.kit_caller)?;
    let registry = present(&ctx.accounts.kit_registry)?;
    let vault = match plan.reward_vault() {
        Some(_) => present(&ctx.accounts.reward_vault)?,
        None => program.clone(),
    };
    let ix = cpi::kit_init(
        &cpi::KitInitKeys {
            kit_caller: caller.key(),
            payer: ctx.accounts.creator.key(),
            mint: plan.mint,
            launch_reserve: ctx.accounts.launch_reserve.key(),
            kit_config: plan.kit_config,
            registry: registry.key(),
            reward_mint: plan.quote_mint,
            reward_vault: plan.reward_vault(),
        },
        KitInitArgs {
            launch: plan.launch,
            pool: plan.pool,
            creator: ctx.accounts.creator.key(),
            reward_mint: plan.quote_mint,
            modules: plan.modules,
            max_wallet_bps: plan.rules.max_wallet_bps,
            creator_unlock_at: plan.creator_unlock_at,
            early_window_end: plan.early_window_end,
            early_unlock_at: plan.early_unlock_at,
            kit_caller_bump: plan.kit_caller_bump,
        },
    );
    let caller_seeds = KitCallerSeeds::new(plan.mint, plan.kit_caller_bump);
    invoke_signed(
        &ix,
        &[
            caller,
            ctx.accounts.creator.to_account_info(),
            ctx.accounts.mint.to_account_info(),
            ctx.accounts.launch_reserve.to_account_info(),
            present(&ctx.accounts.kit_config)?,
            registry,
            ctx.accounts.quote_mint.to_account_info(),
            vault,
            ctx.accounts.token_program.to_account_info(),
            ctx.accounts.token_event_authority.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
            present(&ctx.accounts.kit_event_authority)?,
            program,
        ],
        &[&caller_seeds.seeds()],
    )?;
    Ok(())
}

/// 6. The curve pool, created by this program as its hook: the DEX skips the initialize
///    callbacks and lets a curve through because our hook authority signs. The base token-hook
///    slice (the kit, the token program's signer for it, its config, the reward vault or the
///    kit's id; or the custom hook, its signer and its extras) goes along so the deposit runs the
///    mint's hook: launch to pool, excluded to excluded for the kit.
#[inline(never)]
fn create_curve_pool<'info>(
    ctx: &Context<'info, CreateLaunch<'info>>,
    plan: &Plan<'info>,
) -> Result<()> {
    let launch_seeds = LaunchSeeds::new(plan.mint, ctx.bumps.launch);
    let seeds = launch_seeds.seeds();
    let hook_seeds: &[&[u8]] = &[HOOK_AUTHORITY_SEED, &[HOOK_AUTHORITY_BUMP]];
    let slice = slice_of(ctx, plan)?;
    let config = &ctx.accounts.config;
    let ix = cpi::dex_create_pool(
        &cpi::CreatePoolKeys {
            payer: ctx.accounts.creator.key(),
            authority: plan.launch,
            treasury: ctx.accounts.treasury.key(),
            base_mint: plan.mint,
            quote_mint: plan.quote_mint,
            pool: plan.pool,
            lp_mint: ctx.accounts.lp_mint.key(),
            base_vault: ctx.accounts.base_vault.key(),
            quote_vault: ctx.accounts.quote_vault.key(),
            authority_base: ctx.accounts.launch_reserve.key(),
            authority_quote: ctx.accounts.launch_quote.key(),
            authority_lp: ctx.accounts.launch_lp.key(),
        },
        CreatePoolArgs {
            lp_fee_bps: config.lp_fee_bps,
            hook_program: crate::ID,
            hook_flags: LAUNCH_HOOK_FLAGS,
            virtual_base: plan.curve.virtual_base,
            virtual_quote: plan.curve.virtual_quote,
            base_amount: plan.curve.curve_tokens,
            quote_amount: 0,
            base_hook_accounts: slice.metas.len() as u8,
            quote_hook_accounts: 0,
            hook_data: vec![],
        },
        slice.metas.clone(),
    );
    let mut infos = vec![
        ctx.accounts.creator.to_account_info(),
        ctx.accounts.launch.to_account_info(),
        ctx.accounts.dex_config.to_account_info(),
        ctx.accounts.treasury.to_account_info(),
        ctx.accounts.mint.to_account_info(),
        ctx.accounts.quote_mint.to_account_info(),
        ctx.accounts.pool.to_account_info(),
        ctx.accounts.lp_mint.to_account_info(),
        ctx.accounts.base_vault.to_account_info(),
        ctx.accounts.quote_vault.to_account_info(),
        ctx.accounts.launch_reserve.to_account_info(),
        ctx.accounts.launch_quote.to_account_info(),
        ctx.accounts.launch_lp.to_account_info(),
        ctx.accounts.program.to_account_info(),
        ctx.accounts.hook_signer.to_account_info(),
        ctx.accounts.dex_hook_signer.to_account_info(),
        ctx.accounts.token_program.to_account_info(),
        ctx.accounts.token_event_authority.to_account_info(),
        ctx.accounts.system_program.to_account_info(),
        ctx.accounts.dex_event_authority.to_account_info(),
        ctx.accounts.swap_program.to_account_info(),
    ];
    if let Some((program, signer)) = &slice.hook {
        infos.push(program.clone());
        infos.push(signer.clone());
        infos.extend(slice.extras.iter().cloned());
    }
    invoke_signed(&ix, &infos, &[&seeds, hook_seeds])?;
    Ok(())
}

/// 7. The registry of this hook's extra accounts for the pool.
#[inline(never)]
fn write_pool_registry<'info>(
    ctx: &Context<'info, CreateLaunch<'info>>,
    plan: &Plan<'info>,
) -> Result<()> {
    write_registry(
        &ctx.accounts.creator.to_account_info(),
        &ctx.accounts.registry.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        &crate::ID,
        &plan.pool,
        plan.registry_bump,
        &registry_list(plan.holder_vault, plan.kit_config),
    )
}

/// 8. The launch, and the config's count.
#[inline(never)]
fn record_launch<'info>(
    ctx: &mut Context<'info, CreateLaunch<'info>>,
    plan: &Plan<'info>,
) -> Result<()> {
    let bump = ctx.bumps.launch;
    let reserve_holding = ctx.accounts.launch_reserve.key();
    let quote_holding = ctx.accounts.launch_quote.key();
    let lp_holding = ctx.accounts.launch_lp.key();
    let creator = ctx.accounts.creator.key();
    let config = &mut ctx.accounts.config;
    config.launches = config
        .launches
        .checked_add(1)
        .ok_or(LaunchError::MathOverflow)?;
    let (lp_fee_bps, sniper_window_secs, sniper_start_bps) = (
        config.lp_fee_bps,
        config.sniper_window_secs,
        config.sniper_start_bps,
    );
    let launch = &mut ctx.accounts.launch;
    launch.version = VERSION;
    launch.bump = bump;
    launch.mint = plan.mint;
    launch.creator = creator;
    launch.pool = plan.pool;
    launch.quote_mint = plan.quote_mint;
    launch.status = STATUS_CURVE;
    launch.creator_fee_bps = plan.creator_fee_bps;
    launch.lp_fee_bps = lp_fee_bps;
    launch.sniper_window_secs = sniper_window_secs;
    launch.sniper_start_bps = sniper_start_bps;
    launch.virtual_quote = plan.curve.virtual_quote;
    launch.virtual_base = plan.curve.virtual_base;
    launch.graduation_quote = plan.curve.graduation_quote;
    launch.curve_tokens = plan.curve.curve_tokens;
    launch.reserve_tokens = plan.curve.reserve_tokens;
    launch.reserve_holding = reserve_holding;
    launch.quote_holding = quote_holding;
    launch.lp_holding = lp_holding;
    launch.created_at = plan.now;
    launch.graduated_at = 0;
    launch.creator_fees_accrued = 0;
    launch.creator_fees_claimed = 0;
    launch.graduation_topup = 0;
    launch.graduation_burned = 0;
    launch.rules = plan.rules;
    launch.modules = plan.modules;
    launch.kit_config = plan.kit_config;
    launch.holder_vault = plan.holder_vault;
    launch.kit_caller_bump = plan.kit_caller_bump;
    launch.creator_unlock_at = plan.creator_unlock_at;
    launch.early_window_end = plan.early_window_end;
    launch.early_unlock_at = plan.early_unlock_at;
    launch.creator_bought = false;
    launch.holder_fees_accrued = 0;
    launch.burned_on_trades = 0;
    launch.config = plan.config.unwrap_or_default();
    launch.custom_hook = plan.custom_hook();
    launch.custom_hook_flags = plan.custom_hook_flags();
    launch.reserved = [0; 32];
    Ok(())
}

/// 9. `LaunchCreated`, with the rules, the kit, the config and the custom hook.
#[inline(never)]
fn emit_launch_created<'info>(
    ctx: &Context<'info, CreateLaunch<'info>>,
    args: CreateLaunchArgs,
    plan: &Plan<'info>,
) -> Result<()> {
    let config = &ctx.accounts.config;
    emit_cpi!(LaunchCreated {
        launch: plan.launch,
        mint: plan.mint,
        creator: ctx.accounts.creator.key(),
        pool: plan.pool,
        quote_mint: plan.quote_mint,
        lp_mint: ctx.accounts.lp_mint.key(),
        name: args.name,
        symbol: args.symbol,
        uri: args.uri,
        supply: config.supply,
        decimals: config.decimals,
        creator_fee_bps: plan.creator_fee_bps,
        lp_fee_bps: config.lp_fee_bps,
        sniper_window_secs: config.sniper_window_secs,
        sniper_start_bps: config.sniper_start_bps,
        virtual_quote: plan.curve.virtual_quote,
        virtual_base: plan.curve.virtual_base,
        graduation_quote: plan.curve.graduation_quote,
        curve_tokens: plan.curve.curve_tokens,
        reserve_tokens: plan.curve.reserve_tokens,
        launch_fee_lamports: config.launch_fee_lamports,
        rules: plan.rules,
        modules: plan.modules,
        kit_config: (plan.modules != 0).then_some(plan.kit_config),
        holder_vault: plan.reward_vault(),
        creator_unlock_at: plan.creator_unlock_at,
        early_window_end: plan.early_window_end,
        early_unlock_at: plan.early_unlock_at,
        config: plan.config,
        custom_hook: plan.custom_hook(),
        custom_hook_flags: plan.custom_hook_flags(),
        slot: plan.slot,
        ts: plan.now,
    });
    Ok(())
}

/// `create_launch`, in the order of §5.3.
pub fn process_create_launch<'info>(
    mut ctx: Context<'info, CreateLaunch<'info>>,
    args: CreateLaunchArgs,
) -> Result<()> {
    let plan = plan(&ctx, &args)?;
    pay_launch_fee(&ctx)?;
    create_mint(&ctx, &args, &plan)?;
    create_launch_holding(
        &ctx,
        ctx.accounts.launch_reserve.to_account_info(),
        ctx.accounts.mint.to_account_info(),
    )?;
    create_launch_holding(
        &ctx,
        ctx.accounts.launch_quote.to_account_info(),
        ctx.accounts.quote_mint.to_account_info(),
    )?;
    mint_supply(&ctx, &plan)?;
    if plan.modules != 0 {
        install_kit(&ctx, &plan)?;
    }
    create_curve_pool(&ctx, &plan)?;
    write_pool_registry(&ctx, &plan)?;
    record_launch(&mut ctx, &plan)?;
    emit_launch_created(&ctx, args, &plan)
}
