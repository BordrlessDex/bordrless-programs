//! Registering wrappers.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::system_program;
use bordrless_token::client as token_client;
use bordrless_token::instructions::CreateMintArgs;

use crate::constants::*;
use crate::error::BridgeError;
use crate::events::WrapperRegistered;
use crate::spl;
use crate::state::*;

/// Arguments of `register`: the wrapped mint's metadata, which should mirror the underlying's
/// (the admin can correct it later).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct RegisterArgs {
    /// Name.
    pub name: String,
    /// Symbol.
    pub symbol: String,
    /// Metadata URI.
    pub uri: String,
}

/// Accounts of `register`.
#[event_cpi]
#[derive(Accounts)]
pub struct Register<'info> {
    /// Pays the rent.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    /// CHECK: an SPL Token or Token-2022 mint (owner-checked in the handler).
    #[account(constraint = underlying_mint.key() != NATIVE_MINT @ BridgeError::NativeWrapper)]
    pub underlying_mint: UncheckedAccount<'info>,
    #[account(init, payer = payer, space = Wrapper::LEN, seeds = [WRAPPER_SEED, underlying_mint.key().as_ref()], bump)]
    pub wrapper: Account<'info, Wrapper>,
    /// CHECK: the wrapped BTS mint PDA, created here through the token program.
    #[account(mut, seeds = [WRAPPED_SEED, underlying_mint.key().as_ref()], bump)]
    pub wrapped_mint: UncheckedAccount<'info>,
    /// CHECK: the wrapper's associated token account for the underlying, created here.
    #[account(mut)]
    pub vault: UncheckedAccount<'info>,
    /// CHECK: the underlying's token program (must own the mint).
    pub underlying_program: UncheckedAccount<'info>,
    /// CHECK: the associated token account program.
    #[account(address = ATA_PROGRAM_ID)]
    pub ata_program: UncheckedAccount<'info>,
    /// CHECK: the Bordrless token program.
    #[account(address = bordrless_token::ID)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: its event authority.
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[allow(clippy::too_many_arguments)]
fn create_wrapped_mint<'info>(
    payer: &AccountInfo<'info>,
    wrapped_mint: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    token_event_authority: &AccountInfo<'info>,
    underlying: &Pubkey,
    bump: u8,
    args: CreateMintArgs,
) -> Result<()> {
    require_keys_eq!(
        *token_event_authority.key,
        token_client::event_authority(),
        BridgeError::WrongTokenProgram
    );
    let ix = token_client::create_mint(*payer.key, *wrapped_mint.key, args);
    let seeds: &[&[u8]] = &[WRAPPED_SEED, underlying.as_ref(), &[bump]];
    invoke_signed(
        &ix,
        &[
            payer.clone(),
            wrapped_mint.clone(),
            system_program.clone(),
            token_event_authority.clone(),
            token_program.clone(),
        ],
        &[seeds],
    )
    .map_err(Into::into)
}

/// `register`.
pub fn process_register(ctx: Context<Register>, args: RegisterArgs) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    require!(
        !args.name.is_empty()
            && args.name.len() <= 32
            && !args.symbol.is_empty()
            && args.symbol.len() <= 10
            && args.uri.len() <= 200,
        BridgeError::InvalidMetadata
    );
    let program = ctx.accounts.underlying_program.key();
    require!(
        spl::is_token_program(&program),
        BridgeError::WrongTokenProgram
    );
    require_keys_eq!(
        *ctx.accounts.underlying_mint.owner,
        program,
        BridgeError::WrongTokenProgram
    );
    let decimals = spl::mint_decimals(&ctx.accounts.underlying_mint)?;
    let underlying = ctx.accounts.underlying_mint.key();
    let wrapper_key = ctx.accounts.wrapper.key();
    let expected_vault = spl::associated_token_address(&wrapper_key, &underlying, &program);
    require_keys_eq!(
        ctx.accounts.vault.key(),
        expected_vault,
        BridgeError::WrongVault
    );

    spl::create_ata_idempotent(
        &ctx.accounts.payer.to_account_info(),
        &ctx.accounts.vault.to_account_info(),
        &ctx.accounts.wrapper.to_account_info(),
        &ctx.accounts.underlying_mint.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        &ctx.accounts.underlying_program.to_account_info(),
        &ctx.accounts.ata_program.to_account_info(),
    )?;
    create_wrapped_mint(
        &ctx.accounts.payer.to_account_info(),
        &ctx.accounts.wrapped_mint.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        &ctx.accounts.token_program.to_account_info(),
        &ctx.accounts.token_event_authority.to_account_info(),
        &underlying,
        ctx.bumps.wrapped_mint,
        CreateMintArgs {
            decimals,
            name: args.name.clone(),
            symbol: args.symbol.clone(),
            uri: args.uri.clone(),
            max_supply: 0,
            mint_authority: Some(wrapper_key),
            freeze_authority: None,
            hook_program: None,
            hook_flags: 0,
            hook_authority: None,
            metadata_authority: Some(ctx.accounts.config.admin),
        },
    )?;

    let wrapper = &mut ctx.accounts.wrapper;
    wrapper.version = VERSION;
    wrapper.bump = ctx.bumps.wrapper;
    wrapper.wrapped_mint_bump = ctx.bumps.wrapped_mint;
    wrapper.native = false;
    wrapper.underlying_mint = underlying;
    wrapper.underlying_program = program;
    wrapper.wrapped_mint = ctx.accounts.wrapped_mint.key();
    wrapper.vault = expected_vault;
    wrapper.decimals = decimals;
    wrapper.total_wrapped = 0;
    wrapper.registered_at = now;
    wrapper.registrar = ctx.accounts.payer.key();
    wrapper.reserved = [0; 32];
    let config = &mut ctx.accounts.config;
    config.wrappers = config
        .wrappers
        .checked_add(1)
        .ok_or(BridgeError::MathOverflow)?;
    emit_cpi!(WrapperRegistered {
        wrapper: wrapper_key,
        underlying_mint: underlying,
        underlying_program: program,
        wrapped_mint: ctx.accounts.wrapped_mint.key(),
        vault: expected_vault,
        decimals,
        native: false,
        name: args.name,
        symbol: args.symbol,
        uri: args.uri,
        registrar: ctx.accounts.payer.key(),
        ts: now,
    });
    Ok(())
}

