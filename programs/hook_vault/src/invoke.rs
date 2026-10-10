//! How a vault calls another program (the companion's `invoke.rs`): the instruction is built here,
//! with the callee's own client builder, from keys the vault derives or reads from accounts the
//! callee itself checks; the accounts a caller passed are only looked up by key. A step can
//! therefore never be pointed at other accounts than the ones its instruction names: a caller who
//! leaves one out gets `MissingAccount`, one who passes another gets nothing from it.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program::invoke_signed;

use crate::error::VaultError;

/// Invokes `ix` with the account infos of its keys, found among `available`, signing with `seeds`.
pub fn invoke_built<'info>(
    ix: &Instruction,
    available: &[AccountInfo<'info>],
    seeds: &[&[&[u8]]],
) -> Result<()> {
    // The error is built only for a key that is missing: `error!` allocates its name and message,
    // and the program's heap (32 KiB, never freed) can't afford that on every lookup of every call.
    let find = |key: &Pubkey| {
        available
            .iter()
            .find(|a| a.key == key)
            .cloned()
            .ok_or_else(|| error!(VaultError::MissingAccount))
    };
    let mut infos = Vec::with_capacity(ix.accounts.len() + 1);
    for meta in &ix.accounts {
        infos.push(find(&meta.pubkey)?);
    }
    infos.push(find(&ix.program_id)?);
    invoke_signed(ix, &infos, seeds)?;
    Ok(())
}
