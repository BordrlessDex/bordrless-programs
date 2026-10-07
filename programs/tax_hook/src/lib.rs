//! `tax_hook`: an example token hook for the Bordrless Token Standard, and the worked example of a
//! creator's own hook on a launch (`docs/hooks-v2.md` §5.8).
//!
//! `install` sets an existing mint's hook to this program (the mint's hook authority signs),
//! records the fee and the maximum wallet size, and publishes the extra-accounts registry.
//! `prepare` does the same for a mint that does not exist yet (a launch's: the launchpad creates
//! the mint with the hook already named, so the hook's registry at
//! `["bordrless-hook-accounts", mint]` must be there first), without touching the mint. From then
//! on every transfer pays `fee_bps` to the collector's holding, taken by the token program from
//! what the destination receives, and a transfer that would leave a wallet above `max_wallet_bps`
//! of the supply fails. The collector is exempt on both sides. Until the collector's holding of
//! the mint exists (it can only be created once the mint does), transfers pay no fee.

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke;
use bordrless_core::{fee_amount, BPS};
use bordrless_hook::{
    hook_accounts_address, token_flags, write_registry, AccountSource, Delta, ExtraAccount,
    HookAccountList, HookReturn, Seed, TokenHookArgs, TokenOp, HOOK_AUTHORITY_SEED,
    TOKEN_PREFIX_ACCOUNTS,
};
use bordrless_token::client as token_client;
use bordrless_token::state::Mint as TokenMint;

declare_id!("8tjnVSreJGBRQFyDBf1SyyhBgLsdBxa2rHYh9sbxFyX7");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Bordrless example tax hook",
    project_url: "https://github.com/BordrlessDex/bordrless-programs",
    contacts: "link:https://github.com/BordrlessDex/bordrless-programs/security/advisories/new",
    policy: "https://github.com/BordrlessDex/bordrless-programs/blob/main/SECURITY.md",
    source_code: "https://github.com/BordrlessDex/bordrless-programs"
}

/// `["tax", mint]`.
pub const TAX_SEED: &[u8] = b"tax";
/// The token program's signer of every callback to this hook: `["hook-authority", tax_hook]`
/// under the token program. The token program signs each hook's callbacks with a PDA of that
/// hook's id, so a signer another hook received and passes on in a CPI here is refused.
pub const TOKEN_HOOK_SIGNER: Pubkey =
    Pubkey::from_str_const("8v2CVajpJMVKXLpZePXQpqvq2nyn7r1so7DxgXu4CkAw");
/// Index of the collector's holding in a callback's account list (prefix of 5, the tax config, the
/// collector holding).
pub const COLLECTOR_INDEX: u8 = TOKEN_PREFIX_ACCOUNTS as u8 + 1;
/// The flags `install` sets, and the flags a `LaunchConfig` names for this hook.
pub const FLAGS: u16 = token_flags::BEFORE_TRANSFER | token_flags::TRANSFER_RETURNS_DELTA;

/// Instructions of the example hook.
#[program]
pub mod tax_hook {
    use super::*;

    /// Installs the hook on `mint`: the mint's hook authority signs.
    pub fn install(ctx: Context<Install>, fee_bps: u16, max_wallet_bps: u16) -> Result<()> {
        require!(
            fee_bps <= 2_000 && max_wallet_bps <= 10_000,
            TaxError::BadParams
        );
        let mint = ctx.accounts.mint.key();
        let collector = token_client::read_holding(&ctx.accounts.collector_holding)?;
        require_keys_eq!(collector.mint, mint, TaxError::WrongHolding);
        let tax = &mut ctx.accounts.tax;
        tax.bump = ctx.bumps.tax;
        tax.mint = mint;
        tax.authority = ctx.accounts.authority.key();
        tax.fee_bps = fee_bps;
        tax.max_wallet_bps = max_wallet_bps;
        tax.collector_holding = ctx.accounts.collector_holding.key();
        tax.collector_owner = collector.owner;
        tax.collected = 0;
        publish_registry(
            &ctx.accounts.authority.to_account_info(),
            &ctx.accounts.registry.to_account_info(),
            &ctx.accounts.system_program.to_account_info(),
            &mint,
            ctx.accounts.collector_holding.key(),
        )?;
        // The mint's hook, set by the authority's forwarded signature.
        let ix = token_client::set_hook(ctx.accounts.authority.key(), mint, Some(crate::ID), FLAGS);
        invoke(
            &ix,
            &[
                ctx.accounts.authority.to_account_info(),
                ctx.accounts.mint.to_account_info(),
                ctx.accounts.token_event_authority.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
            ],
        )?;
        Ok(())
    }

