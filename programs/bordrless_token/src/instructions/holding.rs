//! Holdings: creation, delegation, freezing, closing.

use anchor_lang::prelude::*;

use crate::constants::*;
use crate::error::TokenError;
use crate::events::*;
use crate::state::*;

/// Accounts of `create_holding`.
#[event_cpi]
#[derive(Accounts)]
pub struct CreateHolding<'info> {
    /// Pays the rent.
    #[account(mut)]
    pub payer: Signer<'info>,
    pub mint: Account<'info, Mint>,
    /// CHECK: any address; the holding is its token account for `mint`.
    pub owner: UncheckedAccount<'info>,
    #[account(
        init_if_needed,
        payer = payer,
        space = Holding::LEN,
        seeds = [HOLDING_SEED, mint.key().as_ref(), owner.key().as_ref()],
        bump
    )]
    pub holding: Account<'info, Holding>,
    pub system_program: Program<'info, System>,
}

/// `create_holding`: idempotent.
pub fn process_create_holding(ctx: Context<CreateHolding>) -> Result<()> {
    let holding = &mut ctx.accounts.holding;
    if holding.version != 0 {
        // Already there: the seeds guarantee the mint and the owner.
        require_keys_eq!(
            holding.mint,
            ctx.accounts.mint.key(),
            TokenError::HoldingMismatch
        );
        require_keys_eq!(
            holding.owner,
            ctx.accounts.owner.key(),
            TokenError::HoldingMismatch
        );
        return Ok(());
    }
    holding.version = VERSION;
    holding.bump = ctx.bumps.holding;
    holding.mint = ctx.accounts.mint.key();
    holding.owner = ctx.accounts.owner.key();
    holding.amount = 0;
    holding.delegate = None;
    holding.delegated_amount = 0;
    holding.frozen = false;
    holding.hook_data = [0; 64];
    holding.reserved = [0; 16];
    emit_cpi!(HoldingCreated {
        mint: ctx.accounts.mint.key(),
        holding: holding.key(),
        owner: ctx.accounts.owner.key(),
        payer: ctx.accounts.payer.key(),
        ts: Clock::get()?.unix_timestamp,
    });
    Ok(())
}

/// Accounts of `approve`.
#[event_cpi]
#[derive(Accounts)]
pub struct Approve<'info> {
    /// The owner.
    pub owner: Signer<'info>,
    #[account(mut, has_one = owner @ TokenError::NotAuthorized)]
    pub holding: Account<'info, Holding>,
    /// CHECK: any address.
    pub delegate: UncheckedAccount<'info>,
}

/// `approve`.
pub fn process_approve(ctx: Context<Approve>, amount: u64) -> Result<()> {
    let holding = &mut ctx.accounts.holding;
    holding.delegate = Some(ctx.accounts.delegate.key());
    holding.delegated_amount = amount;
    emit_cpi!(DelegateSet {
        mint: holding.mint,
        holding: holding.key(),
        delegate: holding.delegate,
        amount,
        ts: Clock::get()?.unix_timestamp
    });
    Ok(())
}

/// Accounts of `revoke`.
#[event_cpi]
#[derive(Accounts)]
pub struct Revoke<'info> {
    /// The owner.
    pub owner: Signer<'info>,
    #[account(mut, has_one = owner @ TokenError::NotAuthorized)]
    pub holding: Account<'info, Holding>,
}

/// `revoke`.
pub fn process_revoke(ctx: Context<Revoke>) -> Result<()> {
    let holding = &mut ctx.accounts.holding;
    holding.delegate = None;
    holding.delegated_amount = 0;
    emit_cpi!(DelegateSet {
        mint: holding.mint,
        holding: holding.key(),
        delegate: None,
        amount: 0,
        ts: Clock::get()?.unix_timestamp
    });
    Ok(())
}

