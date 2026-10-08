//! Pool creation, curve finalization and protocol fee collection.

use anchor_lang::prelude::*;
use anchor_lang::system_program;
use bordrless_core::{initial_lp, policy};
use bordrless_hook::{
    discriminators, pool_flags, Allowed, Phase, PoolHookArgs, PoolOp, HOOK_AUTHORITY_SEED,
    MAX_HOOK_DATA,
};
use bordrless_token::instructions::CreateMintArgs;
use bordrless_token::state::Mint as TokenMint;

use crate::constants::*;
use crate::error::SwapError;
use crate::events::*;
use crate::hooks::{split_extras, PoolHookCall};
use crate::state::*;
use crate::token::{balance, read_holding, TokenAccounts, TokenSide};

/// Arguments of `create_pool`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct CreatePoolArgs {
    /// LP fee in basis points of the input.
    pub lp_fee_bps: u16,
    /// Hook program; the default key for none.
    pub hook_program: Pubkey,
    /// Its flags.
    pub hook_flags: u16,
    /// Virtual base offset (a curve); zero for an ordinary pool.
    pub virtual_base: u64,
    /// Virtual quote offset (a curve); zero for an ordinary pool.
    pub virtual_quote: u64,
    /// Base deposited now.
    pub base_amount: u64,
    /// Quote deposited now (zero for a curve).
    pub quote_amount: u64,
    /// Number of remaining accounts that belong to the base mint's token hook (its program, the
    /// token program's signer for it, its extras; none without a hook).
    pub base_hook_accounts: u8,
    /// Number of remaining accounts that belong to the quote mint's token hook.
    pub quote_hook_accounts: u8,
    /// Opaque data for the pool hook.
    pub hook_data: Vec<u8>,
}

/// Accounts of `create_pool`.
#[event_cpi]
#[derive(Accounts)]
#[instruction(args: CreatePoolArgs)]
pub struct CreatePool<'info> {
    /// Pays the rent and the creation fee.
    #[account(mut)]
    pub payer: Signer<'info>,
    /// Owns the deposit holdings and receives the LP.
    pub authority: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    /// CHECK: the config's treasury (address-checked).
    #[account(mut, address = config.treasury @ SwapError::WrongHolding)]
    pub treasury: UncheckedAccount<'info>,
    pub base_mint: Account<'info, TokenMint>,
    #[account(constraint = quote_mint.key() != base_mint.key() @ SwapError::SameMint)]
    pub quote_mint: Account<'info, TokenMint>,
    #[account(
        init,
        payer = payer,
        space = Pool::LEN,
        seeds = [POOL_SEED, base_mint.key().as_ref(), quote_mint.key().as_ref(), &args.lp_fee_bps.to_le_bytes(), args.hook_program.as_ref()],
        bump
    )]
    pub pool: Account<'info, Pool>,
    /// CHECK: the LP mint PDA, created here through the token program.
    #[account(mut, seeds = [LP_SEED, pool.key().as_ref()], bump)]
    pub lp_mint: UncheckedAccount<'info>,
    /// CHECK: the pool's base holding, created here.
    #[account(mut)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: the pool's quote holding, created here.
    #[account(mut)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: the authority's base holding (checked by the token program on transfer).
    #[account(mut)]
    pub authority_base: UncheckedAccount<'info>,
    /// CHECK: the authority's quote holding.
    #[account(mut)]
    pub authority_quote: UncheckedAccount<'info>,
    /// CHECK: the authority's LP holding, created here.
    #[account(mut)]
    pub authority_lp: UncheckedAccount<'info>,
    /// CHECK: the pool's hook program (checked against the args).
    pub hook_program: Option<UncheckedAccount<'info>>,
    /// The hook program's own hook authority, when the hook itself creates the pool: the initialize
    /// callbacks are then skipped (a program cannot be re-entered through its own CPI).
    pub hook_caller: Option<Signer<'info>>,
    /// CHECK: with a hook, this program's signer of its callbacks, `["hook-authority",
    /// hook_program]` (checked in the handler); absent (this program's id) without.
    pub hook_signer: Option<UncheckedAccount<'info>>,
    /// CHECK: the token program.
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Signer seeds of a pool.
pub struct PoolSeeds {
    base: Pubkey,
    quote: Pubkey,
    fee: [u8; 2],
    hook: Pubkey,
    bump: [u8; 1],
}

