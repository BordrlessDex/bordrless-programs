//! Launches quoted in tokens (`docs/token-quotes.md` §2.2): a coin launched against 1 to 6 real
//! SPL Token or Token-2022 mints instead of SOL, never both.
//!
//! `create_token_launch` makes the mint (no token hook: no kit, no custom hook, no launch config),
//! mints the whole supply to the launch's holding and records the `Launch` (`reserved[18] = 2`;
//! `pool` and `quote_mint` name leg 0) and its `Legs`: per quote mint (the remaining accounts, in
//! order, each held to the DEX's policy, none wrapped SOL, none repeated) its program, decimals,
//! virtual quote (at least [`MIN_LEG_VIRTUAL_QUOTE`]), share of the supply (an even split, the
//! last leg taking the remainder), pool and fee account. `open_leg(i)`, permissionless, creates
//! leg `i`'s fee account (a token account of the quote at `["leg-fees", mint, quote_mint]` owned by
//! the launch), its pool (DEX `create_spl_pool` with this program as the hook: no virtual base, the
//! leg's virtual quote, the leg's share deposited, the share fee model) and the pool's registry.
//! The pool hook refuses trades until every leg is open, then takes the creator fee on the quote
//! side of every trade into the leg's fee account (`crate::instructions::hooks`).
//! `claim_creator_fees_token(i)`, anyone, pays leg `i`'s fee account to the creator's associated
//! token account of that quote. Legs never graduate.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::system_program;
use bordrless_hook::{
    hook_accounts_address, write_registry, AccountSource, ExtraAccount, HookAccountList, Seed,
    HOOK_AUTHORITY_SEED,
};
use bordrless_swap::instructions::CreatePoolArgs;
use bordrless_swap::spl;
use bordrless_token::instructions::CreateMintArgs;
use bordrless_token::state::AuthorityKind;

use crate::constants::*;
use crate::cpi;
use crate::error::LaunchError;
use crate::events::{LegCreatorFeesClaimed, LegOpened, LegTerms, TokenLaunchCreated};
use crate::instructions::launch::{check_rules, LaunchSeeds};
use crate::state::*;

/// Arguments of `create_token_launch`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct CreateTokenLaunchArgs {
    /// Name.
    pub name: String,
    /// Symbol.
    pub symbol: String,
    /// Metadata URI.
    pub uri: String,
    /// Creator fee in basis points of the quote side of every trade, within the config's bounds.
    pub creator_fee_bps: u16,
    /// Each leg's virtual quote, raw units of its token, in the order of the quote mints.
    pub virtual_quotes: Vec<u64>,
}

/// Accounts of `create_token_launch`. The remaining accounts are the quote mints, 1 to 6, in leg
/// order (read-only).
#[event_cpi]
#[derive(Accounts)]
pub struct CreateTokenLaunch<'info> {
    /// The creator (a wallet): pays the launch fee and the rent.
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(mut, address = CONFIG_ADDRESS)]
    pub config: Box<Account<'info, Config>>,
    /// CHECK: receives the launch fee (address-checked).
    #[account(mut, address = config.treasury @ LaunchError::WrongHolding)]
    pub treasury: UncheckedAccount<'info>,
    /// CHECK: the new mint; signs, created through the token program.
    #[account(mut)]
    pub mint: Signer<'info>,
    #[account(init, payer = creator, space = Launch::LEN, seeds = [LAUNCH_SEED, mint.key().as_ref()], bump)]
    pub launch: Box<Account<'info, Launch>>,
    #[account(init, payer = creator, space = Legs::LEN, seeds = [LEGS_SEED, mint.key().as_ref()], bump)]
    pub legs: Box<Account<'info, Legs>>,
    /// CHECK: the launch's holding of the coin, created here (the token program checks its
    /// address).
    #[account(mut)]
    pub launch_reserve: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = bordrless_token::ID @ LaunchError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority (address-checked).
    #[account(address = TOKEN_EVENT_AUTHORITY @ LaunchError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// The legs' terms from the quote mints and their virtual quotes, checked: 1 to 6, each an SPL
