//! `bordrless_bridge`: SPL Token, Token-2022 and native SOL in; Bordrless Token Standard out, one
//! for one, and back.
//!
//! A wrapper (`["wrapper", underlying]`) holds the underlying in a vault (the wrapper PDA's
//! associated token account, or the `["sol-vault"]` PDA for SOL) and is the mint authority of the
//! wrapped BTS mint (`["wrapped", underlying]`). `wrap` mints what actually arrived in the vault, so
//! a Token-2022 transfer fee is honoured; `unwrap` burns and pays out. Mints with transfer hooks
//! are not supported in this version.

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;

pub mod client;
pub mod constants;
pub mod error;
pub mod events;
pub mod instructions;
pub mod spl;
pub mod state;

pub use instructions::*;

declare_id!("CtLkuFVitoXHTa86Hfp8KmfSDfqJaMYFWr6EGmQVsKb7");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Bordrless bridge",
    project_url: "https://github.com/BordrlessDex/bordrless-programs",
    contacts: "link:https://github.com/BordrlessDex/bordrless-programs/security/advisories/new",
    policy: "https://github.com/BordrlessDex/bordrless-programs/blob/main/SECURITY.md",
    source_code: "https://github.com/BordrlessDex/bordrless-programs"
}

/// Instructions of the bridge.
#[program]
pub mod bordrless_bridge {
    use super::*;

    /// Creates the config; signed by the program's upgrade authority, once.
    pub fn init_config(ctx: Context<InitConfig>, args: ConfigArgs) -> Result<()> {
        config::process_init_config(ctx, args)
    }

    /// Admin: changes the config.
    pub fn set_config(ctx: Context<SetConfig>, args: ConfigArgs) -> Result<()> {
        config::process_set_config(ctx, args)
    }

    /// Registers an SPL Token or Token-2022 mint: creates its wrapper, vault and wrapped mint.
    /// Permissionless.
    pub fn register(ctx: Context<Register>, args: RegisterArgs) -> Result<()> {
        wrapper::process_register(ctx, args)
    }

    /// Registers native SOL (the wSOL mint address stands for it). Permissionless, once.
    pub fn register_sol(ctx: Context<RegisterSol>) -> Result<()> {
        wrapper::process_register_sol(ctx)
    }

    /// Moves `amount` of the underlying into the vault and mints what arrived.
    pub fn wrap(ctx: Context<Wrap>, amount: u64) -> Result<()> {
        wrap::process_wrap(ctx, amount)
    }

    /// Burns `amount` of the wrapped mint and pays the underlying out of the vault.
    pub fn unwrap(ctx: Context<Wrap>, amount: u64) -> Result<()> {
        wrap::process_unwrap(ctx, amount)
    }

    /// Moves `lamports` into the SOL vault and mints as much wrapped SOL.
    pub fn wrap_sol(ctx: Context<WrapSol>, lamports: u64) -> Result<()> {
        wrap::process_wrap_sol(ctx, lamports)
    }

    /// Burns `amount` of wrapped SOL (`u64::MAX` for the whole holding) and pays the lamports out.
    pub fn unwrap_sol(ctx: Context<WrapSol>, amount: u64) -> Result<()> {
        wrap::process_unwrap_sol(ctx, amount, false)
    }

    /// Burns everything above `keep` of the user's wrapped SOL and pays the lamports out: what a
    /// trade just delivered, leaving what was held before it.
    pub fn unwrap_sol_above(ctx: Context<WrapSol>, keep: u64) -> Result<()> {
        wrap::process_unwrap_sol(ctx, keep, true)
    }
}
