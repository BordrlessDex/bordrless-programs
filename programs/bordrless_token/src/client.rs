//! Instruction builders for calling this program: used by the other Bordrless programs (CPI) and
//! by the tests. The account order here is the order of each `Accounts` struct, with the event
//! authority and the program appended as `#[event_cpi]` does.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{system_program, InstructionData};

use crate::instructions::CreateMintArgs;
use crate::state::{AuthorityKind, Holding, Mint};

/// This program's signer of every callback to `hook_program`: `["hook-authority",
/// hook_program]`. One per hook program, so a hook accepts only its own (a search: off chain, or
/// where it is affordable; on chain pass the key you hold to the `_with` builders).
pub fn hook_signer(hook_program: &Pubkey) -> Pubkey {
    Mint::hook_signer(hook_program).0
}

/// A mint's hook as the token instructions take it: the hook program, and this program's signer
/// of its callbacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hook {
    /// The hook program.
    pub program: Pubkey,
    /// `["hook-authority", program]` under this program.
    pub signer: Pubkey,
}

impl Hook {
    /// The hook `program`, its signer derived ([`hook_signer`]).
    pub fn of(program: Pubkey) -> Self {
        Self {
            program,
            signer: hook_signer(&program),
        }
    }
}

/// The two hook accounts of a token instruction: the hook program and its signer, each this
/// program's id (Anchor's `None`) for a mint without a hook.
fn hook_metas(hook: Option<Hook>) -> [AccountMeta; 2] {
    [
        optional(hook.map(|h| h.program)),
        optional(hook.map(|h| h.signer)),
    ]
}

/// This program's event authority.
pub fn event_authority() -> Pubkey {
    crate::EVENT_AUTHORITY_AND_BUMP.0
}

/// The holding of `owner` for `mint`.
pub fn holding_address(mint: &Pubkey, owner: &Pubkey) -> Pubkey {
    Holding::address(mint, owner).0
}

fn with_events(mut accounts: Vec<AccountMeta>) -> Vec<AccountMeta> {
    accounts.push(AccountMeta::new_readonly(event_authority(), false));
    accounts.push(AccountMeta::new_readonly(crate::ID, false));
    accounts
}

/// An optional account as Anchor expects it: the program id stands for `None`.
fn optional(key: Option<Pubkey>) -> AccountMeta {
    AccountMeta::new_readonly(key.unwrap_or(crate::ID), false)
}

/// `create_mint`.
pub fn create_mint(payer: Pubkey, mint: Pubkey, args: CreateMintArgs) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(mint, true),
            AccountMeta::new_readonly(system_program::ID, false),
        ]),
        data: crate::instruction::CreateMint { args }.data(),
    }
}

/// `create_holding` (idempotent).
pub fn create_holding(payer: Pubkey, mint: Pubkey, owner: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(owner, false),
            AccountMeta::new(holding_address(&mint, &owner), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ]),
        data: crate::instruction::CreateHolding {}.data(),
    }
}

/// `transfer`. `hook_program` is the mint's hook, if any (its signer is derived: see
/// [`transfer_with`]); `extras` are its resolved extra accounts.
pub fn transfer(
    authority: Pubkey,
    source: Pubkey,
    destination: Pubkey,
    mint: Pubkey,
    hook_program: Option<Pubkey>,
    extras: Vec<AccountMeta>,
    amount: u64,
) -> Instruction {
    transfer_with(
        authority,
        source,
        destination,
        mint,
        hook_program.map(Hook::of),
        extras,
        amount,
    )
}

/// `transfer` with the mint's hook given whole (no derivation).
pub fn transfer_with(
    authority: Pubkey,
    source: Pubkey,
    destination: Pubkey,
    mint: Pubkey,
    hook: Option<Hook>,
    extras: Vec<AccountMeta>,
    amount: u64,
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new_readonly(authority, true),
        AccountMeta::new(source, false),
        AccountMeta::new(destination, false),
        AccountMeta::new_readonly(mint, false),
    ];
    accounts.extend(hook_metas(hook));
    let mut accounts = with_events(accounts);
    accounts.extend(extras);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::Transfer { amount }.data(),
    }
}

/// `mint_to`. `hook_program` is the mint's hook, if any (its signer is derived).
pub fn mint_to(
    authority: Pubkey,
    mint: Pubkey,
    destination: Pubkey,
    hook_program: Option<Pubkey>,
    extras: Vec<AccountMeta>,
    amount: u64,
) -> Instruction {
    mint_to_with(
        authority,
        mint,
        destination,
        hook_program.map(Hook::of),
        extras,
        amount,
    )
}

/// `mint_to` with the mint's hook given whole (no derivation).
pub fn mint_to_with(
    authority: Pubkey,
    mint: Pubkey,
    destination: Pubkey,
    hook: Option<Hook>,
    extras: Vec<AccountMeta>,
    amount: u64,
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new_readonly(authority, true),
        AccountMeta::new(mint, false),
        AccountMeta::new(destination, false),
    ];
    accounts.extend(hook_metas(hook));
    let mut accounts = with_events(accounts);
    accounts.extend(extras);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::MintTo { amount }.data(),
    }
}