/// Token or Token-2022 mint the DEX's policy allows (`bordrless_swap::spl::check_quote_mint`, which
/// `create_spl_pool` repeats), none wrapped SOL, none repeated, each virtual quote at least the
/// minimum; the pools' and fee accounts' addresses derived.
fn leg_terms(
    quotes: &[AccountInfo],
    virtual_quotes: &[u64],
    mint: &Pubkey,
    supply: u64,
    lp_fee_bps: u16,
) -> Result<[Leg; MAX_LEGS]> {
    let n = quotes.len();
    require!(
        (1..=MAX_LEGS).contains(&n) && virtual_quotes.len() == n,
        LaunchError::InvalidLegCount
    );
    let count = n as u64;
    let even = supply / count;
    let mut legs = [Leg::default(); MAX_LEGS];
    for (i, info) in quotes.iter().enumerate() {
        require!(
            *info.key != spl::NATIVE_MINT && *info.key != spl::NATIVE_MINT_2022,
            LaunchError::SolQuoteToken
        );
        require!(
            !quotes[..i].iter().any(|q| q.key == info.key),
            LaunchError::DuplicateQuoteToken
        );
        let (quote_program, decimals) = spl::check_quote_mint(info).map_err(LaunchError::from)?;
        let virtual_quote = virtual_quotes[i];
        require!(
            virtual_quote >= MIN_LEG_VIRTUAL_QUOTE,
            LaunchError::LegVirtualQuoteTooSmall
        );
        // An even split; the last leg takes what the division leaves (`legShares`).
        let share = if i == n - 1 {
            supply - even * (count - 1)
        } else {
            even
        };
        let pool =
            bordrless_swap::state::Pool::address(mint, info.key, lp_fee_bps, Some(crate::ID)).0;
        let (fee_account, fee_bump) = Pubkey::find_program_address(
            &[LEG_FEES_SEED, mint.as_ref(), info.key.as_ref()],
            &crate::ID,
        );
        legs[i] = Leg {
            quote_mint: *info.key,
            quote_program,
            decimals,
            fee_bump,
            virtual_quote,
            share,
            pool,
            fee_account,
            creator_fees_accrued: 0,
            creator_fees_claimed: 0,
            opened_at: 0,
        };
    }
    Ok(legs)
}

/// The mint (no token hook, the launch its only authority), the launch's holding of it, the whole
/// supply into that holding, then no more minting ever.
#[inline(never)]
fn mint_coin<'info>(
    ctx: &Context<'info, CreateTokenLaunch<'info>>,
    args: &CreateTokenLaunchArgs,
) -> Result<()> {
    let a = &ctx.accounts;
    let config = &a.config;
    let (mint, launch) = (a.mint.key(), a.launch.key());
    let token_program = a.token_program.to_account_info();
    let events = a.token_event_authority.to_account_info();
    let ix = cpi::token_create_mint(
        a.creator.key(),
        mint,
        CreateMintArgs {
            decimals: config.decimals,
            name: args.name.clone(),
            symbol: args.symbol.clone(),
            uri: args.uri.clone(),
            max_supply: config.supply,
            mint_authority: Some(launch),
            freeze_authority: None,
            hook_program: None,
            hook_flags: 0,
            hook_authority: None,
            metadata_authority: None,
        },
    );
    invoke_signed(
        &ix,
        &[
            a.creator.to_account_info(),
            a.mint.to_account_info(),
            a.system_program.to_account_info(),
            events.clone(),
            token_program.clone(),
        ],
        &[],
    )?;
    let ix = cpi::token_create_holding(a.creator.key(), mint, launch, a.launch_reserve.key());
    invoke_signed(
        &ix,
        &[
            a.creator.to_account_info(),
            a.mint.to_account_info(),
            a.launch.to_account_info(),
            a.launch_reserve.to_account_info(),
            a.system_program.to_account_info(),
            events.clone(),
            token_program.clone(),
        ],
        &[],
    )?;
    let launch_seeds = LaunchSeeds::new(mint, ctx.bumps.launch);
    let seeds = launch_seeds.seeds();
    let ix = cpi::token_mint_to(
        launch,
        mint,
        a.launch_reserve.key(),
        None,
        &[],
        config.supply,
    );
    invoke_signed(
        &ix,
        &[
            a.launch.to_account_info(),
            a.mint.to_account_info(),
            a.launch_reserve.to_account_info(),
            token_program.clone(),
            token_program.clone(),
            events.clone(),
            token_program.clone(),
        ],
        &[&seeds],
    )?;
    let ix = cpi::token_set_authority(launch, mint, AuthorityKind::Mint, None);
    invoke_signed(
        &ix,
        &[
            a.launch.to_account_info(),
            a.mint.to_account_info(),
            events,
            token_program,
        ],
        &[&seeds],
    )?;
    Ok(())
}

