//! The SPL Token and Token-2022 calls and reads the kit's holder rewards on coins quoted in tokens
//! need (`docs/token-quotes.md` §7.5.1): a leg token's accounts read by their base layout, the
//! associated token account address, `TransferChecked` and the ATA program's `CreateIdempotent`,
//! encoded by hand (the DEX's `spl` module does the same; the kit depends on neither the DEX nor the
//! launchpad).
//!
//! Both token programs share the base layouts: a mint's decimals at 44 (82 bytes); a token
//! account's mint at 0, owner at 32, amount at 64 and state at 108 (165 bytes). A longer Token-2022
//! account has its account type at 165 (2 a token account) and its extensions from 166 as TLV
//! entries (a u16 type, a u16 length, the value), little-endian.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program::invoke_signed;

/// SPL Token.
pub const TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// Token-2022.
pub const TOKEN_2022_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
/// The associated token account program.
pub const ATA_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// A token account without extensions.
pub const ACCOUNT_LEN: usize = 165;
/// A multisig: never a token account.
const MULTISIG_LEN: usize = 355;
/// Token-2022 `ExtensionType::MemoTransfer`: its one byte says incoming transfers need a memo.
const EXT_MEMO_TRANSFER: u16 = 8;

/// The program of a leg's `quote_program` byte (0 SPL Token, 1 Token-2022).
pub fn program_id(index: u8) -> Pubkey {
    if index == 1 {
        TOKEN_2022_PROGRAM_ID
    } else {
        TOKEN_PROGRAM_ID
    }
}

/// The program byte of `program`, when it is one of the two token programs.
pub fn program_index(program: &Pubkey) -> Option<u8> {
    if *program == TOKEN_PROGRAM_ID {
        Some(0)
    } else if *program == TOKEN_2022_PROGRAM_ID {
        Some(1)
    } else {
        None
    }
}

/// The associated token account of `owner` for `mint` under `program`.
pub fn associated_address(owner: &Pubkey, mint: &Pubkey, program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

/// A token account's base fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenAccount {
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    /// 1 initialized, 2 frozen.
    pub state: u8,
}

/// `info` read as a token account of `program` (owned by it, initialized, not a mint or a
/// multisig).
pub fn read(info: &AccountInfo, program: &Pubkey) -> Option<TokenAccount> {
    if info.owner != program {
        return None;
    }
    let data = info.try_borrow_data().ok()?;
    let len = data.len();
    if len < ACCOUNT_LEN || len == MULTISIG_LEN || (len > ACCOUNT_LEN && data[ACCOUNT_LEN] != 2) {
        return None;
    }
    if data[108] == 0 {
        return None;
    }
    let key = |at: usize| Pubkey::new_from_array(data[at..at + 32].try_into().expect("32"));
    Some(TokenAccount {
        mint: key(0),
        owner: key(32),
        amount: u64::from_le_bytes(data[64..72].try_into().expect("8")),
        state: data[108],
    })
}

/// Whether `info` can receive `mint` for `owner` now: a token account of `program` of that mint and
/// owner, not frozen, and (Token-2022) not asking incoming transfers for a memo. A payment to one
/// that can't is left for later rather than failing every other payment with it.
pub fn receivable(info: &AccountInfo, program: &Pubkey, mint: &Pubkey, owner: &Pubkey) -> bool {
    let Some(account) = read(info, program) else {
        return false;
    };
    if account.mint != *mint || account.owner != *owner || account.state != 1 {
        return false;
    }
    let Ok(data) = info.try_borrow_data() else {
        return false;
    };
    let mut at = ACCOUNT_LEN + 1;
    while at + 4 <= data.len() {
        let kind = u16::from_le_bytes([data[at], data[at + 1]]);
        let len = usize::from(u16::from_le_bytes([data[at + 2], data[at + 3]]));
        if kind == EXT_MEMO_TRANSFER && len >= 1 && data.get(at + 4).is_some_and(|b| *b != 0) {
            return false;
        }
        if kind == 0 && len == 0 {
            break;
        }
        at += 4 + len;
    }
    true
}

/// `TransferChecked` (instruction 12) of `amount` from `source` to `destination`, signed by
/// `authority` (a PDA with `signer_seeds`, or a signer of the transaction with none).
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

/// The ATA program's `CreateIdempotent`: `ata` for `owner` and `mint` under `token_program`, `payer`
/// paying when it does not exist.
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
