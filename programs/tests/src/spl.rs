//! Hand-built SPL Token instructions and account writers for the bridge tests.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use solana_account::Account;

use crate::env::{Env, SYSTEM_PROGRAM_ID};

/// SPL Token.
pub const TOKEN_PROGRAM_ID: Pubkey = bordrless_bridge::constants::TOKEN_PROGRAM_ID;
/// The ATA program.
pub const ATA_PROGRAM_ID: Pubkey = bordrless_bridge::constants::ATA_PROGRAM_ID;
/// Mint account length.
pub const MINT_LEN: usize = 82;
/// Token account length.
pub const TOKEN_ACCOUNT_LEN: usize = 165;

/// Packs an SPL mint.
pub fn pack_mint(authority: Option<Pubkey>, decimals: u8, supply: u64) -> Vec<u8> {
    let mut d = vec![0u8; MINT_LEN];
    match authority {
        Some(a) => {
            d[..4].copy_from_slice(&1u32.to_le_bytes());
            d[4..36].copy_from_slice(a.as_ref());
        }
        None => d[..4].copy_from_slice(&0u32.to_le_bytes()),
    }
    d[36..44].copy_from_slice(&supply.to_le_bytes());
    d[44] = decimals;
    d[45] = 1;
    d
}

/// Packs an SPL token account.
pub fn pack_token_account(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut d = vec![0u8; TOKEN_ACCOUNT_LEN];
    d[..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(owner.as_ref());
    d[64..72].copy_from_slice(&amount.to_le_bytes());
    d[108] = 1;
    d
}

/// The associated token account of `owner` for `mint`.
pub fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), TOKEN_PROGRAM_ID.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

/// `amount` of an SPL token account.
pub fn amount(env: &Env, key: &Pubkey) -> u64 {
    env.account(key).map_or(0, |a| {
        u64::from_le_bytes(a.data[64..72].try_into().unwrap())
    })
}

impl Env {
    /// Writes an SPL mint with `supply` already issued.
    pub fn set_spl_mint(
        &mut self,
        key: Pubkey,
        authority: Option<Pubkey>,
        decimals: u8,
        supply: u64,
    ) {
        let data = pack_mint(authority, decimals, supply);
        let lamports = self.rent(data.len());
        self.put(
            key,
            Account {
                lamports,
                data,
                owner: TOKEN_PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        );
    }

    /// Writes the ATA of `owner` for `mint` holding `amount`.
    pub fn set_spl_ata(&mut self, owner: &Pubkey, mint: &Pubkey, amount: u64) -> Pubkey {
        let key = ata(owner, mint);
        let data = pack_token_account(mint, owner, amount);
        let lamports = self.rent(data.len());
        self.put(
            key,
            Account {
                lamports,
                data,
                owner: TOKEN_PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        );
        key
    }
}

/// ATA `CreateIdempotent`.
pub fn create_ata(payer: Pubkey, owner: Pubkey, mint: Pubkey) -> Instruction {
    Instruction {
        program_id: ATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(ata(&owner, &mint), false),
            AccountMeta::new_readonly(owner, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data: vec![1],
    }
}
