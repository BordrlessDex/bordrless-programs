//! Hand-encoded SPL Token / Token-2022 and associated-token-account calls and reads. Both token
//! programs share the base layouts: a mint's decimals at offset 44, a token account's mint at 0,
//! owner at 32 and amount at 64.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{instruction::Instruction, program::invoke_signed};

use crate::constants::*;
use crate::error::BridgeError;

fn read_u64(data: &[u8], at: usize) -> Result<u64> {
    let bytes = data
        .get(at..at + 8)
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .ok_or(BridgeError::WrongTokenAccount)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_key(data: &[u8], at: usize) -> Result<Pubkey> {
    let bytes = data
        .get(at..at + 32)
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .ok_or(BridgeError::WrongTokenAccount)?;
    Ok(Pubkey::new_from_array(bytes))
}

/// Whether `program` is one of the two token programs.
pub fn is_token_program(program: &Pubkey) -> bool {
    *program == TOKEN_PROGRAM_ID || *program == TOKEN_2022_PROGRAM_ID
}

/// Decimals of a mint owned by a token program.
pub fn mint_decimals(mint: &AccountInfo) -> Result<u8> {
    require!(is_token_program(mint.owner), BridgeError::NotAMint);
    let data = mint.try_borrow_data()?;
    require!(data.len() >= 82, BridgeError::NotAMint);
    Ok(data[44])
}

/// `amount` of a token account.
pub fn amount(account: &AccountInfo) -> Result<u64> {
    read_u64(&account.try_borrow_data()?, 64)
}

/// Checks a token account belongs to `program`, holds `mint` and is owned by `owner`.
pub fn check_token_account(
    account: &AccountInfo,
    program: &Pubkey,
    mint: &Pubkey,
    owner: Option<&Pubkey>,
) -> Result<()> {
    require_keys_eq!(*account.owner, *program, BridgeError::WrongTokenProgram);
    let data = account.try_borrow_data()?;
    require!(data.len() >= 165, BridgeError::WrongTokenAccount);
    require_keys_eq!(read_key(&data, 0)?, *mint, BridgeError::WrongTokenAccount);
    if let Some(owner) = owner {
        require_keys_eq!(read_key(&data, 32)?, *owner, BridgeError::WrongTokenAccount);
    }
    Ok(())
}

/// The associated token account of `owner` for `mint` under `program`.
pub fn associated_token_address(owner: &Pubkey, mint: &Pubkey, program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

/// ATA `CreateIdempotent`.
#[allow(clippy::too_many_arguments)]
pub fn create_ata_idempotent<'info>(
    payer: &AccountInfo<'info>,
    ata: &AccountInfo<'info>,
    owner: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    ata_program: &AccountInfo<'info>,
) -> Result<()> {
    let ix = Instruction {
        program_id: ATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new(*ata.key, false),
            AccountMeta::new_readonly(*owner.key, false),
            AccountMeta::new_readonly(*mint.key, false),
            AccountMeta::new_readonly(*system_program.key, false),
            AccountMeta::new_readonly(*token_program.key, false),
        ],
        data: vec![1],
    };
    invoke_signed(
        &ix,
        &[
            payer.clone(),
            ata.clone(),
            owner.clone(),
            mint.clone(),
            system_program.clone(),
            token_program.clone(),
            ata_program.clone(),
        ],
        &[],
    )
    .map_err(Into::into)
}

/// `TransferChecked`.
#[allow(clippy::too_many_arguments)]
pub fn transfer_checked<'info>(
    token_program: &AccountInfo<'info>,
    source: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    destination: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    amount: u64,
    decimals: u8,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    let mut data = Vec::with_capacity(10);
    data.push(12);
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    let ix = Instruction {
        program_id: *token_program.key,
        accounts: vec![
            AccountMeta::new(*source.key, false),
            AccountMeta::new_readonly(*mint.key, false),
            AccountMeta::new(*destination.key, false),
            AccountMeta::new_readonly(*authority.key, true),
        ],
        data,
    };
    invoke_signed(
        &ix,
        &[
            source.clone(),
            mint.clone(),
            destination.clone(),
            authority.clone(),
            token_program.clone(),
        ],
        signer_seeds,
    )
    .map_err(Into::into)
}