/// `create_token_launch`.
pub fn process_create_token_launch<'info>(
    mut ctx: Context<'info, CreateTokenLaunch<'info>>,
    args: CreateTokenLaunchArgs,
) -> Result<()> {
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
    require!(
        args.creator_fee_bps <= config.max_creator_fee_bps,
        LaunchError::CreatorFeeTooHigh
    );
    check_rules(&LaunchRules::NONE, args.creator_fee_bps, config)?;
    let mint = ctx.accounts.mint.key();
    // A companion coin's pots are SOL: no companion here (`PartnerOnCompanion`'s pattern).
    require!(
        !is_companion_creator(&ctx.accounts.creator.key(), &mint),
        LaunchError::CompanionTokenLaunch
    );
    let (supply, lp_fee_bps, launch_fee) =
        (config.supply, config.lp_fee_bps, config.launch_fee_lamports);
    let legs = leg_terms(
        ctx.remaining_accounts,
        &args.virtual_quotes,
        &mint,
        supply,
        lp_fee_bps,
    )?;
    let count = ctx.remaining_accounts.len();

    // The launch fee, as for any launch.
    if launch_fee > 0 {
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                system_program::Transfer {
                    from: ctx.accounts.creator.to_account_info(),
                    to: ctx.accounts.treasury.to_account_info(),
                },
            ),
            launch_fee,
        )?;
    }
    mint_coin(&ctx, &args)?;

    let a = &mut ctx.accounts;
    a.config.launches = a
        .config
        .launches
        .checked_add(1)
        .ok_or(LaunchError::MathOverflow)?;
    let decimals = a.config.decimals;
    let (launch_key, legs_key) = (a.launch.key(), a.legs.key());
    let creator = a.creator.key();
    let reserve_holding = a.launch_reserve.key();
    let (kit_config, _) = Pubkey::find_program_address(&[KIT_SEED, mint.as_ref()], &KIT_ID);
    let launch = &mut a.launch;
    launch.version = VERSION;
    launch.bump = ctx.bumps.launch;
    launch.mint = mint;
    launch.creator = creator;
    launch.pool = legs[0].pool;
    launch.quote_mint = legs[0].quote_mint;
    launch.status = STATUS_CURVE;
    launch.creator_fee_bps = args.creator_fee_bps;
    launch.lp_fee_bps = lp_fee_bps;
    launch.sniper_window_secs = 0;
    launch.sniper_start_bps = 0;
    // Each leg's curve is in `Legs`; the launch never graduates.
    launch.virtual_quote = 0;
    launch.virtual_base = 0;
    launch.graduation_quote = u64::MAX;
    launch.curve_tokens = supply;
    launch.reserve_tokens = 0;
    launch.reserve_holding = reserve_holding;
    launch.quote_holding = legs[0].fee_account;
    launch.lp_holding = Pubkey::default();
    launch.created_at = clock.unix_timestamp;
    launch.graduated_at = 0;
    launch.creator_fees_accrued = 0;
    launch.creator_fees_claimed = 0;
    launch.graduation_topup = 0;
    launch.graduation_burned = 0;
    launch.rules = LaunchRules::NONE;
    launch.modules = 0;
    launch.kit_config = kit_config;
    launch.holder_vault = Pubkey::default();
    launch.kit_caller_bump = 0;
    launch.creator_unlock_at = 0;
    launch.early_window_end = 0;
    launch.early_unlock_at = 0;
    launch.creator_bought = false;
    launch.holder_fees_accrued = 0;
    launch.burned_on_trades = 0;
    launch.config = Pubkey::default();
    launch.custom_hook = None;
    launch.custom_hook_flags = 0;
    launch.author_share_bps = 0;
    launch.author_fees_paid = 0;
    launch.reserved = [0; 22];
    launch.set_token_quoted();
    let record = &mut a.legs;
    record.version = VERSION;
    record.bump = ctx.bumps.legs;
    record.mint = mint;
    record.launch = launch_key;
    record.count = count as u8;
    record.opened = 0;
    record.legs = legs;
    record.reserved = [0; 64];

    let terms = legs[..count]
        .iter()
        .map(|l| LegTerms {
            quote_mint: l.quote_mint,
            quote_program: spl::program_id(l.quote_program),
            decimals: l.decimals,
            virtual_quote: l.virtual_quote,
            share: l.share,
            pool: l.pool,
            fee_account: l.fee_account,
        })
        .collect();
    emit_cpi!(TokenLaunchCreated {
        launch: launch_key,
        mint,
        creator,
        legs_account: legs_key,
        name: args.name,
        symbol: args.symbol,
        uri: args.uri,
        supply,
        decimals,
        creator_fee_bps: args.creator_fee_bps,
        lp_fee_bps,
        launch_fee_lamports: launch_fee,
        legs: terms,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}

/// The extra accounts a leg pool's callbacks take, at indices 5 to 8 (`docs/token-quotes.md`
/// §2.4): the launch (w), the leg's fee account (w, `["leg-fees", base mint, quote mint]`), the
/// launch's `Legs` (w) and the kit config's fixed key (r, `launch.kit_config`, as a launch pool's).
pub fn leg_registry_list(kit_config: Pubkey) -> HookAccountList {
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
                program: crate::ID,
                seeds: vec![
                    Seed::Literal(LEG_FEES_SEED.to_vec()),
                    Seed::Account(2),
                    Seed::Account(3),
                ],
            },
        },
        ExtraAccount {
            writable: true,
            source: AccountSource::Pda {
                program: crate::ID,
                seeds: vec![Seed::Literal(LEGS_SEED.to_vec()), Seed::Account(2)],
            },
        },
        ExtraAccount {
            writable: false,
            source: AccountSource::Key(kit_config),
        },
    ])
}