impl PoolSeeds {
    /// The seeds of `pool`.
    pub fn of(pool: &Pool) -> Self {
        Self {
            base: pool.base_mint,
            quote: pool.quote_mint,
            fee: pool.lp_fee_bps.to_le_bytes(),
            hook: Pool::hook_key(pool.hook_program),
            bump: [pool.bump],
        }
    }

    /// As signer seeds.
    pub fn seeds(&self) -> [&[u8]; 6] {
        [
            POOL_SEED,
            self.base.as_ref(),
            self.quote.as_ref(),
            &self.fee,
            self.hook.as_ref(),
            &self.bump,
        ]
    }
}

/// Checks `hook_caller` is `["hook-authority"]` of `hook_program` at `bump`.
pub fn check_hook_caller(hook_caller: &Pubkey, hook_program: &Pubkey, bump: u8) -> Result<()> {
    let expected = Pubkey::create_program_address(&[HOOK_AUTHORITY_SEED, &[bump]], hook_program)
        .map_err(|_| SwapError::BadHookCaller)?;
    require_keys_eq!(*hook_caller, expected, SwapError::BadHookCaller);
    Ok(())
}

/// `create_pool`.
pub fn process_create_pool<'info>(
    ctx: Context<'info, CreatePool<'info>>,
    args: CreatePoolArgs,
) -> Result<()> {
    let clock = Clock::get()?;
    let config = &mut ctx.accounts.config;
    require!(!config.paused, SwapError::Paused);
    require!(args.lp_fee_bps <= MAX_LP_FEE_BPS, SwapError::FeeTooHigh);
    require!(
        args.hook_flags & !pool_flags::ALL == 0,
        SwapError::InvalidHookFlags
    );
    require!(
        args.hook_data.len() <= MAX_HOOK_DATA,
        SwapError::HookDataTooLong
    );
    require!(args.base_amount > 0, SwapError::ZeroAmount);
    let curve = args.virtual_base > 0 || args.virtual_quote > 0;
    let hook_program_opt = if args.hook_program == Pubkey::default() {
        None
    } else {
        Some(args.hook_program)
    };
    // The pool's hook and this program's signer of its callbacks (one per hook program), whose
    // bump the pool keeps.
    let hook_signer_bump = match hook_program_opt {
        Some(program) => {
            require!(program != crate::ID, SwapError::InvalidHookFlags);
            Pool::hook_signer(&program).1
        }
        None => {
            require!(args.hook_flags == 0, SwapError::InvalidHookFlags);
            0
        }
    };
    // The hook program creating its own pool proves it by signing with its hook authority.
    let by_hook = match (&ctx.accounts.hook_caller, hook_program_opt) {
        (Some(caller), Some(program)) => {
            let (expected, _) = bordrless_hook::hook_authority(&program);
            require_keys_eq!(caller.key(), expected, SwapError::BadHookCaller);
            true
        }
        (Some(_), None) => return err!(SwapError::BadHookCaller),
        _ => false,
    };
    if curve {
        require!(by_hook, SwapError::CurveNeedsHook);
        require!(args.quote_amount == 0, SwapError::CurveQuoteNotZero);
    } else {
        require!(args.quote_amount > 0, SwapError::ZeroAmount);
    }
    // The protocol fee model the pool keeps for life: a launch pool (a curve the launchpad
    // creates as its own hook, its hook authority signing) pays the config's share of what its
    // hooks cut; every other pool, a curve another hook program creates included, the flat rate.
    // Any program can sign with its own ["hook-authority"], so "a curve by a hook" alone would
    // let anyone open a pool whose hook cuts nothing and so pays nothing, for life.
    let launch_pool = curve && by_hook && hook_program_opt == Some(LAUNCHPAD_ID);
    let (fee_model, protocol_fee_bps, protocol_share_bps) = if launch_pool {
        (FEE_MODEL_SHARE, 0, config.launch_protocol_share_bps)
    } else {
        (FEE_MODEL_FLAT, config.protocol_fee_bps, 0)
    };

    let token = TokenAccounts {
        program: ctx.accounts.token_program.to_account_info(),
        event_authority: ctx.accounts.token_event_authority.to_account_info(),
    };
    token.check()?;

    // The creation fee.
    if config.pool_creation_fee_lamports > 0 {
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                system_program::Transfer {
                    from: ctx.accounts.payer.to_account_info(),
                    to: ctx.accounts.treasury.to_account_info(),
                },
            ),
            config.pool_creation_fee_lamports,
        )?;
    }

    let pool_key = ctx.accounts.pool.key();
    let (base_extras, quote_extras, pool_extras) = split_extras(
        ctx.remaining_accounts,
        args.base_hook_accounts,
        args.quote_hook_accounts,
    )?;
    let call = PoolHookCall::of(
        hook_program_opt,
        hook_signer_bump,
        ctx.accounts
            .hook_program
            .as_ref()
            .map(|a| a.to_account_info()),
        ctx.accounts
            .hook_signer
            .as_ref()
            .map(|a| a.to_account_info()),
        [
            ctx.accounts.pool.to_account_info(),
            ctx.accounts.base_mint.to_account_info(),
            ctx.accounts.quote_mint.to_account_info(),
            ctx.accounts.authority.to_account_info(),
        ],
        pool_extras,
    )?;

    // The pool's fields, before anything is signed with its seeds.
    {
        let pool = &mut ctx.accounts.pool;
        pool.version = VERSION;
        pool.bump = ctx.bumps.pool;
        pool.lp_mint_bump = ctx.bumps.lp_mint;
        pool.base_mint = ctx.accounts.base_mint.key();
        pool.quote_mint = ctx.accounts.quote_mint.key();
        pool.lp_mint = ctx.accounts.lp_mint.key();
        pool.base_vault = ctx.accounts.base_vault.key();
        pool.quote_vault = ctx.accounts.quote_vault.key();
        pool.hook_program = hook_program_opt;
        pool.hook_flags = args.hook_flags;
        pool.lp_fee_bps = args.lp_fee_bps;
        pool.protocol_fee_bps = protocol_fee_bps;
        pool.virtual_base = args.virtual_base;
        pool.virtual_quote = args.virtual_quote;
        pool.curve = curve;
        pool.creator = ctx.accounts.authority.key();
        pool.created_at = clock.unix_timestamp;
        pool.hook_signer_bump = hook_signer_bump;
        pool.fee_model = fee_model;
        pool.protocol_share_bps = protocol_share_bps;
        pool.reserved = [0; 60];
    }
    let seeds = PoolSeeds::of(&ctx.accounts.pool);
    let pool_seeds = seeds.seeds();
    let lp_bump = [ctx.bumps.lp_mint];
    let lp_seeds: [&[u8]; 3] = [LP_SEED, pool_key.as_ref(), &lp_bump];

    // The LP mint, the vaults and the authority's LP holding.
    let name = format!(
        "{LP_NAME} {}/{}",
        ctx.accounts.base_mint.symbol, ctx.accounts.quote_mint.symbol
    );
    token.create_mint(
        &ctx.accounts.payer.to_account_info(),
        &ctx.accounts.lp_mint.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        CreateMintArgs {
            decimals: policy::LP_DECIMALS,
            name: name.chars().take(32).collect(),
            symbol: LP_SYMBOL.to_string(),
            uri: String::new(),
            max_supply: 0,
            mint_authority: Some(pool_key),
            freeze_authority: None,
            hook_program: None,
            hook_flags: 0,
            hook_authority: None,
            metadata_authority: None,
        },
        &[&lp_seeds],
    )?;
    let pool_info = ctx.accounts.pool.to_account_info();
    let payer = ctx.accounts.payer.to_account_info();
    let system = ctx.accounts.system_program.to_account_info();
    token.create_holding(
        &payer,
        &ctx.accounts.base_mint.to_account_info(),
        &pool_info,
        &ctx.accounts.base_vault.to_account_info(),
        &system,
    )?;
    token.create_holding(
        &payer,
        &ctx.accounts.quote_mint.to_account_info(),
        &pool_info,
        &ctx.accounts.quote_vault.to_account_info(),
        &system,
    )?;
    token.create_holding(
        &payer,
        &ctx.accounts.lp_mint.to_account_info(),
        &ctx.accounts.authority.to_account_info(),
        &ctx.accounts.authority_lp.to_account_info(),
        &system,
    )?;

    let mut hook_args = PoolHookArgs {
        op: PoolOp::Initialize,
        phase: Phase::Before,
        pool: pool_key,
        base_mint: ctx.accounts.base_mint.key(),
        quote_mint: ctx.accounts.quote_mint.key(),
        actor: ctx.accounts.authority.key(),
        recipient: Pubkey::default(),
        direction: 0,
        amount_in: args.base_amount,
        amount_out: args.quote_amount,
        base_reserve: 0,
        quote_reserve: 0,
        virtual_base: args.virtual_base,
        virtual_quote: args.virtual_quote,
        lp_fee_bps: args.lp_fee_bps,
        protocol_fee_bps,
        swap_count: 0,
        created_at: clock.unix_timestamp,
        lp_amount: 0,
        hook_data: args.hook_data.clone(),
    };
    if let Some(call) = &call {
        if !by_hook && args.hook_flags & pool_flags::BEFORE_INITIALIZE != 0 {
            call.invoke(discriminators::BEFORE_INITIALIZE, &hook_args, Allowed::NONE)?;
        }
    }

    // The deposits, measured.
    let authority = ctx.accounts.authority.to_account_info();
    let base_before = balance(&ctx.accounts.base_vault)?;
    token.transfer(
        &authority,
        &ctx.accounts.authority_base.to_account_info(),
        &ctx.accounts.base_vault.to_account_info(),
        &ctx.accounts.base_mint.to_account_info(),
        &TokenSide::of(base_extras, ctx.accounts.base_mint.hook_program.is_some()),
        args.base_amount,
        &[],
    )?;
    let base_received = balance(&ctx.accounts.base_vault)?
        .checked_sub(base_before)
        .ok_or(SwapError::MathOverflow)?;
    require!(base_received > 0, SwapError::NothingReceived);
    let mut quote_received = 0u64;
    if args.quote_amount > 0 {
        let quote_before = balance(&ctx.accounts.quote_vault)?;
        token.transfer(
            &authority,
            &ctx.accounts.authority_quote.to_account_info(),
            &ctx.accounts.quote_vault.to_account_info(),
            &ctx.accounts.quote_mint.to_account_info(),
            &TokenSide::of(quote_extras, ctx.accounts.quote_mint.hook_program.is_some()),
            args.quote_amount,
            &[],
        )?;
        quote_received = balance(&ctx.accounts.quote_vault)?
            .checked_sub(quote_before)
            .ok_or(SwapError::MathOverflow)?;
        require!(quote_received > 0, SwapError::NothingReceived);
    }

    // LP: an ordinary pool mints the first shares now; a curve mints them when finalized.
    let mut lp_minted = 0u64;
    let mut lp_supply = 0u64;
    if !curve {
        let (to_depositor, total) =
            initial_lp(base_received, quote_received).ok_or(SwapError::DepositTooSmall)?;
        token.mint_to(
            &pool_info,
            &ctx.accounts.lp_mint.to_account_info(),
            &ctx.accounts.authority_lp.to_account_info(),
            to_depositor,
            &[&pool_seeds],
        )?;
        lp_minted = to_depositor;
        lp_supply = total;
    }
    {
        let pool = &mut ctx.accounts.pool;
        pool.base_reserve = base_received;
        pool.quote_reserve = quote_received;
        pool.lp_supply = lp_supply;
    }
    config.pools_created = config
        .pools_created
        .checked_add(1)
        .ok_or(SwapError::MathOverflow)?;

    if let Some(call) = &call {
        if !by_hook && args.hook_flags & pool_flags::AFTER_INITIALIZE != 0 {
            ctx.accounts.pool.exit(&crate::ID)?;
            hook_args.phase = Phase::After;
            hook_args.base_reserve = base_received;
            hook_args.quote_reserve = quote_received;
            hook_args.lp_amount = lp_minted;
            call.invoke(discriminators::AFTER_INITIALIZE, &hook_args, Allowed::NONE)?;
        }
    }

    emit_cpi!(PoolCreated {
        pool: pool_key,
        payer: ctx.accounts.payer.key(),
        creator: ctx.accounts.authority.key(),
        base_mint: ctx.accounts.base_mint.key(),
        quote_mint: ctx.accounts.quote_mint.key(),
        lp_mint: ctx.accounts.lp_mint.key(),
        base_vault: ctx.accounts.base_vault.key(),
        quote_vault: ctx.accounts.quote_vault.key(),
        lp_fee_bps: args.lp_fee_bps,
        protocol_fee_bps,
        fee_model,
        protocol_share_bps,
        hook_program: hook_program_opt,
        hook_flags: args.hook_flags,
        virtual_base: args.virtual_base,
        virtual_quote: args.virtual_quote,
        base_reserve: base_received,
        quote_reserve: quote_received,
        lp_supply,
        lp_minted,
        curve,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}

/// Accounts of `finalize_curve`.
#[event_cpi]
#[derive(Accounts)]
pub struct FinalizeCurve<'info> {
    /// The pool hook program's hook authority.
    pub hook_caller: Signer<'info>,
    #[account(mut, constraint = pool.curve @ SwapError::NotCurve)]
    pub pool: Account<'info, Pool>,
    /// CHECK: the pool's base vault (address-checked).
    #[account(address = pool.base_vault @ SwapError::WrongVault)]
    pub base_vault: UncheckedAccount<'info>,
    /// CHECK: the pool's quote vault (address-checked).
    #[account(address = pool.quote_vault @ SwapError::WrongVault)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: the LP mint (address-checked).
    #[account(mut, address = pool.lp_mint @ SwapError::WrongHolding)]
    pub lp_mint: UncheckedAccount<'info>,
    /// CHECK: receives the first LP (a holding of the LP mint, checked by the token program).
    #[account(mut)]
    pub lp_recipient: UncheckedAccount<'info>,
    /// CHECK: the token program.
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    pub token_event_authority: UncheckedAccount<'info>,
}

