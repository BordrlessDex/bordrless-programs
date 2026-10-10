//! `hook_vault`: deferred actions for a coin's own token hook (`docs/phase3a.md` §5).
//!
//! A token hook's only value is its cuts: up to three deltas a transfer, in its own coin, to
//! holdings of that coin. It can't act on them itself: a callback makes no CPI, and outside a
//! callback a hook that moves its own coin re-enters itself (hook → DEX → token → hook), which the
//! runtime refuses. So whatever acts on the cuts must be another program. This is it.
//!
//! A vault is made for a coin before the coin exists (`create_vault`, the mint's keypair signing,
//! as the companion's `create`): up to three slots, each with a policy fixed there for good:
//!
//! - `Burn`: the slot's coin is burned.
//! - `SellForSol { to }`: the slot's coin is sold for SOL, paid to `to`, a wallet fixed before the
//!   launch. In effect a creator tax, and labelled as one.
//! - `SellBuyBurn { pool }`: the slot's coin is sold for SOL, and the SOL buys another launchpad
//!   token on `pool` and burns it.
//!
//! The coin's hook sends each cut, as a delta, to the coin holding of the slot whose policy it wants
//! (slot `i`'s owner is `PDA(["slot", mint, [i]])`): the slot's balance is the queue, and the hook
//! protocol, the token program and the DEX need no change. Once the coin is launched, `open_vault`
//! (its creator's, in the transaction after the launch's) creates the slots' holdings and sets the
//! reference prices. From then on anyone runs the slots
//! (`execute`, `execute_buy`) for a bounty, under a port of the companion's audited buyback guards,
//! mirrored for sells: small slices, spaced, at no worse than the pool's own quote less fees and
//! 2%, and never while the price is more than 3% off a reference price. A slot nobody could run for
//! 60 days is retired (`retire`): what it holds is burned, and nobody receives anything.
//!
//! Scope (phase 3a): launchpad coins only, whose launch's `custom_hook` is the vault's hook. A
//! token made outside the launchpad (whose mint's `hook_authority` would make its vault) is deferred.
//!
//! Layout: [`state`], [`instructions`] (`create`: create and open; `steps`: execute, execute_buy,
//! retire; `common`: what they share), [`invoke`] (how a step calls another program), [`events`],
//! [`error`], [`client`].

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;

pub mod client;
pub mod constants;
pub mod error;
pub mod events;
pub mod instructions;
pub mod invoke;
pub mod state;

pub use instructions::*;

declare_id!("5cojoUStG7WFhHJiSncCUEwTqDuhDLKu4a9BqTbF8jzG");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Bordrless hook vault",
    project_url: "https://github.com/BordrlessDex/bordrless-programs",
    contacts: "link:https://github.com/BordrlessDex/bordrless-programs/security/advisories/new",
    policy: "https://github.com/BordrlessDex/bordrless-programs/blob/main/SECURITY.md",
    source_code: "https://github.com/BordrlessDex/bordrless-programs"
}

/// Logs how much of the 32 KiB heap the instruction has used (a `heap-probe` build only, for the
/// tests' measurement). The program's bump allocator never frees and keeps its position in the
/// heap's first word, so the end position is the peak.
#[cfg(feature = "heap-probe")]
pub fn log_heap() {
    #[cfg(target_os = "solana")]
    {
        const START: usize = 0x3_0000_0000;
        // SAFETY: the runtime maps the heap at START; its first word is the allocator's position.
        let pos = unsafe { *(START as *const usize) };
        let used = if pos == 0 { 0 } else { START + 32 * 1024 - pos };
        msg!("heap used: {}", used);
    }
}

#[program]
pub mod hook_vault {
    use super::*;

    /// Makes the vault of `mint` (not created yet; it signs): the coin's hook, the slots' policies,
    /// the bounty, the sell limits and the interval, all fixed for good. Remaining: each
    /// `SellBuyBurn` slot's pool and that pool's launch.
    pub fn create_vault(ctx: Context<CreateVault>, args: CreateVaultArgs) -> Result<()> {
        instructions::create::process_create_vault(ctx, args)
    }

    /// The vault's creator (or a sender with the mint's keypair signing), once the coin is launched
    /// with the vault's hook: creates the slots' holdings (the sender pays) and sets the reference
    /// prices (the sells': the coin's pool price, at most its launch's opening price).
    pub fn open_vault<'info>(ctx: Context<'info, OpenVault<'info>>) -> Result<()> {
        instructions::create::process_open_vault(ctx)?;
        #[cfg(feature = "heap-probe")]
        log_heap();
        Ok(())
    }

    /// Anyone: runs slot `i`: burns its coin, or sells a slice of it (or waits), paying the SOL to
    /// the slot's wallet or keeping it for the slot's buy.
    pub fn execute<'info>(ctx: Context<'info, Step<'info>>, i: u8) -> Result<()> {
        instructions::steps::process_execute(ctx, i)?;
        #[cfg(feature = "heap-probe")]
        log_heap();
        Ok(())
    }

    /// Anyone: a `SellBuyBurn` slot buys the other token with the SOL its sells left (or waits),
    /// and burns it.
    pub fn execute_buy<'info>(ctx: Context<'info, Step<'info>>, i: u8) -> Result<()> {
        instructions::steps::process_execute_buy(ctx, i)?;
        #[cfg(feature = "heap-probe")]
        log_heap();
        Ok(())
    }

    /// Anyone, after 60 days in which slot `i` did not run: burns its SOL (to the incinerator) and,
    /// with `burn_coin`, its coin. Nobody receives anything.
    pub fn retire<'info>(ctx: Context<'info, Step<'info>>, i: u8, burn_coin: bool) -> Result<()> {
        instructions::steps::process_retire(ctx, i, burn_coin)?;
        #[cfg(feature = "heap-probe")]
        log_heap();
        Ok(())
    }
}