/// Accounts of `open_leg(index)`: anyone opens leg `index` of a launch quoted in tokens, paying the
/// rent of its fee account, its pool, the pool's vaults and registry (and the DEX's pool creation
/// fee, if any).
#[event_cpi]
#[derive(Accounts)]
pub struct OpenLeg<'info> {
    /// Pays the rent.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(
        seeds = [LAUNCH_SEED, launch.mint.as_ref()],
        bump = launch.bump,
        constraint = launch.is_token_quoted() @ LaunchError::NotTokenLaunch
    )]
    pub launch: Box<Account<'info, Launch>>,
    #[account(mut, seeds = [LEGS_SEED, launch.mint.as_ref()], bump = legs.bump)]
    pub legs: Box<Account<'info, Legs>>,
    /// CHECK: address-checked: the coin.
    #[account(address = launch.mint @ LaunchError::WrongHolding)]
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the leg's quote mint (checked in the handler).
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: the leg's fee account (checked in the handler), created here.
    #[account(mut)]
    pub leg_fees: UncheckedAccount<'info>,
    /// CHECK: address-checked: the launch's holding of the coin (the deposit).
    #[account(mut, address = launch.reserve_holding @ LaunchError::WrongHolding)]
    pub launch_reserve: UncheckedAccount<'info>,
    /// CHECK: this hook's registry for the pool (address checked when written).
    #[account(mut)]
    pub registry: UncheckedAccount<'info>,
    /// CHECK: address-checked: the DEX config.
    #[account(mut, address = DEX_CONFIG @ LaunchError::WrongProgram)]
    pub dex_config: UncheckedAccount<'info>,
    /// CHECK: the DEX config's treasury (the DEX checks it).
    #[account(mut)]
    pub dex_treasury: UncheckedAccount<'info>,
    /// CHECK: the leg's pool (checked in the handler), created by the DEX.
    #[account(mut)]
    pub pool: UncheckedAccount<'info>,
    /// CHECK: the pool's base vault, created by the DEX through the token program.
    #[account(mut)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: the pool's quote vault, created by the DEX through the ATA program.
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: this program's hook authority (address-checked).
    #[account(address = LAUNCH_HOOK_AUTHORITY @ LaunchError::WrongProgram)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: the DEX's event authority (address-checked).
    #[account(address = DEX_EVENT_AUTHORITY @ LaunchError::WrongProgram)]
    pub dex_event_authority: UncheckedAccount<'info>,
    /// CHECK: the DEX.
    #[account(address = bordrless_swap::ID @ LaunchError::WrongProgram)]
    pub swap_program: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = bordrless_token::ID @ LaunchError::WrongProgram)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority (address-checked).
    #[account(address = TOKEN_EVENT_AUTHORITY @ LaunchError::WrongProgram)]
    pub token_event_authority: UncheckedAccount<'info>,
    /// CHECK: the leg's quote program (checked in the handler).
    pub quote_token_program: UncheckedAccount<'info>,
    /// CHECK: the associated token account program (address-checked).
    #[account(address = spl::ATA_PROGRAM_ID @ LaunchError::WrongProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// A program-owned account at a PDA of this program (`seeds`, which sign), `space` bytes owned by
