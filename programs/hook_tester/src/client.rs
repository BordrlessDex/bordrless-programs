//! Instruction builders for the test hook. Account order is that of each `Accounts` struct (none
//! of this program's instructions emits events). The pool callbacks are called by the DEX only.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{system_program, InstructionData};
use bordrless_hook::{hook_accounts_address, ExtraAccount, HookReturn, HOOK_AUTHORITY_SEED};
use bordrless_swap::client as swap_client;
use bordrless_swap::instructions::SwapArgs;
use bordrless_token::client as token_client;

use crate::SCRIPT_SEED;

/// The script of `key` (a mint, or a pool).
pub fn script_address(key: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[SCRIPT_SEED, key.as_ref()], &crate::ID).0
}

/// The registry of `key`.
pub fn registry_address(key: &Pubkey) -> Pubkey {
    hook_accounts_address(&crate::ID, key).0
}

/// This program's `["hook-authority"]` PDA and its canonical bump.
pub fn hook_authority() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[HOOK_AUTHORITY_SEED], &crate::ID)
}

/// `probe_curve`: whether each of `keys` is on the ed25519 curve, one byte each in the return
/// data (zeros without checking when `check` is false).
pub fn probe_curve(keys: Vec<Pubkey>, check: bool) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: vec![],
        data: crate::instruction::ProbeCurve { keys, check }.data(),
    }
}

/// `init_script`: the script of `key` and its registry (the script, then `extras`).
pub fn init_script(payer: Pubkey, key: Pubkey, extras: Vec<ExtraAccount>) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(key, false),
            AccountMeta::new(script_address(&key), false),
            AccountMeta::new(registry_address(&key), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: crate::instruction::InitScript { extras }.data(),
    }
}

/// `set_answer`: callback `which` ([`crate::callback`]) of `key`'s script does `action`
/// ([`crate::mode`]), returning `data` for `mode::RETURN`.
pub fn set_answer(
    authority: Pubkey,
    key: Pubkey,
    which: u8,
    action: u8,
    data: Vec<u8>,
) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: vec![
            AccountMeta::new_readonly(authority, true),
            AccountMeta::new(script_address(&key), false),
        ],
        data: crate::instruction::SetAnswer {
            which,
            action,
            data,
        }
        .data(),
    }
}

/// `set_answer` returning `answer` (Borsh) from callback `which`.
pub fn answer(authority: Pubkey, key: Pubkey, which: u8, answer: &HookReturn) -> Instruction {
    let mut data = Vec::new();
    answer.serialize(&mut data).expect("vec write");
    set_answer(authority, key, which, crate::mode::RETURN, data)
}

/// `set_answer` making callback `which` pass on its signer: it calls the first of its remaining
/// accounts (after the script) with `data`, the signer it was given and the rest of its remaining
/// accounts ([`crate::mode::FORWARD`]).
pub fn forward(authority: Pubkey, key: Pubkey, which: u8, data: Vec<u8>) -> Instruction {
    set_answer(authority, key, which, crate::mode::FORWARD, data)
}

/// `set_answer` making `key`'s `before_transfer` a honeypot ([`crate::mode::HONEYPOT`]): a
/// transfer whose destination owner is `pool` and whose source owner is not `launch` is refused.
pub fn honeypot(authority: Pubkey, key: Pubkey, pool: Pubkey, launch: Pubkey) -> Instruction {
    let mut data = pool.to_bytes().to_vec();
    data.extend_from_slice(&launch.to_bytes());
    set_answer(
        authority,
        key,
        crate::callback::BEFORE_TRANSFER,
        crate::mode::HONEYPOT,
        data,
    )
}

/// `write_hook_data_as`: `signer` is `create_program_address(seeds, hook_tester)`.
pub fn write_hook_data_as(
    signer: Pubkey,
    seeds: Vec<Vec<u8>>,
    mint: Pubkey,
    holding: Pubkey,
    data: [u8; 64],
) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: vec![
            AccountMeta::new_readonly(signer, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(holding, false),
            AccountMeta::new_readonly(bordrless_token::ID, false),
            AccountMeta::new_readonly(token_client::event_authority(), false),
        ],
        data: crate::instruction::WriteHookDataAs { seeds, data }.data(),
    }
}

/// `route`: the DEX `swap` that `swap_client::swap(keys, args, extras)` builds, performed by CPI
/// from this program. The trader still signs the transaction.
pub fn route(
    keys: &swap_client::SwapKeys,
    args: SwapArgs,
    extras: Vec<AccountMeta>,
) -> Instruction {
    let swap = swap_client::swap(keys, args.clone(), extras);
    let mut accounts = vec![AccountMeta::new_readonly(bordrless_swap::ID, false)];
    accounts.extend(swap.accounts);
    Instruction {
        program_id: crate::ID,
        accounts,
        data: crate::instruction::Route { args }.data(),
    }
}

/// `write_hook_data_as` signed by this program's `["hook-authority"]` at its canonical bump: what
/// the token program accepts for a mint whose hook is this program.
pub fn write_hook_data(mint: Pubkey, holding: Pubkey, data: [u8; 64]) -> Instruction {
    let (signer, bump) = hook_authority();
    write_hook_data_as(
        signer,
        vec![HOOK_AUTHORITY_SEED.to_vec(), vec![bump]],
        mint,
        holding,
        data,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_derived() {
        let key = Pubkey::new_unique();
        let (authority, bump) = hook_authority();
        assert_eq!(
            Pubkey::create_program_address(&[HOOK_AUTHORITY_SEED, &[bump]], &crate::ID).unwrap(),
            authority
        );
        assert_eq!(
            registry_address(&key),
            hook_accounts_address(&crate::ID, &key).0
        );
        assert_ne!(script_address(&key), registry_address(&key));
        // The callers' signers for this hook.
        assert_eq!(
            crate::TOKEN_HOOK_SIGNER,
            token_client::hook_signer(&crate::ID)
        );
        assert_eq!(crate::DEX_HOOK_SIGNER, swap_client::hook_signer(&crate::ID));
    }
}