/// `finalize_curve`.
pub fn process_finalize_curve(ctx: Context<FinalizeCurve>, hook_caller_bump: u8) -> Result<()> {
    let clock = Clock::get()?;
    let hook_program = ctx
        .accounts
        .pool
        .hook_program
        .ok_or(SwapError::BadHookCaller)?;
    check_hook_caller(
        ctx.accounts.hook_caller.key,
        &hook_program,
        hook_caller_bump,
    )?;
    let token = TokenAccounts {
        program: ctx.accounts.token_program.to_account_info(),
        event_authority: ctx.accounts.token_event_authority.to_account_info(),
    };
    token.check()?;
    let base_total = balance(&ctx.accounts.base_vault)?;
    let quote_total = balance(&ctx.accounts.quote_vault)?;
    let pool_info = ctx.accounts.pool.to_account_info();
    let seeds = PoolSeeds::of(&ctx.accounts.pool);
    let pool_seeds = seeds.seeds();
    let pool = &mut ctx.accounts.pool;
    // Protocol fees are only ever in the quote token: the whole base vault is reserve.
    let base_reserve = base_total;
    let quote_reserve = quote_total
        .checked_sub(pool.protocol_fees_quote)
        .ok_or(SwapError::MathOverflow)?;
    let (to_recipient, total) =
        initial_lp(base_reserve, quote_reserve).ok_or(SwapError::DepositTooSmall)?;
    token.mint_to(
        &pool_info,
        &ctx.accounts.lp_mint.to_account_info(),
        &ctx.accounts.lp_recipient.to_account_info(),
        to_recipient,
        &[&pool_seeds],
    )?;
    pool.base_reserve = base_reserve;
    pool.quote_reserve = quote_reserve;
    pool.virtual_base = 0;
    pool.virtual_quote = 0;
    pool.lp_supply = total;
    pool.curve = false;
    emit_cpi!(CurveFinalized {
        pool: pool.key(),
        base_reserve,
        quote_reserve,
        lp_supply: total,
        lp_minted: to_recipient,
        lp_recipient: ctx.accounts.lp_recipient.key(),
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}

/// Accounts of `collect_protocol_fees`. The remaining accounts are the quote mint's token-hook
/// slice (its program, the token program's signer for it, its extras), exactly
/// `quote_hook_accounts` of them, as a swap passes them.
#[event_cpi]
#[derive(Accounts)]
pub struct CollectProtocolFees<'info> {
    /// The admin.
    pub admin: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ SwapError::NotAdmin)]
    pub config: Account<'info, Config>,
    #[account(mut)]
    pub pool: Account<'info, Pool>,
    /// CHECK: address-checked.
    #[account(address = pool.quote_mint @ SwapError::WrongHolding)]
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = pool.quote_vault @ SwapError::WrongVault)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: the fee collector's quote holding (owner- and mint-checked in the handler).
    #[account(mut)]
    pub collector_quote: UncheckedAccount<'info>,
    /// CHECK: the token program.
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority.
    pub token_event_authority: UncheckedAccount<'info>,
}