/// `owner`, `payer` funding its rent. An address someone already sent lamports to is topped up,
/// allocated and assigned (a plain create would fail on it, and a leg that can't open would keep
/// the coin from ever trading).
fn create_pda_account<'info>(
    payer: &AccountInfo<'info>,
    account: &AccountInfo<'info>,
    system: &AccountInfo<'info>,
    space: usize,
    owner: &Pubkey,
    seeds: &[&[u8]],
) -> Result<()> {
    require_keys_eq!(
        *account.owner,
        system_program::ID,
        LaunchError::LegAlreadyOpen
    );
    let rent = Rent::get()?.minimum_balance(space);
    let current = account.lamports();
    if current == 0 {
        system_program::create_account(
            CpiContext::new_with_signer(
                system.key(),
                system_program::CreateAccount {
                    from: payer.clone(),
                    to: account.clone(),
                },
                &[seeds],
            ),
            rent,
            space as u64,
            owner,
        )?;
        return Ok(());
    }
    let top_up = rent.saturating_sub(current);
    if top_up > 0 {
        system_program::transfer(
            CpiContext::new(
                system.key(),
                system_program::Transfer {
                    from: payer.clone(),
                    to: account.clone(),
                },
            ),
            top_up,
        )?;
    }
    system_program::allocate(
        CpiContext::new_with_signer(
            system.key(),
            system_program::Allocate {
                account_to_allocate: account.clone(),
            },
            &[seeds],
        ),
        space as u64,
    )?;
    system_program::assign(
        CpiContext::new_with_signer(
            system.key(),
            system_program::Assign {
                account_to_assign: account.clone(),
            },
            &[seeds],
        ),
        owner,
    )?;
    Ok(())
}

/// 1. The leg's fee account: a token account of the quote at `["leg-fees", mint, quote_mint]`,
///    owned by the launch, sized for the mint's account extensions.
#[inline(never)]
fn create_leg_fees<'info>(
    ctx: &Context<'info, OpenLeg<'info>>,
    leg: &Leg,
    mint: &Pubkey,
) -> Result<()> {
    let a = &ctx.accounts;
    let program = a.quote_token_program.to_account_info();
    let quote_mint = a.quote_mint.to_account_info();
    let fees = a.leg_fees.to_account_info();
    let space = if leg.quote_program == spl::PROGRAM_TOKEN_2022 {
        usize::try_from(spl::account_data_size(&program, &quote_mint)?)
            .map_err(|_| LaunchError::MathOverflow)?
    } else {
        spl::ACCOUNT_LEN
    };
    let bump = [leg.fee_bump];
    let seeds: &[&[u8]] = &[LEG_FEES_SEED, mint.as_ref(), leg.quote_mint.as_ref(), &bump];
    create_pda_account(
        &a.payer.to_account_info(),
        &fees,
        &a.system_program.to_account_info(),
        space,
        program.key,
        seeds,
    )?;
    spl::initialize_account3(&program, &fees, &quote_mint, &a.launch.key())
}

