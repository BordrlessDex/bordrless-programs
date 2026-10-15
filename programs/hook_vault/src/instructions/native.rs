//! Native vaults (phase 3): a vault whose slots hold their SOL (`pending_sol`, and each sell's
//! proceeds) as their owners' lamports above the rent-exempt minimum of an account with no data,
//! instead of bridged SOL in their holdings. A native vault sells its coin through the DEX's
//! `swap_v2` (the SOL delivered to the slot's owner), buys a native pool through `swap_v2` and a
//! bridged one with its lamports wrapped first (the slot's owner, a system account this program
//! signs for, is the bridge's `wrap_sol` user), and pays its wallets, crankers and the incinerator
//! with plain transfers. `pending_sol` keeps its meaning: what the slot holds for later.
//!
//! `migrate_vault_to_native` and `revert_vault_to_bridged`: the program's upgrade authority, only
//! while the DEX is paused, the first only once the coin's launch is native; idempotent. Each slot
//! in use with `pending_sol` moves exactly that much between its holding and its owner's lamports
//! (bridged SOL sent to a slot's holding beyond it stays there, uncounted, as before).
//! Remaining: each slot in use with pending SOL, its owner then its holding of bridged SOL (both
//! writable), then the bridge's `wrap_sol`/`unwrap_sol` accounts.

use anchor_lang::prelude::*;
use bordrless_launch::client as launch_client;
use bordrless_launch::state::Launch;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::VaultError;
use crate::instructions::common::*;
use crate::invoke::invoke_built;
use crate::state::*;

/// Accounts of `migrate_vault_to_native` and `revert_vault_to_bridged`.
#[derive(Accounts)]
pub struct VaultForm<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    /// CHECK: this program's ProgramData (parsed by the DEX's `upgrade_authority`).
    pub program_data: UncheckedAccount<'info>,
    /// CHECK: the DEX config (address-checked), read for its pause.
    #[account(address = bordrless_swap::client::config_address())]
    pub dex_config: UncheckedAccount<'info>,
    #[account(mut, seeds = [VAULT_SEED, vault.mint.as_ref()], bump = vault.bump)]
    pub vault: Box<Account<'info, Vault>>,
    #[account(address = launch_client::launch_address(&vault.mint))]
    pub launch: Box<Account<'info, Launch>>,
    pub system_program: Program<'info, System>,
}

/// `migrate_vault_to_native` (`native`) and `revert_vault_to_bridged`.
pub fn process_set_vault_native<'info>(
    ctx: Context<'info, VaultForm<'info>>,
    native: bool,
) -> Result<()> {
    let upgrade =
        bordrless_swap::instructions::upgrade_authority(&ctx.accounts.program_data, &crate::ID)
            .map_err(|_| error!(VaultError::NotUpgradeAuthority))?;
    require!(
        upgrade == Some(ctx.accounts.admin.key()),
        VaultError::NotUpgradeAuthority
    );
    {
        let data = ctx.accounts.dex_config.try_borrow_data()?;
        let dex = bordrless_swap::state::Config::try_deserialize(&mut &data[..])?;
        require!(dex.paused, VaultError::DexNotPaused);
    }
    if ctx.accounts.vault.is_native() == native {
        return Ok(());
    }
    if native {
        require!(ctx.accounts.launch.is_native(), VaultError::LaunchNotNative);
    }
    let mut all = ctx.accounts.to_account_infos();
    all.extend_from_slice(ctx.remaining_accounts);
    let v = &ctx.accounts.vault;
    for i in 0..v.n_slots {
        let slot = v.slots[usize::from(i)];
        if slot.policy == policy::UNUSED || slot.pending_sol == 0 {
            continue;
        }
        let seeds = SlotSeeds::new(v.mint, i, slot.owner_bump);
        let owner = seeds.address()?;
        let holding = token_client::holding_address(&BRIDGED_SOL_MINT, &owner);
        require!(
            all.iter().any(|a| *a.key == holding) && all.iter().any(|a| *a.key == owner),
            VaultError::MissingAccount
        );
        let ix = if native {
            bordrless_bridge::client::unwrap_sol(owner, slot.pending_sol)
        } else {
            bordrless_bridge::client::wrap_sol(owner, slot.pending_sol)
        };
        invoke_built(&ix, &all, &[&seeds.seeds()])?;
    }
    ctx.accounts.vault.set_native(native);
    Ok(())
}