/// `burn`. `hook_program` is the mint's hook, if any (its signer is derived).
pub fn burn(
    authority: Pubkey,
    source: Pubkey,
    mint: Pubkey,
    hook_program: Option<Pubkey>,
    extras: Vec<AccountMeta>,
    amount: u64,
) -> Instruction {
    burn_with(
        authority,
        source,
        mint,
        hook_program.map(Hook::of),
        extras,
        amount,
    )
}

/// `burn` with the mint's hook given whole (no derivation).
pub fn burn_with(
    authority: Pubkey,
    source: Pubkey,
    mint: Pubkey,
    hook: Option<Hook>,
    extras: Vec<AccountMeta>,
    amount: u64,
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new_readonly(authority, true),
        AccountMeta::new(source, false),
        AccountMeta::new(mint, false),
    ];
    accounts.extend(hook_metas(hook));
    let mut accounts = with_events(accounts);
    accounts.extend(extras);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::Burn { amount }.data(),
    }
}

/// `approve`.
pub fn approve(owner: Pubkey, holding: Pubkey, delegate: Pubkey, amount: u64) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new_readonly(owner, true),
            AccountMeta::new(holding, false),
            AccountMeta::new_readonly(delegate, false),
        ]),
        data: crate::instruction::Approve { amount }.data(),
    }
}

/// `revoke`.
pub fn revoke(owner: Pubkey, holding: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new_readonly(owner, true),
            AccountMeta::new(holding, false),
        ]),
        data: crate::instruction::Revoke {}.data(),
    }
}

/// `set_frozen`.
pub fn set_frozen(authority: Pubkey, mint: Pubkey, holding: Pubkey, frozen: bool) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new_readonly(authority, true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(holding, false),
        ]),
        data: crate::instruction::SetFrozen { frozen }.data(),
    }
}

/// `close_holding`: `mint` is the holding's mint.
pub fn close_holding(
    owner: Pubkey,
    mint: Pubkey,
    holding: Pubkey,
    destination: Pubkey,
) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new_readonly(owner, true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(holding, false),
            AccountMeta::new(destination, false),
        ]),
        data: crate::instruction::CloseHolding {}.data(),
    }
}

/// `write_hook_data`: `hook_signer` is the `["hook-authority"]` PDA of the mint's hook program,
/// which signs by CPI.
pub fn write_hook_data(
    hook_signer: Pubkey,
    mint: Pubkey,
    holding: Pubkey,
    data: [u8; 64],
) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new_readonly(hook_signer, true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(holding, false),
        ]),
        data: crate::instruction::WriteHookData { data }.data(),
    }
}

/// `set_authority`.
pub fn set_authority(
    authority: Pubkey,
    mint: Pubkey,
    kind: AuthorityKind,
    new_authority: Option<Pubkey>,
) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new_readonly(authority, true),
            AccountMeta::new(mint, false),
        ]),
        data: crate::instruction::SetAuthority {
            kind,
            new_authority,
        }
        .data(),
    }
}

/// `set_hook`.
pub fn set_hook(
    authority: Pubkey,
    mint: Pubkey,
    hook_program: Option<Pubkey>,
    hook_flags: u16,
) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new_readonly(authority, true),
            AccountMeta::new(mint, false),
        ]),
        data: crate::instruction::SetHook {
            hook_program,
            hook_flags,
        }
        .data(),
    }
}

/// `update_metadata`.
pub fn update_metadata(
    authority: Pubkey,
    mint: Pubkey,
    name: Option<String>,
    symbol: Option<String>,
    uri: Option<String>,
) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new_readonly(authority, true),
            AccountMeta::new(mint, false),
        ]),
        data: crate::instruction::UpdateMetadata { name, symbol, uri }.data(),
    }
}

/// Reads a holding from its account data (owner and discriminator checked).
pub fn read_holding(info: &AccountInfo) -> Result<Holding> {
    require_keys_eq!(
        *info.owner,
        crate::ID,
        anchor_lang::error::ErrorCode::AccountOwnedByWrongProgram
    );
    let data = info.try_borrow_data()?;
    Holding::try_deserialize(&mut &data[..])
}

/// Reads a mint from its account data (owner and discriminator checked).
pub fn read_mint(info: &AccountInfo) -> Result<crate::state::Mint> {
    require_keys_eq!(
        *info.owner,
        crate::ID,
        anchor_lang::error::ErrorCode::AccountOwnedByWrongProgram
    );
    let data = info.try_borrow_data()?;
    crate::state::Mint::try_deserialize(&mut &data[..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use bordrless_hook::HOOK_AUTHORITY_SEED;

    #[test]
    fn hook_signers_are_one_per_hook_program() {
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        let (expected, bump) =
            Pubkey::find_program_address(&[HOOK_AUTHORITY_SEED, a.as_ref()], &crate::ID);
        assert_eq!((hook_signer(&a), Mint::hook_signer(&a).1), (expected, bump));
        assert_ne!(hook_signer(&a), hook_signer(&b));
        assert_eq!(Hook::of(a).signer, expected);
        // A mint without a hook passes this program's id in both hook slots.
        let ix = transfer(a, b, a, b, None, vec![], 1);
        assert_eq!(
            (ix.accounts[4].pubkey, ix.accounts[5].pubkey),
            (crate::ID, crate::ID)
        );
        let ix = transfer(a, b, a, b, Some(a), vec![], 1);
        assert_eq!(
            (ix.accounts[4].pubkey, ix.accounts[5].pubkey),
            (a, expected)
        );
    }
}
