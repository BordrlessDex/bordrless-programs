//! `bordrless_token`: the Bordrless Token Standard.
//!
//! A mint is an account of this program holding supply, authorities, hook settings and metadata.
//! A holding (token account) is the PDA `["holding", mint, owner]`: one per owner per mint, which
//! anyone may create for anyone. Transfers, mints and burns run the mint's hook program before and
//! after the operation (`bordrless_hook`, protocol v2), signing each callback with this program's
//! `["hook-authority", hook_program]` PDA: one per hook program, so a signer one hook receives and
//! passes on is never another hook's (the bump is kept in the mint). A `before_transfer` answer
//! may take up to three deltas from the transfer, which this program credits to the holdings the
//! hook names and reports in the event; `before_transfer`, `before_mint` and `before_burn` answers
//! may replace the 64 bytes of state the hook keeps in each holding (`Holding.hook_data`), which
//! the hook program can also write directly with `write_hook_data`. A hook never gets a signature
//! and never moves tokens itself.
//!
//! Layout: [`state`] (accounts), [`instructions`] (contexts and handlers), [`hooks`] (hook CPIs,
//! answers and delta application), [`events`], [`error`], [`client`] (instruction builders other
//! programs and tests use to call this one).

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;

pub mod client;
pub mod constants;
pub mod error;
pub mod events;
pub mod hooks;
pub mod instructions;
pub mod state;

pub use instructions::*;
pub use state::AuthorityKind;

declare_id!("2XoEWp8cF3kRXg74eVwPAyTFhVCAztn3V88komxAvr22");

#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Bordrless token standard",
    project_url: "https://github.com/BordrlessDex/bordrless-programs",
    contacts: "link:https://github.com/BordrlessDex/bordrless-programs/security/advisories/new",
    policy: "https://github.com/BordrlessDex/bordrless-programs/blob/main/SECURITY.md",
    source_code: "https://github.com/BordrlessDex/bordrless-programs"
}

/// Instructions of the token standard.
#[program]
pub mod bordrless_token {
    use super::*;

    /// Creates a mint. The mint account signs (a keypair, or a PDA of the creating program).
    pub fn create_mint(ctx: Context<CreateMint>, args: CreateMintArgs) -> Result<()> {
        mint::process_create_mint(ctx, args)
    }

    /// Creates the holding of `owner` for `mint`, or does nothing if it exists.
    pub fn create_holding(ctx: Context<CreateHolding>) -> Result<()> {
        holding::process_create_holding(ctx)
    }

    /// Moves `amount` from one holding to another, running the mint's hooks. The authority is the
    /// source's owner or its delegate.
    pub fn transfer<'info>(ctx: Context<'info, Transfer<'info>>, amount: u64) -> Result<()> {
        transfer::process_transfer(ctx, amount)
    }

    /// Mints `amount` into a holding; the mint authority signs.
    pub fn mint_to<'info>(ctx: Context<'info, MintTo<'info>>, amount: u64) -> Result<()> {
        mint::process_mint_to(ctx, amount)
    }

    /// Burns `amount` from a holding; its owner or delegate signs.
    pub fn burn<'info>(ctx: Context<'info, Burn<'info>>, amount: u64) -> Result<()> {
        transfer::process_burn(ctx, amount)
    }

    /// Lets `delegate` move up to `amount` from the holding.
    pub fn approve(ctx: Context<Approve>, amount: u64) -> Result<()> {
        holding::process_approve(ctx, amount)
    }

    /// Removes the holding's delegate.
    pub fn revoke(ctx: Context<Revoke>) -> Result<()> {
        holding::process_revoke(ctx)
    }

    /// Freezes or thaws a holding; the freeze authority signs.
    pub fn set_frozen(ctx: Context<SetFrozen>, frozen: bool) -> Result<()> {
        holding::process_set_frozen(ctx, frozen)
    }

    /// Closes an empty holding and returns its rent; the owner signs. Refused while the mint's
    /// hook writes hook data and the holding still has some.
    pub fn close_holding(ctx: Context<CloseHolding>) -> Result<()> {
        holding::process_close_holding(ctx)
    }

    /// The mint's hook program replaces a holding's hook data, signing with its
    /// `["hook-authority"]` PDA; the mint must have `WRITES_HOOK_DATA`.
    pub fn write_hook_data(ctx: Context<WriteHookData>, data: [u8; 64]) -> Result<()> {
        holding::process_write_hook_data(ctx, data)
    }

    /// Replaces or revokes one of the mint's authorities; the current one signs.
    pub fn set_authority(
        ctx: Context<SetAuthority>,
        kind: AuthorityKind,
        new_authority: Option<Pubkey>,
    ) -> Result<()> {
        mint::process_set_authority(ctx, kind, new_authority)
    }

    /// Sets or clears the mint's hook; the hook authority signs.
    pub fn set_hook(
        ctx: Context<SetHook>,
        hook_program: Option<Pubkey>,
        hook_flags: u16,
    ) -> Result<()> {
        mint::process_set_hook(ctx, hook_program, hook_flags)
    }

    /// Updates the mint's metadata; the metadata authority signs.
    pub fn update_metadata(
        ctx: Context<UpdateMetadata>,
        name: Option<String>,
        symbol: Option<String>,
        uri: Option<String>,
    ) -> Result<()> {
        mint::process_update_metadata(ctx, name, symbol, uri)
    }
}