/// 2. The leg's pool: DEX `create_spl_pool` as the pool's hook (this program's hook authority
///    signs), the launch depositing the leg's share (the launch signs).
#[inline(never)]
fn create_leg_pool<'info>(ctx: &Context<'info, OpenLeg<'info>>, leg: &Leg) -> Result<()> {
    let a = &ctx.accounts;
    let launch = &a.launch;
    let launch_seeds = LaunchSeeds::new(launch.mint, launch.bump);
    let hook_seeds: &[&[u8]] = &[HOOK_AUTHORITY_SEED, &[HOOK_AUTHORITY_BUMP]];
    let ix = cpi::dex_create_spl_pool(
        &cpi::CreateSplPoolKeys {
            payer: a.payer.key(),
            authority: launch.key(),
            treasury: a.dex_treasury.key(),
            base_mint: launch.mint,
            quote_mint: leg.quote_mint,
            pool: leg.pool,
            base_vault: a.base_vault.key(),
            quote_vault: a.quote_vault.key(),
            authority_base: a.launch_reserve.key(),
            quote_program: a.quote_token_program.key(),
        },
        CreatePoolArgs {
            lp_fee_bps: launch.lp_fee_bps,
            hook_program: crate::ID,
            hook_flags: LEG_HOOK_FLAGS,
            virtual_base: 0,
            virtual_quote: leg.virtual_quote,
            base_amount: leg.share,
            quote_amount: 0,
            base_hook_accounts: 0,
            quote_hook_accounts: 0,
            hook_data: vec![],
        },
    );
    invoke_signed(
        &ix,
        &[
            a.payer.to_account_info(),
            launch.to_account_info(),
            a.dex_config.to_account_info(),
            a.dex_treasury.to_account_info(),
            a.mint.to_account_info(),
            a.quote_mint.to_account_info(),
            a.pool.to_account_info(),
            a.base_vault.to_account_info(),
            a.quote_vault.to_account_info(),
            a.launch_reserve.to_account_info(),
            a.hook_signer.to_account_info(),
            a.token_program.to_account_info(),
            a.token_event_authority.to_account_info(),
            a.quote_token_program.to_account_info(),
            a.associated_token_program.to_account_info(),
            a.system_program.to_account_info(),
            a.dex_event_authority.to_account_info(),
            a.swap_program.to_account_info(),
        ],
        &[&launch_seeds.seeds(), hook_seeds],
    )?;
    Ok(())
}

/// `open_leg(index)`.
pub fn process_open_leg<'info>(ctx: Context<'info, OpenLeg<'info>>, index: u8) -> Result<()> {
    let clock = Clock::get()?;
    let i = usize::from(index);
    let leg = {
        let legs = &ctx.accounts.legs;
        require!(i < usize::from(legs.count), LaunchError::NoSuchLeg);
        legs.legs[i]
    };
    require!(leg.opened_at == 0, LaunchError::LegAlreadyOpen);
    let a = &ctx.accounts;
    require!(
        a.quote_mint.key() == leg.quote_mint
            && a.pool.key() == leg.pool
            && a.leg_fees.key() == leg.fee_account
            && a.quote_token_program.key() == spl::program_id(leg.quote_program),
        LaunchError::WrongLegAccount
    );
    let mint = a.launch.mint;
    let launch_key = a.launch.key();
    create_leg_fees(&ctx, &leg, &mint)?;
    create_leg_pool(&ctx, &leg)?;
    // 3. The pool's registry: the launch, the leg's fee account, the `Legs`, the kit config.
    let (registry, registry_bump) = hook_accounts_address(&crate::ID, &leg.pool);
    require_keys_eq!(
        ctx.accounts.registry.key(),
        registry,
        LaunchError::WrongHolding
    );
    write_registry(
        &ctx.accounts.payer.to_account_info(),
        &ctx.accounts.registry.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        &crate::ID,
        &leg.pool,
        registry_bump,
        &leg_registry_list(ctx.accounts.launch.kit_config),
    )?;
    // 4. Opened.
    let payer = ctx.accounts.payer.key();
    let legs = &mut ctx.accounts.legs;
    legs.legs[i].opened_at = clock.unix_timestamp;
    legs.opened = legs
        .opened
        .checked_add(1)
        .ok_or(LaunchError::MathOverflow)?;
    let (opened, count) = (legs.opened, legs.count);
    emit_cpi!(LegOpened {
        launch: launch_key,
        mint,
        index,
        quote_mint: leg.quote_mint,
        pool: leg.pool,
        fee_account: leg.fee_account,
        virtual_quote: leg.virtual_quote,
        share: leg.share,
        opened,
        count,
        payer,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}

/// Accounts of `claim_creator_fees_token(index)`: anyone pays leg `index`'s creator fees to the
/// creator's associated token account of the leg's quote (created if needed, `payer` paying).
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimCreatorFeesToken<'info> {
    /// Whoever sends it; pays the creator's token account's rent when it is created.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(
        seeds = [LAUNCH_SEED, launch.mint.as_ref()],
        bump = launch.bump,
        constraint = launch.is_token_quoted() @ LaunchError::NotTokenLaunch
    )]
    pub launch: Box<Account<'info, Launch>>,
    #[account(mut, seeds = [LEGS_SEED, launch.mint.as_ref()], bump = legs.bump)]
    pub legs: Box<Account<'info, Legs>>,
    /// CHECK: the leg's quote mint (checked in the handler).
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: the leg's fee account (checked in the handler).
    #[account(mut)]
    pub leg_fees: UncheckedAccount<'info>,
    /// CHECK: address-checked: the launch's creator.
    #[account(address = launch.creator @ LaunchError::NotCreator)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: the creator's associated token account of the quote (the ATA program checks its
    /// address; created if needed).
    #[account(mut)]
    pub creator_quote: UncheckedAccount<'info>,
    /// CHECK: the leg's quote program (checked in the handler).
    pub quote_token_program: UncheckedAccount<'info>,
    /// CHECK: the associated token account program (address-checked).
    #[account(address = spl::ATA_PROGRAM_ID @ LaunchError::WrongProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `claim_creator_fees_token(index)`: the whole balance of the leg's fee account (the launch