    /// Prepares the hook for `mint`, which need not exist yet (a launch's mint: the launchpad
    /// creates it with this program as its hook, `FLAGS` and no hook authority, from a
    /// `LaunchConfig` that names it): the tax config and the registry, with the collector's
    /// future holding of the mint (`holding(mint, collector)`) as the fee's destination. Create
    /// that holding once the mint exists; transfers before that pay no fee.
    pub fn prepare(ctx: Context<Prepare>, fee_bps: u16, max_wallet_bps: u16) -> Result<()> {
        require!(
            fee_bps <= 2_000 && max_wallet_bps <= 10_000,
            TaxError::BadParams
        );
        let mint = ctx.accounts.mint.key();
        let collector_owner = ctx.accounts.collector.key();
        let collector_holding = token_client::holding_address(&mint, &collector_owner);
        let tax = &mut ctx.accounts.tax;
        tax.bump = ctx.bumps.tax;
        tax.mint = mint;
        tax.authority = ctx.accounts.authority.key();
        tax.fee_bps = fee_bps;
        tax.max_wallet_bps = max_wallet_bps;
        tax.collector_holding = collector_holding;
        tax.collector_owner = collector_owner;
        tax.collected = 0;
        publish_registry(
            &ctx.accounts.authority.to_account_info(),
            &ctx.accounts.registry.to_account_info(),
            &ctx.accounts.system_program.to_account_info(),
            &mint,
            collector_holding,
        )
    }

    /// `before_transfer`: the fee (one delta to the collector), and the wallet cap.
    pub fn before_transfer(
        ctx: Context<BeforeTransfer>,
        args: TokenHookArgs,
    ) -> Result<HookReturn> {
        // The collector's holding exists once the mint does and someone created it; until then a
        // delta to it would be refused by the token program, so no fee is taken.
        let collector_exists = *ctx.accounts.collector_holding.owner == bordrless_token::ID;
        let tax = &mut ctx.accounts.tax;
        require!(args.op == TokenOp::Transfer, TaxError::BadParams);
        // The arguments are the token program's for this mint (its signer for this hook signed).
        require_keys_eq!(args.mint, tax.mint, TaxError::WrongMint);
        let exempt = args.source_owner == tax.collector_owner
            || args.destination_owner == tax.collector_owner;
        let delta = if exempt || !collector_exists {
            0
        } else {
            fee_amount(args.amount, tax.fee_bps)
                .ok_or(TaxError::Overflow)?
                .min(args.amount)
        };
        if tax.max_wallet_bps > 0
            && tax.max_wallet_bps < 10_000
            && args.destination_owner != tax.collector_owner
        {
            let cap = u128::from(args.supply) * u128::from(tax.max_wallet_bps) / u128::from(BPS);
            let after = u128::from(args.destination_balance) + u128::from(args.amount - delta);
            require!(after <= cap, TaxError::WalletTooLarge);
        }
        // A running total, informational: it never blocks a transfer.
        tax.collected = tax.collected.saturating_add(delta);
        // A delta must be above zero, so a free transfer answers none.
        let deltas = if delta > 0 {
            vec![Delta {
                amount: delta,
                account: COLLECTOR_INDEX,
            }]
        } else {
            vec![]
        };
        Ok(HookReturn {
            deltas,
            ..HookReturn::default()
        })
    }
}

/// The registry for `mint`: the tax config (a PDA of this program on the mint), then the
/// collector's holding, at `["bordrless-hook-accounts", mint]` under this program.
fn publish_registry<'info>(
    payer: &AccountInfo<'info>,
    registry: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    mint: &Pubkey,
    collector_holding: Pubkey,
) -> Result<()> {
    let (expected, bump) = hook_accounts_address(&crate::ID, mint);
    require_keys_eq!(*registry.key, expected, TaxError::WrongHolding);
    let list = HookAccountList::new(vec![
        ExtraAccount {
            writable: true,
            source: AccountSource::Pda {
                program: crate::ID,
                seeds: vec![Seed::Literal(TAX_SEED.to_vec()), Seed::Account(1)],
            },
        },
        ExtraAccount {
            writable: true,
            source: AccountSource::Key(collector_holding),
        },
    ]);
    write_registry(
        payer,
        registry,
        system_program,
        &crate::ID,
        mint,
        bump,
        &list,
    )
}