/// Accounts of `register_sol`.
#[event_cpi]
#[derive(Accounts)]
pub struct RegisterSol<'info> {
    /// Pays the rent.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(init, payer = payer, space = Wrapper::LEN, seeds = [WRAPPER_SEED, NATIVE_MINT.as_ref()], bump)]
    pub wrapper: Account<'info, Wrapper>,
    /// CHECK: the wrapped SOL mint PDA, created here.
    #[account(mut, seeds = [WRAPPED_SEED, NATIVE_MINT.as_ref()], bump)]
    pub wrapped_mint: UncheckedAccount<'info>,
    /// CHECK: the SOL vault, a system account made rent-exempt here.
    #[account(mut, seeds = [SOL_VAULT_SEED], bump = SOL_VAULT_BUMP)]
    pub sol_vault: UncheckedAccount<'info>,
    /// CHECK: the Bordrless token program.
    #[account(address = bordrless_token::ID)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: its event authority.
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `register_sol`.
pub fn process_register_sol(ctx: Context<RegisterSol>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    // The vault must exist as a rent-exempt system account so it can hold lamports on its own.
    let rent = Rent::get()?.minimum_balance(0);
    let have = ctx.accounts.sol_vault.lamports();
    if have < rent {
        system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                system_program::Transfer {
                    from: ctx.accounts.payer.to_account_info(),
                    to: ctx.accounts.sol_vault.to_account_info(),
                },
            ),
            rent - have,
        )?;
    }
    let wrapper_key = ctx.accounts.wrapper.key();
    create_wrapped_mint(
        &ctx.accounts.payer.to_account_info(),
        &ctx.accounts.wrapped_mint.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        &ctx.accounts.token_program.to_account_info(),
        &ctx.accounts.token_event_authority.to_account_info(),
        &NATIVE_MINT,
        ctx.bumps.wrapped_mint,
        CreateMintArgs {
            decimals: SOL_DECIMALS,
            name: SOL_NAME.to_string(),
            symbol: SOL_SYMBOL.to_string(),
            uri: String::new(),
            max_supply: 0,
            mint_authority: Some(wrapper_key),
            freeze_authority: None,
            hook_program: None,
            hook_flags: 0,
            hook_authority: None,
            metadata_authority: Some(ctx.accounts.config.admin),
        },
    )?;
    let wrapper = &mut ctx.accounts.wrapper;
    wrapper.version = VERSION;
    wrapper.bump = ctx.bumps.wrapper;
    wrapper.wrapped_mint_bump = ctx.bumps.wrapped_mint;
    wrapper.native = true;
    wrapper.underlying_mint = NATIVE_MINT;
    wrapper.underlying_program = system_program::ID;
    wrapper.wrapped_mint = ctx.accounts.wrapped_mint.key();
    wrapper.vault = ctx.accounts.sol_vault.key();
    wrapper.decimals = SOL_DECIMALS;
    wrapper.total_wrapped = 0;
    wrapper.registered_at = now;
    wrapper.registrar = ctx.accounts.payer.key();
    wrapper.reserved = [0; 32];
    let config = &mut ctx.accounts.config;
    config.wrappers = config
        .wrappers
        .checked_add(1)
        .ok_or(BridgeError::MathOverflow)?;
    emit_cpi!(WrapperRegistered {
        wrapper: wrapper_key,
        underlying_mint: NATIVE_MINT,
        underlying_program: system_program::ID,
        wrapped_mint: ctx.accounts.wrapped_mint.key(),
        vault: ctx.accounts.sol_vault.key(),
        decimals: SOL_DECIMALS,
        native: true,
        name: SOL_NAME.to_string(),
        symbol: SOL_SYMBOL.to_string(),
        uri: String::new(),
        registrar: ctx.accounts.payer.key(),
        ts: now,
    });
    Ok(())
}
