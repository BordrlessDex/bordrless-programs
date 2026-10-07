//! Mint creation, minting, authorities, hook and metadata.

use anchor_lang::prelude::*;
use bordrless_hook::{discriminators, token_flags, Allowed, Phase, TokenHookArgs, TokenOp};

use crate::constants::*;
use crate::error::TokenError;
use crate::events::*;
use crate::hooks::HookCall;
use crate::state::*;

/// Arguments of `create_mint`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct CreateMintArgs {
    /// Decimals (at most 12).
    pub decimals: u8,
    /// Name (at most 32 bytes).
    pub name: String,
    /// Symbol (at most 10 bytes).
    pub symbol: String,
    /// Metadata URI (at most 200 bytes).
    pub uri: String,
    /// Largest supply allowed; 0 for unlimited.
    pub max_supply: u64,
    /// May mint.
    pub mint_authority: Option<Pubkey>,
    /// May freeze.
    pub freeze_authority: Option<Pubkey>,
    /// The hook program.
    pub hook_program: Option<Pubkey>,
    /// Its flags (zero without a hook program).
    pub hook_flags: u16,
    /// May change the hook later.
    pub hook_authority: Option<Pubkey>,
    /// May change the metadata later.
    pub metadata_authority: Option<Pubkey>,
}

/// Accounts of `create_mint`.
#[event_cpi]
#[derive(Accounts)]
pub struct CreateMint<'info> {
    /// Pays the rent.
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The new mint, which signs.
    #[account(init, payer = payer, space = Mint::LEN)]
    pub mint: Account<'info, Mint>,
    pub system_program: Program<'info, System>,
}

pub(crate) fn check_metadata(name: &str, symbol: &str, uri: &str) -> Result<()> {
    require!(
        !name.is_empty() && name.len() <= MAX_NAME,
        TokenError::InvalidMetadata
    );
    require!(
        !symbol.is_empty() && symbol.len() <= MAX_SYMBOL,
        TokenError::InvalidMetadata
    );
    require!(uri.len() <= MAX_URI, TokenError::InvalidMetadata);
    Ok(())
}

pub(crate) fn check_hook(hook_program: Option<Pubkey>, flags: u16) -> Result<()> {
    require!(flags & !token_flags::ALL == 0, TokenError::InvalidHookFlags);
    if hook_program.is_none() {
        require!(flags == 0, TokenError::InvalidHookFlags);
    }
    if let Some(p) = hook_program {
        require!(p != crate::ID, TokenError::InvalidHookFlags);
    }
    Ok(())
}

/// The bump of this program's signer of `hook_program`'s callbacks (0 without a hook).
fn hook_signer_bump(hook_program: Option<Pubkey>) -> u8 {
    hook_program.map_or(0, |p| Mint::hook_signer(&p).1)
}

/// `create_mint`.
pub fn process_create_mint(ctx: Context<CreateMint>, args: CreateMintArgs) -> Result<()> {
    require!(args.decimals <= MAX_DECIMALS, TokenError::InvalidDecimals);
    check_metadata(&args.name, &args.symbol, &args.uri)?;
    check_hook(args.hook_program, args.hook_flags)?;
    let now = Clock::get()?.unix_timestamp;
    let mint = &mut ctx.accounts.mint;
    mint.version = VERSION;
    mint.decimals = args.decimals;
    mint.supply = 0;
    mint.max_supply = args.max_supply;
    mint.mint_authority = args.mint_authority;
    mint.freeze_authority = args.freeze_authority;
    mint.hook_authority = args.hook_authority;
    mint.metadata_authority = args.metadata_authority;
    mint.hook_program = args.hook_program;
    mint.hook_flags = args.hook_flags;
    mint.name = args.name.clone();
    mint.symbol = args.symbol.clone();
    mint.uri = args.uri.clone();
    mint.created_at = now;
    mint.creator = ctx.accounts.payer.key();
    mint.hook_signer_bump = hook_signer_bump(args.hook_program);
    mint.reserved = [0; 31];
    emit_cpi!(MintCreated {
        mint: mint.key(),
        creator: ctx.accounts.payer.key(),
        decimals: args.decimals,
        max_supply: args.max_supply,
        mint_authority: args.mint_authority,
        freeze_authority: args.freeze_authority,
        hook_authority: args.hook_authority,
        metadata_authority: args.metadata_authority,
        hook_program: args.hook_program,
        hook_flags: args.hook_flags,
        name: args.name,
        symbol: args.symbol,
        uri: args.uri,
        ts: now,
    });
    Ok(())
}