/// signs) to the creator's associated token account of the quote.
pub fn process_claim_creator_fees_token<'info>(
    ctx: Context<'info, ClaimCreatorFeesToken<'info>>,
    index: u8,
) -> Result<()> {
    let clock = Clock::get()?;
    let i = usize::from(index);
    let leg = {
        let legs = &ctx.accounts.legs;
        require!(i < usize::from(legs.count), LaunchError::NoSuchLeg);
        legs.legs[i]
    };
    let a = &ctx.accounts;
    require!(
        a.quote_mint.key() == leg.quote_mint
            && a.leg_fees.key() == leg.fee_account
            && a.quote_token_program.key() == spl::program_id(leg.quote_program),
        LaunchError::WrongLegAccount
    );
    // A leg not yet open has no fee account.
    require!(leg.opened_at != 0, LaunchError::NothingToClaim);
    let program = a.quote_token_program.to_account_info();
    let quote_mint = a.quote_mint.to_account_info();
    let creator_quote = a.creator_quote.to_account_info();
    spl::create_ata_idempotent(
        &a.payer.to_account_info(),
        &creator_quote,
        &a.creator.to_account_info(),
        &quote_mint,
        &a.system_program.to_account_info(),
        &program,
        &a.associated_token_program.to_account_info(),
    )?;
    let to =
        spl::read_token_account(&creator_quote, program.key).ok_or(LaunchError::WrongHolding)?;
    require!(
        to.owner == a.launch.creator && to.mint == leg.quote_mint,
        LaunchError::WrongHolding
    );
    let fees = a.leg_fees.to_account_info();
    let amount = spl::read_token_account(&fees, program.key)
        .ok_or(LaunchError::WrongLegAccount)?
        .amount;
    require!(amount > 0, LaunchError::NothingToClaim);
    let launch_seeds = LaunchSeeds::new(a.launch.mint, a.launch.bump);
    spl::transfer_checked(
        &program,
        &fees,
        &quote_mint,
        &creator_quote,
        &a.launch.to_account_info(),
        amount,
        leg.decimals,
        &[&launch_seeds.seeds()],
    )?;
    let (launch_key, mint, creator) = (a.launch.key(), a.launch.mint, a.launch.creator);
    // A running total for display (the claim pays the account's balance): saturating, so it never
    // blocks a claim.
    let legs = &mut ctx.accounts.legs;
    let claimed = legs.legs[i].creator_fees_claimed.saturating_add(amount);
    legs.legs[i].creator_fees_claimed = claimed;
    emit_cpi!(LegCreatorFeesClaimed {
        launch: launch_key,
        mint,
        index,
        quote_mint: leg.quote_mint,
        creator,
        destination: creator_quote.key(),
        amount,
        claimed_total: claimed,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}
