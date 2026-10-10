//! The upgradeable loader's (v3) instructions this program sends, built by hand (bincode: a u32
//! tag, no arguments), and the ones its clients send around it (buffers, extension).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use bordrless_hook::authority::BPF_LOADER_UPGRADEABLE_ID;

/// The rent sysvar.
pub const RENT_SYSVAR: Pubkey = Pubkey::from_str_const("SysvarRent111111111111111111111111111111111");
/// The clock sysvar.
pub const CLOCK_SYSVAR: Pubkey =
    Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111");

const INITIALIZE_BUFFER: u32 = 0;
const WRITE: u32 = 1;
const UPGRADE: u32 = 3;
const SET_AUTHORITY: u32 = 4;
const CLOSE: u32 = 5;
const EXTEND_PROGRAM: u32 = 6;
const SET_AUTHORITY_CHECKED: u32 = 7;

fn ix(accounts: Vec<AccountMeta>, data: Vec<u8>) -> Instruction {
    Instruction {
        program_id: BPF_LOADER_UPGRADEABLE_ID,
        accounts,
        data,
    }
}

/// `SetAuthorityChecked`: `account`'s authority moves from `current` to `new`, both signing.
pub fn set_authority_checked(account: Pubkey, current: Pubkey, new: Pubkey) -> Instruction {
    ix(
        vec![
            AccountMeta::new(account, false),
            AccountMeta::new_readonly(current, true),
            AccountMeta::new_readonly(new, true),
        ],
        SET_AUTHORITY_CHECKED.to_le_bytes().to_vec(),
    )
}

/// `SetAuthority`: `account`'s authority moves from `current` (signing) to `new` (`None`: a
/// program becomes immutable; a buffer can't).
pub fn set_authority(account: Pubkey, current: Pubkey, new: Option<Pubkey>) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new(account, false),
        AccountMeta::new_readonly(current, true),
    ];
    if let Some(new) = new {
        accounts.push(AccountMeta::new_readonly(new, false));
    }
    ix(accounts, SET_AUTHORITY.to_le_bytes().to_vec())
}

/// `Upgrade`: `program` takes `buffer`'s code, signed by `authority`; the buffer's lamports beyond
/// the ProgramData's rent go to `spill`.
pub fn upgrade(
    programdata: Pubkey,
    program: Pubkey,
    buffer: Pubkey,
    spill: Pubkey,
    authority: Pubkey,
) -> Instruction {
    ix(
        vec![
            AccountMeta::new(programdata, false),
            AccountMeta::new(program, false),
            AccountMeta::new(buffer, false),
            AccountMeta::new(spill, false),
            AccountMeta::new_readonly(RENT_SYSVAR, false),
            AccountMeta::new_readonly(CLOCK_SYSVAR, false),
            AccountMeta::new_readonly(authority, true),
        ],
        UPGRADE.to_le_bytes().to_vec(),
    )
}

/// `Close` of a buffer: its lamports to `recipient`, signed by its `authority`.
pub fn close_buffer(buffer: Pubkey, recipient: Pubkey, authority: Pubkey) -> Instruction {
    ix(
        vec![
            AccountMeta::new(buffer, false),
            AccountMeta::new(recipient, false),
            AccountMeta::new_readonly(authority, true),
        ],
        CLOSE.to_le_bytes().to_vec(),
    )
}

/// `InitializeBuffer`: `buffer` (already allocated to the loader) gets `authority`.
pub fn initialize_buffer(buffer: Pubkey, authority: Pubkey) -> Instruction {
    ix(
        vec![
            AccountMeta::new(buffer, false),
            AccountMeta::new_readonly(authority, false),
        ],
        INITIALIZE_BUFFER.to_le_bytes().to_vec(),
    )
}

/// `Write`: `bytes` at `offset` of `buffer`'s code, signed by its `authority`.
pub fn write(buffer: Pubkey, authority: Pubkey, offset: u32, bytes: Vec<u8>) -> Instruction {
    let mut data = WRITE.to_le_bytes().to_vec();
    data.extend_from_slice(&offset.to_le_bytes());
    data.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    data.extend_from_slice(&bytes);
    ix(
        vec![
            AccountMeta::new(buffer, false),
            AccountMeta::new_readonly(authority, true),
        ],
        data,
    )
}

/// `ExtendProgram` (permissionless): `programdata` grows by `additional_bytes` (at least 10,240),
/// `payer` paying the rent.
pub fn extend_program(
    programdata: Pubkey,
    program: Pubkey,
    payer: Pubkey,
    additional_bytes: u32,
) -> Instruction {
    let mut data = EXTEND_PROGRAM.to_le_bytes().to_vec();
    data.extend_from_slice(&additional_bytes.to_le_bytes());
    ix(
        vec![
            AccountMeta::new(programdata, false),
            AccountMeta::new(program, false),
            AccountMeta::new_readonly(anchor_lang::system_program::ID, false),
            AccountMeta::new(payer, true),
        ],
        data,
    )
}