/// Accounts of `mint_to`.
#[event_cpi]
#[derive(Accounts)]
pub struct MintTo<'info> {
    /// The mint authority.
    pub authority: Signer<'info>,
    #[account(mut)]
    pub mint: Account<'info, Mint>,
    /// Receives the tokens.
    #[account(mut, has_one = mint @ TokenError::MintMismatch)]
    pub destination: Account<'info, Holding>,
    /// CHECK: the mint's hook program, when it has one (checked in the handler).
    pub hook_program: Option<UncheckedAccount<'info>>,
    /// CHECK: when the mint has a hook, this program's signer of its callbacks,
    /// `["hook-authority", hook_program]` (checked in the handler); unused (this program's id)
    /// otherwise.
    pub hook_signer: Option<UncheckedAccount<'info>>,
}

/// `mint_to`.
pub fn process_mint_to<'info>(ctx: Context<'info, MintTo<'info>>, amount: u64) -> Result<()> {
    require!(amount > 0, TokenError::ZeroAmount);
    let clock = Clock::get()?;
    let mint = &mut ctx.accounts.mint;
    let destination = &mut ctx.accounts.destination;
    require_keys_eq!(
        mint.mint_authority.ok_or(TokenError::AuthorityRevoked)?,
        ctx.accounts.authority.key(),
        TokenError::NotAuthorized
    );
    require!(!destination.frozen, TokenError::Frozen);
    let supply_post = mint
        .supply
        .checked_add(amount)
        .ok_or(TokenError::MathOverflow)?;
    require!(
        mint.max_supply == 0 || supply_post <= mint.max_supply,
        TokenError::MaxSupplyExceeded
    );

    let hook = mint.hook();
    let call = HookCall::of(
        mint,
        ctx.accounts
            .hook_program
            .as_ref()
            .map(|a| a.to_account_info()),
        ctx.accounts
            .hook_signer
            .as_ref()
            .map(|a| a.to_account_info()),
        [
            mint.to_account_info(),
            mint.to_account_info(),
            destination.to_account_info(),
            ctx.accounts.authority.to_account_info(),
        ],
        ctx.remaining_accounts,
    )?;
    let mut args = TokenHookArgs {
        op: TokenOp::Mint,
        phase: Phase::Before,
        mint: mint.key(),
        source: mint.key(),
        destination: destination.key(),
        source_owner: Pubkey::default(),
        destination_owner: destination.owner,
        authority: ctx.accounts.authority.key(),
        authority_is_delegate: false,
        amount,
        delta: 0,
        source_balance: 0,
        destination_balance: destination.amount,
        decimals: mint.decimals,
        supply: mint.supply,
        source_hook_data: [0; 64],
        destination_hook_data: destination.hook_data,
    };
    // `before_mint` may answer the destination's hook data (with `WRITES_HOOK_DATA`), nothing else.
    let mut destination_data: Option<[u8; 64]> = None;
    if let (Some(call), Some((_, flags))) = (&call, hook) {
        if flags & token_flags::BEFORE_MINT != 0 {
            let allowed = Allowed::token(TokenOp::Mint, Phase::Before, flags);
            if let Some((answer, _)) =
                call.invoke_for_answer(discriminators::BEFORE_MINT, &args, allowed)?
            {
                destination_data = answer.destination_hook_data;
            }
        }
    }
    destination.amount = destination
        .amount
        .checked_add(amount)
        .ok_or(TokenError::MathOverflow)?;
    mint.supply = supply_post;
    if let Some(data) = destination_data {
        destination.hook_data = data;
    }
    if let (Some(call), Some((_, flags))) = (&call, hook) {
        if flags & token_flags::AFTER_MINT != 0 {
            destination.exit(&crate::ID)?;
            mint.exit(&crate::ID)?;
            args.phase = Phase::After;
            args.destination_balance = destination.amount;
            args.supply = supply_post;
            args.destination_hook_data = destination.hook_data;
            call.invoke(discriminators::AFTER_MINT, &args)?;
        }
    }
    emit_cpi!(Minted {
        mint: mint.key(),
        destination: destination.key(),
        destination_owner: destination.owner,
        authority: ctx.accounts.authority.key(),
        amount,
        destination_post: destination.amount,
        supply_post,
        slot: clock.slot,
        ts: clock.unix_timestamp,
    });
    Ok(())
}