/// The hook's settings for one mint, at `["tax", mint]`.
#[account]
#[derive(InitSpace)]
pub struct TaxConfig {
    pub bump: u8,
    pub mint: Pubkey,
    pub authority: Pubkey,
    pub fee_bps: u16,
    pub max_wallet_bps: u16,
    pub collector_holding: Pubkey,
    pub collector_owner: Pubkey,
    pub collected: u64,
}

/// Accounts of `install`.
#[derive(Accounts)]
pub struct Install<'info> {
    /// The mint's hook authority; pays the rent.
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(mut)]
    pub mint: Account<'info, TokenMint>,
    #[account(init, payer = authority, space = 8 + TaxConfig::INIT_SPACE, seeds = [TAX_SEED, mint.key().as_ref()], bump)]
    pub tax: Account<'info, TaxConfig>,
    /// CHECK: the collector's holding of the mint (checked in the handler).
    pub collector_holding: UncheckedAccount<'info>,
    /// CHECK: the registry PDA, created here.
    #[account(mut)]
    pub registry: UncheckedAccount<'info>,
    /// CHECK: the token program.
    #[account(address = bordrless_token::ID)]
    pub token_program: UncheckedAccount<'info>,
    /// CHECK: its event authority.
    pub token_event_authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Accounts of `prepare`.
#[derive(Accounts)]
pub struct Prepare<'info> {
    /// Whoever prepares the hook (the launch's creator); pays the rent.
    #[account(mut)]
    pub authority: Signer<'info>,
    /// CHECK: the mint the hook is prepared for; need not exist yet.
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the collector: the owner whose holding of the mint receives the fee.
    pub collector: UncheckedAccount<'info>,
    #[account(init, payer = authority, space = 8 + TaxConfig::INIT_SPACE, seeds = [TAX_SEED, mint.key().as_ref()], bump)]
    pub tax: Account<'info, TaxConfig>,
    /// CHECK: the registry PDA, created here.
    #[account(mut)]
    pub registry: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Accounts of `before_transfer`: the prefix, then the registry's extras.
#[derive(Accounts)]
pub struct BeforeTransfer<'info> {
    /// CHECK: the token program's signer for this hook (address- and signer-checked).
    #[account(signer, address = TOKEN_HOOK_SIGNER @ TaxError::BadHookSigner)]
    pub hook_signer: UncheckedAccount<'info>,
    /// CHECK: the mint.
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the source holding.
    pub source: UncheckedAccount<'info>,
    /// CHECK: the destination holding.
    pub destination: UncheckedAccount<'info>,
    /// CHECK: the authority.
    pub authority: UncheckedAccount<'info>,
    #[account(mut, seeds = [TAX_SEED, mint.key().as_ref()], bump = tax.bump)]
    pub tax: Account<'info, TaxConfig>,
    /// CHECK: the collector's holding (address-checked; may not exist yet after `prepare`).
    #[account(mut, address = tax.collector_holding @ TaxError::WrongHolding)]
    pub collector_holding: UncheckedAccount<'info>,
}

/// Errors.
#[error_code]
pub enum TaxError {
    #[msg("bad parameters")]
    BadParams,
    #[msg("the holding is for another mint")]
    WrongHolding,
    #[msg("the hook signer is not the token program's signer for this hook")]
    BadHookSigner,
    #[msg("the destination would hold more than the maximum")]
    WalletTooLarge,
    #[msg("math overflow")]
    Overflow,
    #[msg("the arguments are for another mint")]
    WrongMint,
}

/// `HOOK_AUTHORITY_SEED` is re-exported for clients computing this program's hook authority (unused
/// here: the hook never signs).
pub const _HOOK_AUTHORITY_SEED: &[u8] = HOOK_AUTHORITY_SEED;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_programs_signer_for_this_hook() {
        assert_eq!(TOKEN_HOOK_SIGNER, token_client::hook_signer(&crate::ID));
        assert_eq!(
            TOKEN_HOOK_SIGNER,
            Pubkey::find_program_address(
                &[HOOK_AUTHORITY_SEED, crate::ID.as_ref()],
                &bordrless_token::ID
            )
            .0
        );
    }
}