/// `collect_protocol_fees`: moves the pool's `protocol_fees_quote` from its quote vault to the
/// holding of whoever the config names as fee collector now. Protocol fees are only ever in the
/// quote token, so no other token (a launched one in particular) passes through here, and the
/// admin may change the fee collector freely.
pub fn process_collect_protocol_fees<'info>(
    ctx: Context<'info, CollectProtocolFees<'info>>,
    quote_hook_accounts: u8,
) -> Result<()> {
    let token = TokenAccounts {
        program: ctx.accounts.token_program.to_account_info(),
        event_authority: ctx.accounts.token_event_authority.to_account_info(),
    };
    token.check()?;
    require!(
        ctx.remaining_accounts.len() == usize::from(quote_hook_accounts),
        SwapError::AccountCounts
    );
    let collector = read_holding(&ctx.accounts.collector_quote)?;
    require_keys_eq!(
        collector.owner,
        ctx.accounts.config.fee_collector,
        SwapError::WrongHolding
    );
    require_keys_eq!(
        collector.mint,
        ctx.accounts.pool.quote_mint,
        SwapError::WrongHolding
    );
    let quote_amount = ctx.accounts.pool.protocol_fees_quote;
    if quote_amount > 0 {
        let quote_hook = bordrless_token::client::read_mint(&ctx.accounts.quote_mint)?.hook_program;
        let extras = ctx.remaining_accounts;
        let pool_info = ctx.accounts.pool.to_account_info();
        let seeds = PoolSeeds::of(&ctx.accounts.pool);
        let pool_seeds = seeds.seeds();
        token.transfer(
            &pool_info,
            &ctx.accounts.quote_vault.to_account_info(),
            &ctx.accounts.collector_quote.to_account_info(),
            &ctx.accounts.quote_mint.to_account_info(),
            &TokenSide::of(extras, quote_hook.is_some()),
            quote_amount,
            &[&pool_seeds],
        )?;
    }
    let pool = &mut ctx.accounts.pool;
    pool.protocol_fees_quote = 0;
    emit_cpi!(ProtocolFeesCollected {
        pool: pool.key(),
        quote_amount,
        collector: ctx.accounts.config.fee_collector,
        ts: Clock::get()?.unix_timestamp
    });
    Ok(())
}