/// Accounts of `set_authority`.
#[event_cpi]
#[derive(Accounts)]
pub struct SetAuthority<'info> {
    /// The current authority of the kind being set.
    pub authority: Signer<'info>,
    #[account(mut)]
    pub mint: Account<'info, Mint>,
}

/// `set_authority`.
pub fn process_set_authority(
    ctx: Context<SetAuthority>,
    kind: AuthorityKind,
    new_authority: Option<Pubkey>,
) -> Result<()> {
    let mint = &mut ctx.accounts.mint;
    let signer = ctx.accounts.authority.key();
    let slot = match kind {
        AuthorityKind::Mint => &mut mint.mint_authority,
        AuthorityKind::Freeze => &mut mint.freeze_authority,
        AuthorityKind::Hook => &mut mint.hook_authority,
        AuthorityKind::Metadata => &mut mint.metadata_authority,
    };
    require_keys_eq!(
        (*slot).ok_or(TokenError::AuthorityRevoked)?,
        signer,
        TokenError::NotAuthorized
    );
    *slot = new_authority;
    emit_cpi!(AuthoritySet {
        mint: mint.key(),
        kind: kind as u8,
        new_authority,
        ts: Clock::get()?.unix_timestamp
    });
    Ok(())
}

/// Accounts of `set_hook`.
#[event_cpi]
#[derive(Accounts)]
pub struct SetHook<'info> {
    /// The hook authority.
    pub authority: Signer<'info>,
    #[account(mut)]
    pub mint: Account<'info, Mint>,
}

/// `set_hook`.
pub fn process_set_hook(
    ctx: Context<SetHook>,
    hook_program: Option<Pubkey>,
    hook_flags: u16,
) -> Result<()> {
    let mint = &mut ctx.accounts.mint;
    require_keys_eq!(
        mint.hook_authority.ok_or(TokenError::AuthorityRevoked)?,
        ctx.accounts.authority.key(),
        TokenError::NotAuthorized
    );
    check_hook(hook_program, hook_flags)?;
    mint.hook_program = hook_program;
    mint.hook_flags = hook_flags;
    mint.hook_signer_bump = hook_signer_bump(hook_program);
    emit_cpi!(HookSet {
        mint: mint.key(),
        hook_program,
        hook_flags,
        ts: Clock::get()?.unix_timestamp
    });
    Ok(())
}

/// Accounts of `update_metadata`.
#[event_cpi]
#[derive(Accounts)]
pub struct UpdateMetadata<'info> {
    /// The metadata authority.
    pub authority: Signer<'info>,
    #[account(mut)]
    pub mint: Account<'info, Mint>,
}

/// `update_metadata`.
pub fn process_update_metadata(
    ctx: Context<UpdateMetadata>,
    name: Option<String>,
    symbol: Option<String>,
    uri: Option<String>,
) -> Result<()> {
    let mint = &mut ctx.accounts.mint;
    require_keys_eq!(
        mint.metadata_authority
            .ok_or(TokenError::AuthorityRevoked)?,
        ctx.accounts.authority.key(),
        TokenError::NotAuthorized
    );
    let name = name.unwrap_or_else(|| mint.name.clone());
    let symbol = symbol.unwrap_or_else(|| mint.symbol.clone());
    let uri = uri.unwrap_or_else(|| mint.uri.clone());
    check_metadata(&name, &symbol, &uri)?;
    mint.name = name.clone();
    mint.symbol = symbol.clone();
    mint.uri = uri.clone();
    emit_cpi!(MetadataUpdated {
        mint: mint.key(),
        name,
        symbol,
        uri,
        ts: Clock::get()?.unix_timestamp
    });
    Ok(())
}