/// Accounts of `set_frozen`.
#[event_cpi]
#[derive(Accounts)]
pub struct SetFrozen<'info> {
    /// The freeze authority.
    pub authority: Signer<'info>,
    pub mint: Account<'info, Mint>,
    #[account(mut, has_one = mint @ TokenError::MintMismatch)]
    pub holding: Account<'info, Holding>,
}

/// `set_frozen`.
pub fn process_set_frozen(ctx: Context<SetFrozen>, frozen: bool) -> Result<()> {
    require_keys_eq!(
        ctx.accounts
            .mint
            .freeze_authority
            .ok_or(TokenError::AuthorityRevoked)?,
        ctx.accounts.authority.key(),
        TokenError::NotAuthorized
    );
    let holding = &mut ctx.accounts.holding;
    holding.frozen = frozen;
    emit_cpi!(FrozenSet {
        mint: holding.mint,
        holding: holding.key(),
        frozen,
        ts: Clock::get()?.unix_timestamp
    });
    Ok(())
}

/// Accounts of `close_holding`.
#[event_cpi]
#[derive(Accounts)]
pub struct CloseHolding<'info> {
    /// The owner.
    pub owner: Signer<'info>,
    /// The holding's mint (its hook decides whether hook data blocks the close).
    pub mint: Account<'info, Mint>,
    #[account(mut, has_one = owner @ TokenError::NotAuthorized, has_one = mint @ TokenError::MintMismatch, close = destination)]
    pub holding: Account<'info, Holding>,
    /// CHECK: receives the rent.
    #[account(mut)]
    pub destination: UncheckedAccount<'info>,
}

/// `close_holding`: an empty holding closes, unless the mint's hook writes hook data and the
/// holding still has some (a hook's record of what a holder is owed must not vanish with the
/// account). Without a hook, or without `WRITES_HOOK_DATA`, nobody can ever clear the data, so the
/// holding closes regardless.
pub fn process_close_holding(ctx: Context<CloseHolding>) -> Result<()> {
    let holding = &ctx.accounts.holding;
    require!(holding.amount == 0, TokenError::HoldingNotEmpty);
    require!(
        !ctx.accounts.mint.hook_writes_data() || holding.hook_data_is_empty(),
        TokenError::HookDataNotEmpty
    );
    emit_cpi!(HoldingClosed {
        mint: holding.mint,
        holding: holding.key(),
        owner: holding.owner,
        ts: Clock::get()?.unix_timestamp
    });
    Ok(())
}

/// Accounts of `write_hook_data`.
#[event_cpi]
#[derive(Accounts)]
pub struct WriteHookData<'info> {
    /// The `["hook-authority"]` PDA (canonical bump) of the mint's hook program, signing by CPI.
    pub hook_signer: Signer<'info>,
    pub mint: Account<'info, Mint>,
    #[account(mut, has_one = mint @ TokenError::MintMismatch)]
    pub holding: Account<'info, Holding>,
}

/// `write_hook_data`: the mint's hook program replaces a holding's hook data. The mint must have
/// a hook with `WRITES_HOOK_DATA`, and the signer must be that hook program's `["hook-authority"]`
/// PDA at its canonical bump. Calls no hook.
pub fn process_write_hook_data(ctx: Context<WriteHookData>, data: [u8; 64]) -> Result<()> {
    let mint = &ctx.accounts.mint;
    let program = mint.hook_program.ok_or(TokenError::HookDataNotWritable)?;
    require!(mint.hook_writes_data(), TokenError::HookDataNotWritable);
    let (expected, _) =
        Pubkey::find_program_address(&[bordrless_hook::HOOK_AUTHORITY_SEED], &program);
    require_keys_eq!(
        ctx.accounts.hook_signer.key(),
        expected,
        TokenError::NotHookAuthority
    );
    let holding = &mut ctx.accounts.holding;
    holding.hook_data = data;
    emit_cpi!(HookDataWritten {
        mint: mint.key(),
        holding: holding.key(),
        owner: holding.owner,
        data
    });
    Ok(())
}
