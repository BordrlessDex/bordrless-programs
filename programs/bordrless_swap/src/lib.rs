//! `bordrless_swap`: the Bordrless DEX.
//!
//! Constant-product pools between two Bordrless Token Standard mints. A pool has an LP fee that
//! stays in the pool, a protocol fee always taken in the quote token that the admin collects for
//! the fee collector, optional virtual reserves (a launch curve), an LP mint on the token standard,
//! and an optional hook program that runs before and after swaps and liquidity changes, can take
//! up to three deltas and a burn from a swap's input (before) and output (after), and can set the
//! LP fee of a swap (hook protocol v2, `bordrless_hook`; `docs/hooks-v2.md` §3). Every callback is
//! signed by this program's `["hook-authority", hook_program]` PDA, one per hook program, so a
//! signer one pool hook receives and passes on is never another hook's (the pool keeps the bump).
//! Each side's token-hook slice carries the token program's signer for that mint's hook.
//!
//! Amounts are measured, never assumed: the vault balance is read before and after every transfer,
//! so a token whose own hook takes a cut still prices correctly, and `min_amount_out` is checked
//! against what the recipient's holding actually gained.
//!
//! Layout: [`state`], [`instructions`] (config, pool, liquidity, swap), [`hooks`] (pool hook CPIs
//! and delta transfers), [`token`] (token standard CPIs), [`events`], [`error`], [`client`].

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;

pub mod client;
pub mod constants;
pub mod error;
pub mod events;
pub mod hooks;
pub mod instructions;
pub mod state;
pub mod token;

pub use instructions::*;

declare_id!("GyzKSnnEu2uN5bBRecE4XYY2enbfR2D2MtxnbJPGy7hk");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Bordrless DEX",
    project_url: "https://github.com/BordrlessDex/bordrless-programs",
    contacts: "link:https://github.com/BordrlessDex/bordrless-programs/security/advisories/new",
    policy: "https://github.com/BordrlessDex/bordrless-programs/blob/main/SECURITY.md",
    source_code: "https://github.com/BordrlessDex/bordrless-programs"
}

/// Instructions of the DEX.
#[program]
pub mod bordrless_swap {
    use super::*;

    /// Creates the config; signed by the program's upgrade authority, once.
    pub fn init_config(ctx: Context<InitConfig>, args: ConfigArgs) -> Result<()> {
        config::process_init_config(ctx, args)
    }

    /// Admin: changes the config.
    pub fn set_config(ctx: Context<SetConfig>, args: ConfigArgs) -> Result<()> {
        config::process_set_config(ctx, args)
    }

    /// Creates a pool with its first deposit. A pool with virtual reserves (a curve) can only be
    /// created by its hook program.
    pub fn create_pool<'info>(
        ctx: Context<'info, CreatePool<'info>>,
        args: CreatePoolArgs,
    ) -> Result<()> {
        pool::process_create_pool(ctx, args)
    }

    /// The pool's hook turns a curve pool into an ordinary one: reserves synced to the vaults, no
    /// virtual offsets, the first LP minted to a recipient.
    pub fn finalize_curve(ctx: Context<FinalizeCurve>, hook_caller_bump: u8) -> Result<()> {
        pool::process_finalize_curve(ctx, hook_caller_bump)
    }

    /// Admin: moves the accrued protocol fees of a pool (always in its quote token) to the fee
    /// collector's quote holding. The remaining accounts are the quote mint's token-hook slice
    /// (`quote_hook_accounts` of them, its program first).
    pub fn collect_protocol_fees<'info>(
        ctx: Context<'info, CollectProtocolFees<'info>>,
        quote_hook_accounts: u8,
    ) -> Result<()> {
        pool::process_collect_protocol_fees(ctx, quote_hook_accounts)
    }

    /// Adds liquidity at the pool's ratio and mints LP.
    pub fn add_liquidity<'info>(
        ctx: Context<'info, Liquidity<'info>>,
        args: AddLiquidityArgs,
    ) -> Result<()> {
        liquidity::process_add_liquidity(ctx, args)
    }

    /// Burns LP and withdraws the proportional reserves.
    pub fn remove_liquidity<'info>(
        ctx: Context<'info, Liquidity<'info>>,
        args: RemoveLiquidityArgs,
    ) -> Result<()> {
        liquidity::process_remove_liquidity(ctx, args)
    }

    /// Swaps an exact input for at least `min_amount_out`.
    pub fn swap<'info>(ctx: Context<'info, Swap<'info>>, args: SwapArgs) -> Result<()> {
        swap::process_swap(ctx, args)
    }
}
