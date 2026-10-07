//! Instruction builders for calling the bridge (tests and the TypeScript SDK's reference).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{system_program, InstructionData};
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::instructions::{ConfigArgs, RegisterArgs};
use crate::spl;
use crate::state::Wrapper;

/// This program's event authority.
pub fn event_authority() -> Pubkey {
    crate::EVENT_AUTHORITY_AND_BUMP.0
}

/// `["config"]`.
pub fn config_address() -> Pubkey {
    Pubkey::create_program_address(&[CONFIG_SEED, &[CONFIG_BUMP]], &crate::ID).expect("config bump")
}

/// `["sol-vault"]`.
pub fn sol_vault_address() -> Pubkey {
    Pubkey::create_program_address(&[SOL_VAULT_SEED, &[SOL_VAULT_BUMP]], &crate::ID)
        .expect("sol vault bump")
}

/// The wrapper of `underlying`.
pub fn wrapper_address(underlying: &Pubkey) -> Pubkey {
    Wrapper::address(underlying).0
}

/// The wrapped mint of `underlying`.
pub fn wrapped_mint_address(underlying: &Pubkey) -> Pubkey {
    Wrapper::wrapped_mint_address(underlying).0
}

/// The vault of an SPL wrapper.
pub fn vault_address(underlying: &Pubkey, underlying_program: &Pubkey) -> Pubkey {
    spl::associated_token_address(&wrapper_address(underlying), underlying, underlying_program)
}

/// The ProgramData account of this program.
pub fn program_data_address() -> Pubkey {
    Pubkey::find_program_address(&[crate::ID.as_ref()], &BPF_LOADER_UPGRADEABLE_ID).0
}

fn with_events(mut accounts: Vec<AccountMeta>) -> Vec<AccountMeta> {
    accounts.push(AccountMeta::new_readonly(event_authority(), false));
    accounts.push(AccountMeta::new_readonly(crate::ID, false));
    accounts
}

/// The token program and its event authority (a wrapped mint has no hook, so no hook signer).
fn token_fixed() -> [AccountMeta; 2] {
    [
        AccountMeta::new_readonly(bordrless_token::ID, false),
        AccountMeta::new_readonly(token_client::event_authority(), false),
    ]
}

/// `init_config`.
pub fn init_config(authority: Pubkey, args: ConfigArgs) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new(authority, true),
            AccountMeta::new(config_address(), false),
            AccountMeta::new_readonly(program_data_address(), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ]),
        data: crate::instruction::InitConfig { args }.data(),
    }
}

/// `set_config`.
pub fn set_config(admin: Pubkey, args: ConfigArgs) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new_readonly(admin, true),
            AccountMeta::new(config_address(), false),
        ]),
        data: crate::instruction::SetConfig { args }.data(),
    }
}

/// `register`.
pub fn register(
    payer: Pubkey,
    underlying: Pubkey,
    underlying_program: Pubkey,
    args: RegisterArgs,
) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(config_address(), false),
            AccountMeta::new_readonly(underlying, false),
            AccountMeta::new(wrapper_address(&underlying), false),
            AccountMeta::new(wrapped_mint_address(&underlying), false),
            AccountMeta::new(vault_address(&underlying, &underlying_program), false),
            AccountMeta::new_readonly(underlying_program, false),
            AccountMeta::new_readonly(ATA_PROGRAM_ID, false),
            AccountMeta::new_readonly(bordrless_token::ID, false),
            AccountMeta::new_readonly(token_client::event_authority(), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ]),
        data: crate::instruction::Register { args }.data(),
    }
}

/// `register_sol`.
pub fn register_sol(payer: Pubkey) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: with_events(vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(config_address(), false),
            AccountMeta::new(wrapper_address(&NATIVE_MINT), false),
            AccountMeta::new(wrapped_mint_address(&NATIVE_MINT), false),
            AccountMeta::new(sol_vault_address(), false),
            AccountMeta::new_readonly(bordrless_token::ID, false),
            AccountMeta::new_readonly(token_client::event_authority(), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ]),
        data: crate::instruction::RegisterSol {}.data(),
    }
}

fn wrap_accounts(
    user: Pubkey,
    underlying: Pubkey,
    underlying_program: Pubkey,
    user_underlying: Pubkey,
) -> Vec<AccountMeta> {
    let wrapped = wrapped_mint_address(&underlying);
    let mut accounts = vec![
        AccountMeta::new_readonly(user, true),
        AccountMeta::new_readonly(config_address(), false),
        AccountMeta::new(wrapper_address(&underlying), false),
        AccountMeta::new_readonly(underlying, false),
        AccountMeta::new(user_underlying, false),
        AccountMeta::new(vault_address(&underlying, &underlying_program), false),
        AccountMeta::new(wrapped, false),
        AccountMeta::new(token_client::holding_address(&wrapped, &user), false),
        AccountMeta::new_readonly(underlying_program, false),
    ];
    accounts.extend(token_fixed());
    with_events(accounts)
}

/// `wrap`.
pub fn wrap(
    user: Pubkey,
    underlying: Pubkey,
    underlying_program: Pubkey,
    user_underlying: Pubkey,
    amount: u64,
) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: wrap_accounts(user, underlying, underlying_program, user_underlying),
        data: crate::instruction::Wrap { amount }.data(),
    }
}

/// `unwrap`.
pub fn unwrap(
    user: Pubkey,
    underlying: Pubkey,
    underlying_program: Pubkey,
    user_underlying: Pubkey,
    amount: u64,
) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: wrap_accounts(user, underlying, underlying_program, user_underlying),
        data: crate::instruction::Unwrap { amount }.data(),
    }
}

fn wrap_sol_accounts(user: Pubkey) -> Vec<AccountMeta> {
    let wrapped = wrapped_mint_address(&NATIVE_MINT);
    let mut accounts = vec![
        AccountMeta::new(user, true),
        AccountMeta::new_readonly(config_address(), false),
        AccountMeta::new(wrapper_address(&NATIVE_MINT), false),
        AccountMeta::new(sol_vault_address(), false),
        AccountMeta::new(wrapped, false),
        AccountMeta::new(token_client::holding_address(&wrapped, &user), false),
    ];
    accounts.extend(token_fixed());
    accounts.push(AccountMeta::new_readonly(system_program::ID, false));
    with_events(accounts)
}

/// `wrap_sol`.
pub fn wrap_sol(user: Pubkey, lamports: u64) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: wrap_sol_accounts(user),
        data: crate::instruction::WrapSol { lamports }.data(),
    }
}

/// `unwrap_sol` (`u64::MAX` for the whole holding).
pub fn unwrap_sol(user: Pubkey, amount: u64) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: wrap_sol_accounts(user),
        data: crate::instruction::UnwrapSol { amount }.data(),
    }
}

/// `unwrap_sol_above`: everything above `keep`.
pub fn unwrap_sol_above(user: Pubkey, keep: u64) -> Instruction {
    Instruction {
        program_id: crate::ID,
        accounts: wrap_sol_accounts(user),
        data: crate::instruction::UnwrapSolAbove { keep }.data(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bumps_are_canonical() {
        assert_eq!(
            Pubkey::find_program_address(&[CONFIG_SEED], &crate::ID),
            (config_address(), CONFIG_BUMP)
        );
        assert_eq!(
            Pubkey::find_program_address(&[SOL_VAULT_SEED], &crate::ID),
            (sol_vault_address(), SOL_VAULT_BUMP)
        );
    }
}