/// Accounts of `collect_protocol_fees_sol`: the pool, the fee collector the config names (a wallet,
/// paid in SOL), and the bridge's `unwrap_sol` accounts for the pool as its user. Anyone may send it:
/// the fees can only go to the configured collector.
#[event_cpi]
#[derive(Accounts)]
pub struct CollectProtocolFeesSol<'info> {
    /// Whoever sends it; it pays the transaction fee and receives nothing.
    pub cranker: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, constraint = pool.quote_mint == BRIDGED_SOL_MINT @ SwapError::NotBridgedSol)]
    pub pool: Account<'info, Pool>,
    /// CHECK: address-checked: the pool's quote vault, its holding of bridged SOL.
    #[account(mut, address = pool.quote_vault @ SwapError::WrongVault)]
    pub quote_vault: UncheckedAccount<'info>,
    /// CHECK: address-checked: the fee collector the config names; it receives the SOL.
    #[account(mut, address = config.fee_collector @ SwapError::WrongHolding)]
    pub collector: UncheckedAccount<'info>,
    /// CHECK: address-checked: the bridge.
    #[account(address = BRIDGE_ID)]
    pub bridge_program: UncheckedAccount<'info>,
    /// CHECK: the bridge's config (the bridge checks it).
    pub bridge_config: UncheckedAccount<'info>,
    /// CHECK: the SOL wrapper (the bridge checks it).
    #[account(mut)]
    pub sol_wrapper: UncheckedAccount<'info>,
    /// CHECK: the bridge's SOL vault (the bridge checks it).
    #[account(mut)]
    pub sol_vault: UncheckedAccount<'info>,
    /// CHECK: address-checked: bridged SOL, the pool's quote mint.
    #[account(mut, address = pool.quote_mint @ SwapError::WrongHolding)]
    pub bridged_sol_mint: UncheckedAccount<'info>,
    /// CHECK: the bridge's event authority (the bridge checks it).
    pub bridge_event_authority: UncheckedAccount<'info>,
    /// CHECK: address-checked: the token program.
    #[account(address = bordrless_token::ID)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: the token program's event authority (the token program checks it).
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `collect_protocol_fees_sol`: the pool's `protocol_fees_quote` (bridged SOL in its quote vault),
/// unwrapped by the bridge with the pool as its user and paid to the fee collector as SOL. The
/// pool's own lamports are left as they were.
pub fn process_collect_protocol_fees_sol(ctx: Context<CollectProtocolFeesSol>) -> Result<()> {
    let amount = ctx.accounts.pool.protocol_fees_quote;
    if amount > 0 {
        let pool_info = ctx.accounts.pool.to_account_info();
        let seeds = PoolSeeds::of(&ctx.accounts.pool);
        let pool_seeds = seeds.seeds();
        // The bridge's `unwrap_sol` with the pool as its user, from the accounts passed (the
        // bridge checks every one of them).
        let mut data = UNWRAP_SOL_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&amount.to_le_bytes());
        let ro = |info: &AccountInfo| AccountMeta::new_readonly(info.key(), false);
        let rw = |info: &AccountInfo| AccountMeta::new(info.key(), false);
        let ix = anchor_lang::solana_program::instruction::Instruction {
            program_id: BRIDGE_ID,
            accounts: vec![
                AccountMeta::new(pool_info.key(), true),
                ro(&ctx.accounts.bridge_config),
                rw(&ctx.accounts.sol_wrapper),
                rw(&ctx.accounts.sol_vault),
                rw(&ctx.accounts.bridged_sol_mint),
                rw(&ctx.accounts.quote_vault),
                ro(&ctx.accounts.token_program),
                ro(&ctx.accounts.token_event_authority),
                ro(&ctx.accounts.system_program),
                ro(&ctx.accounts.bridge_event_authority),
                ro(&ctx.accounts.bridge_program),
            ],
            data,
        };
        anchor_lang::solana_program::program::invoke_signed(
            &ix,
            &[
                pool_info.clone(),
                ctx.accounts.bridge_config.to_account_info(),
                ctx.accounts.sol_wrapper.to_account_info(),
                ctx.accounts.sol_vault.to_account_info(),
                ctx.accounts.bridged_sol_mint.to_account_info(),
                ctx.accounts.quote_vault.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
                ctx.accounts.token_event_authority.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
                ctx.accounts.bridge_event_authority.to_account_info(),
                ctx.accounts.bridge_program.to_account_info(),
            ],
            &[&pool_seeds],
        )?;
        // The SOL landed on the pool's account, which this program owns: hand it on.
        let collector = ctx.accounts.collector.to_account_info();
        let pool_lamports = pool_info.lamports();
        **pool_info.try_borrow_mut_lamports()? = pool_lamports
            .checked_sub(amount)
            .ok_or(SwapError::MathOverflow)?;
        let collector_lamports = collector.lamports();
        **collector.try_borrow_mut_lamports()? = collector_lamports
            .checked_add(amount)
            .ok_or(SwapError::MathOverflow)?;
    }
    let pool = &mut ctx.accounts.pool;
    pool.protocol_fees_quote = 0;
    emit_cpi!(ProtocolFeesCollected {
        pool: pool.key(),
        quote_amount: amount,
        collector: ctx.accounts.config.fee_collector,
        ts: Clock::get()?.unix_timestamp
    });
    Ok(())
}
