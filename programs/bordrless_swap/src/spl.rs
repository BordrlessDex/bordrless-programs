//! Quote tokens (`docs/token-quotes.md` §2.1, §2.3): pools whose quote is a real SPL Token or
//! Token-2022 mint (quote kind 2, `Pool::quote_spl`), held in the pool's associated token account.
//! Here: the hand-encoded token-program and associated-token-account calls (the bridge's approach,
//! `bordrless_bridge::spl`: no crates), the reads of mints and token accounts, and the Token-2022
//! extension policy a quote mint is held to. The launchpad uses the same functions.
//!
//! Both token programs share the base layouts: a mint's decimals at 44 and `is_initialized` at 45
//! (82 bytes); a token account's mint at 0, owner at 32, amount at 64 and state at 108 (165 bytes).
//! A Token-2022 account with extensions is longer: its account type at 165 (1 a mint, 2 a token
//! account) and its extensions from 166 as TLV entries (a u16 type, a u16 length, the value), all
//! little-endian; a mint's bytes 82 to 165 are zero padding.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::Instruction,
    program::{get_return_data, invoke_signed},
};

use crate::error::SwapError;
use crate::state::Pool;

/// SPL Token.
pub const TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// Token-2022.
pub const TOKEN_2022_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
/// The associated token account program.
pub const ATA_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// Wrapped SOL under SPL Token (refused as a launch's quote by the launchpad: SOL launches stay SOL
/// launches).
pub const NATIVE_MINT: Pubkey =
    Pubkey::from_str_const("So11111111111111111111111111111111111111112");
/// Wrapped SOL under Token-2022.
pub const NATIVE_MINT_2022: Pubkey =
    Pubkey::from_str_const("9pan9bMn5HatX4EJdBwg9VgCa7Uz5HL8N1m5D3NdXejP");

/// `Pool.reserved[SPL_PROGRAM_OFFSET]`: SPL Token.
pub const PROGRAM_SPL_TOKEN: u8 = 0;
/// `Pool.reserved[SPL_PROGRAM_OFFSET]`: Token-2022.
pub const PROGRAM_TOKEN_2022: u8 = 1;

/// A mint without extensions.
pub const MINT_LEN: usize = 82;
/// A token account without extensions (and where the account type sits on longer accounts).
pub const ACCOUNT_LEN: usize = 165;
/// A multisig: never a mint or a token account.
const MULTISIG_LEN: usize = 355;
/// Token-2022 `AccountType::Mint`.
const ACCOUNT_TYPE_MINT: u8 = 1;
/// Token-2022 `AccountType::Account`.
const ACCOUNT_TYPE_ACCOUNT: u8 = 2;

/// Token-2022 `ExtensionType` numbers (spl-token-2022-interface 3.1.2 `extension::ExtensionType`,
/// `#[repr(u16)]`, in declaration order) that the policy names. Mirrored by
/// `packages/shared/src/quotes.ts` `TOKEN_2022_MINT_EXTENSIONS`.
pub mod ext {
    /// Refused: a fee on every transfer.
    pub const TRANSFER_FEE_CONFIG: u16 = 1;
    /// Allowed (label: the issuer may close the mint once empty).
    pub const MINT_CLOSE_AUTHORITY: u16 = 3;
    /// Allowed.
    pub const CONFIDENTIAL_TRANSFER_MINT: u16 = 4;
    /// Allowed only when new accounts start `Initialized` (1); `Frozen` is refused.
    pub const DEFAULT_ACCOUNT_STATE: u16 = 6;
    /// Refused.
    pub const NON_TRANSFERABLE: u16 = 9;
    /// Allowed (label: scaled amounts).
    pub const INTEREST_BEARING_CONFIG: u16 = 10;
    /// Allowed (label: the issuer can move tokens out of any account, the pool's included).
    pub const PERMANENT_DELEGATE: u16 = 12;
    /// Allowed only with no program set (label: the issuer may set one later, which stops the
    /// pool's transfers until it is removed).
    pub const TRANSFER_HOOK: u16 = 14;
    /// Refused (it comes with a transfer fee).
    pub const CONFIDENTIAL_TRANSFER_FEE_CONFIG: u16 = 16;
    /// Allowed.
    pub const METADATA_POINTER: u16 = 18;
    /// Allowed.
    pub const TOKEN_METADATA: u16 = 19;
    /// Allowed.
    pub const GROUP_POINTER: u16 = 20;
    /// Allowed.
    pub const TOKEN_GROUP: u16 = 21;
    /// Allowed.
    pub const GROUP_MEMBER_POINTER: u16 = 22;
    /// Allowed.
    pub const TOKEN_GROUP_MEMBER: u16 = 23;
    /// Refused.
    pub const CONFIDENTIAL_MINT_BURN: u16 = 24;
    /// Allowed (label: scaled amounts).
    pub const SCALED_UI_AMOUNT: u16 = 25;
    /// Allowed only while not paused (label: the issuer can pause every transfer).
    pub const PAUSABLE: u16 = 26;
}

/// Why a mint can't be a pool's quote (`docs/token-quotes.md` §2.3). Each program maps these to
/// its own error codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MintRefusal {
    /// Not an initialized SPL Token or Token-2022 mint, or its extensions do not parse.
    NotAMint,
    /// `TransferFeeConfig` or `ConfidentialTransferFeeConfig`.
    TransferFee,
    /// `NonTransferable`.
    NonTransferable,
    /// `DefaultAccountState` other than `Initialized`.
    FrozenByDefault,
    /// `TransferHook` with a program set.
    TransferHook,
    /// `Pausable`, paused.
    Paused,
    /// `ConfidentialMintBurn`.
    Confidential,
    /// Any other extension number (an account extension on a mint, one this policy predates).
    UnknownExtension,
}

impl From<MintRefusal> for SwapError {
    fn from(r: MintRefusal) -> Self {
        match r {
            MintRefusal::NotAMint => SwapError::QuoteMintInvalid,
            MintRefusal::TransferFee => SwapError::QuoteMintTransferFee,
            MintRefusal::NonTransferable => SwapError::QuoteMintNonTransferable,
            MintRefusal::FrozenByDefault => SwapError::QuoteMintFrozenByDefault,
            MintRefusal::TransferHook => SwapError::QuoteMintTransferHook,
            MintRefusal::Paused => SwapError::QuoteMintPaused,
            MintRefusal::Confidential => SwapError::QuoteMintConfidential,
            MintRefusal::UnknownExtension => SwapError::QuoteMintUnknownExtension,
        }
    }
}

/// The program index (`PROGRAM_SPL_TOKEN`, `PROGRAM_TOKEN_2022`) of a token program's id.
pub fn program_index(program: &Pubkey) -> Option<u8> {
    if *program == TOKEN_PROGRAM_ID {
        Some(PROGRAM_SPL_TOKEN)
    } else if *program == TOKEN_2022_PROGRAM_ID {
        Some(PROGRAM_TOKEN_2022)
    } else {
        None
    }
}

/// The token program of a program index (Token-2022 for 1, SPL Token otherwise).
pub fn program_id(index: u8) -> Pubkey {
    if index == PROGRAM_TOKEN_2022 {
        TOKEN_2022_PROGRAM_ID
    } else {
        TOKEN_PROGRAM_ID
    }
}

fn read_key(data: &[u8], at: usize) -> Option<Pubkey> {
    let bytes: [u8; 32] = data.get(at..at + 32)?.try_into().ok()?;
    Some(Pubkey::new_from_array(bytes))
}

fn read_u64(data: &[u8], at: usize) -> Option<u64> {
    let bytes: [u8; 8] = data.get(at..at + 8)?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

/// One extension of a quote mint against the policy.
fn check_extension(kind: u16, value: &[u8]) -> core::result::Result<(), MintRefusal> {
    match kind {
        ext::TRANSFER_FEE_CONFIG | ext::CONFIDENTIAL_TRANSFER_FEE_CONFIG => {
            Err(MintRefusal::TransferFee)
        }
        ext::NON_TRANSFERABLE => Err(MintRefusal::NonTransferable),
        ext::CONFIDENTIAL_MINT_BURN => Err(MintRefusal::Confidential),
        // `AccountState::Initialized` is 1 (0 uninitialized, 2 frozen).
        ext::DEFAULT_ACCOUNT_STATE => match value.first() {
            Some(1) => Ok(()),
            Some(_) => Err(MintRefusal::FrozenByDefault),
            None => Err(MintRefusal::NotAMint),
        },
        // `TransferHook { authority, program_id }`: no program is 32 zero bytes.
        ext::TRANSFER_HOOK => match value.get(32..64) {
            Some(program) if program.iter().all(|b| *b == 0) => Ok(()),
            Some(_) => Err(MintRefusal::TransferHook),
            None => Err(MintRefusal::NotAMint),
        },
        // `PausableConfig { authority, paused: bool }`.
        ext::PAUSABLE => match value.get(32) {
            Some(0) => Ok(()),
            Some(_) => Err(MintRefusal::Paused),
            None => Err(MintRefusal::NotAMint),
        },
        ext::MINT_CLOSE_AUTHORITY
        | ext::CONFIDENTIAL_TRANSFER_MINT
        | ext::INTEREST_BEARING_CONFIG
        | ext::PERMANENT_DELEGATE
        | ext::METADATA_POINTER
        | ext::TOKEN_METADATA
        | ext::GROUP_POINTER
        | ext::TOKEN_GROUP
        | ext::GROUP_MEMBER_POINTER
        | ext::TOKEN_GROUP_MEMBER
        | ext::SCALED_UI_AMOUNT => Ok(()),
        _ => Err(MintRefusal::UnknownExtension),
    }
}

/// The policy on a mint's data (`token_2022`: the mint is Token-2022's): an initialized mint, and
/// for Token-2022 every extension allowed (§2.3), its TLV read as Token-2022 reads it (the entries
/// end at an `Uninitialized` type or when fewer than two bytes are left; a length past the end is
/// malformed). Answers the mint's decimals.
pub fn check_mint_data(data: &[u8], token_2022: bool) -> core::result::Result<u8, MintRefusal> {
    if data.len() < MINT_LEN || data[45] != 1 {
        return Err(MintRefusal::NotAMint);
    }
    let decimals = data[44];
    if data.len() == MINT_LEN {
        return Ok(decimals);
    }
    // Longer than a plain mint: only a Token-2022 mint with extensions.
    if !token_2022
        || data.len() <= ACCOUNT_LEN
        || data.len() == MULTISIG_LEN
        || data[MINT_LEN..ACCOUNT_LEN].iter().any(|b| *b != 0)
        || data[ACCOUNT_LEN] != ACCOUNT_TYPE_MINT
    {
        return Err(MintRefusal::NotAMint);
    }
    let tlv = &data[ACCOUNT_LEN + 1..];
    let mut at = 0usize;
    while at + 2 <= tlv.len() {
        let kind = u16::from_le_bytes([tlv[at], tlv[at + 1]]);
        if kind == 0 {
            break;
        }
        let len = tlv
            .get(at + 2..at + 4)
            .map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
            .ok_or(MintRefusal::NotAMint)?;
        let value = tlv.get(at + 4..at + 4 + len).ok_or(MintRefusal::NotAMint)?;
        check_extension(kind, value)?;
        at += 4 + len;
    }
    Ok(decimals)
}

/// A quote mint, checked: owned by SPL Token or Token-2022 and allowed by the policy. Answers its
/// program index and decimals.
pub fn check_quote_mint(mint: &AccountInfo) -> core::result::Result<(u8, u8), MintRefusal> {
    let program = program_index(mint.owner).ok_or(MintRefusal::NotAMint)?;
    let data = mint.try_borrow_data().map_err(|_| MintRefusal::NotAMint)?;
    let decimals = check_mint_data(&data, program == PROGRAM_TOKEN_2022)?;
    Ok((program, decimals))
}

/// The base fields of a token account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenAccount {
    /// Its mint.
    pub mint: Pubkey,
    /// Its owner.
    pub owner: Pubkey,
    /// Its balance.
    pub amount: u64,
}

/// A token account of `program` (owned by it, initialized, a token account rather than a mint or
/// a multisig), read.
pub fn read_token_account(info: &AccountInfo, program: &Pubkey) -> Option<TokenAccount> {
    if info.owner != program {
        return None;
    }
    let data = info.try_borrow_data().ok()?;
    let len = data.len();
    if len < ACCOUNT_LEN || len == MULTISIG_LEN {
        return None;
    }
    if len > ACCOUNT_LEN && data[ACCOUNT_LEN] != ACCOUNT_TYPE_ACCOUNT {
        return None;
    }
    // `AccountState`: 0 uninitialized, 1 initialized, 2 frozen.
    if data[108] == 0 {
        return None;
    }
    Some(TokenAccount {
        mint: read_key(&data, 0)?,
        owner: read_key(&data, 32)?,
        amount: read_u64(&data, 64)?,
    })
}

/// The associated token account of `owner` for `mint` under `program`.
pub fn associated_token_address(owner: &Pubkey, mint: &Pubkey, program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

/// ATA `CreateIdempotent`: `ata` for `owner` and `mint` under `token_program`, `payer` paying the
/// rent when it does not exist yet. The ATA program checks the address and, for an existing
/// account, its owner.
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
    require_keys_eq!(
        *ata_program.key,
        ATA_PROGRAM_ID,
        anchor_lang::error::ErrorCode::InvalidProgramId
    );
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

/// `TransferChecked` (instruction 12: the amount, then the decimals) of `amount` from `source` to
/// `destination`, signed by `authority` (a signer of the transaction, or a PDA with
/// `signer_seeds`).
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

/// `GetAccountDataSize` (instruction 21, no extra extensions): the size a token account of `mint`
/// needs (165 under SPL Token; under Token-2022 with the account extensions the mint requires).
pub fn account_data_size<'info>(
    token_program: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
) -> Result<u64> {
    let ix = Instruction {
        program_id: *token_program.key,
        accounts: vec![AccountMeta::new_readonly(*mint.key, false)],
        data: vec![21],
    };
    invoke_signed(&ix, &[mint.clone(), token_program.clone()], &[])?;
    match get_return_data() {
        Some((program, bytes)) if program == *token_program.key && bytes.len() == 8 => {
            Ok(u64::from_le_bytes(bytes.try_into().expect("8 bytes")))
        }
        _ => err!(SwapError::QuoteMintInvalid),
    }
}

/// `InitializeAccount3` (instruction 18, the owner in the data) of `account` (allocated and
/// assigned to the token program already) for `mint`, owned by `owner`.
pub fn initialize_account3<'info>(
    token_program: &AccountInfo<'info>,
    account: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    owner: &Pubkey,
) -> Result<()> {
    let mut data = Vec::with_capacity(33);
    data.push(18);
    data.extend_from_slice(owner.as_ref());
    let ix = Instruction {
        program_id: *token_program.key,
        accounts: vec![
            AccountMeta::new(*account.key, false),
            AccountMeta::new_readonly(*mint.key, false),
        ],
        data,
    };
    invoke_signed(
        &ix,
        &[account.clone(), mint.clone(), token_program.clone()],
        &[],
    )
    .map_err(Into::into)
}

/// The SPL side of a `swap_spl`: the quote mint's program and decimals.
pub struct SplLeg<'info> {
    /// SPL Token or Token-2022 (the pool's, address-checked).
    pub token_program: AccountInfo<'info>,
    /// The quote mint's decimals (the pool's).
    pub decimals: u8,
}

impl<'info> SplLeg<'info> {
    /// A token account of `mint` under the leg's program, read (`WrongTokenAccount` otherwise).
    pub fn account(&self, info: &AccountInfo, mint: &Pubkey) -> Result<TokenAccount> {
        let account =
            read_token_account(info, self.token_program.key).ok_or(SwapError::WrongTokenAccount)?;
        require_keys_eq!(account.mint, *mint, SwapError::WrongTokenAccount);
        Ok(account)
    }

    /// The balance of a token account of the leg's program.
    pub fn amount(&self, info: &AccountInfo) -> Result<u64> {
        Ok(read_token_account(info, self.token_program.key)
            .ok_or(SwapError::WrongTokenAccount)?
            .amount)
    }

    /// `transfer_checked` of `amount` (nothing for 0).
    #[allow(clippy::too_many_arguments)]
    pub fn transfer(
        &self,
        authority: &AccountInfo<'info>,
        source: &AccountInfo<'info>,
        destination: &AccountInfo<'info>,
        mint: &AccountInfo<'info>,
        amount: u64,
        signer_seeds: &[&[&[u8]]],
    ) -> Result<()> {
        if amount == 0 {
            return Ok(());
        }
        transfer_checked(
            &self.token_program,
            source,
            mint,
            destination,
            authority,
            amount,
            self.decimals,
            signer_seeds,
        )
    }
}

/// The invariant of a kind-2 pool, checked at the end of every instruction that moves its quote:
/// its quote vault holds at least its books, `quote_reserve + protocol_fees_quote` (exactly, unless
/// someone sent the vault tokens: a transfer fee is refused at creation).
pub fn check_books(pool: &Pool, vault_amount: u64) -> Result<()> {
    let books = pool.quote_books().ok_or(SwapError::MathOverflow)?;
    require!(vault_amount >= books, SwapError::SplInvariant);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Token-2022 mint with `extensions` (type, value), `decimals`, initialized.
    fn mint_2022(extensions: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut d = vec![0u8; MINT_LEN];
        d[44] = 6;
        d[45] = 1;
        if extensions.is_empty() {
            return d;
        }
        d.resize(ACCOUNT_LEN, 0);
        d.push(ACCOUNT_TYPE_MINT);
        for (kind, value) in extensions {
            d.extend_from_slice(&kind.to_le_bytes());
            d.extend_from_slice(&(value.len() as u16).to_le_bytes());
            d.extend_from_slice(value);
        }
        d
    }

    #[test]
    fn the_policy_reads_extensions_as_token_2022_does() {
        // Plain mints of either program.
        assert_eq!(check_mint_data(&mint_2022(&[]), false), Ok(6));
        assert_eq!(check_mint_data(&mint_2022(&[]), true), Ok(6));
        // Uninitialized.
        let mut d = mint_2022(&[]);
        d[45] = 0;
        assert_eq!(check_mint_data(&d, true), Err(MintRefusal::NotAMint));
        // xStocks-like: permanent delegate, pausable (not paused), a hook with no program, scaled
        // amounts, metadata.
        let x = mint_2022(&[
            (ext::METADATA_POINTER, vec![1; 64]),
            (ext::PERMANENT_DELEGATE, vec![2; 32]),
            (ext::TRANSFER_HOOK, [vec![3; 32], vec![0; 32]].concat()),
            (ext::PAUSABLE, [vec![4; 32], vec![0]].concat()),
            (ext::SCALED_UI_AMOUNT, vec![5; 56]),
            (ext::TOKEN_METADATA, vec![6; 90]),
        ]);
        assert_eq!(check_mint_data(&x, true), Ok(6));
        // An SPL Token mint is never longer than 82 bytes.
        assert_eq!(check_mint_data(&x, false), Err(MintRefusal::NotAMint));
        let refused = [
            (
                ext::TRANSFER_FEE_CONFIG,
                vec![0; 108],
                MintRefusal::TransferFee,
            ),
            (
                ext::CONFIDENTIAL_TRANSFER_FEE_CONFIG,
                vec![0; 129],
                MintRefusal::TransferFee,
            ),
            (ext::NON_TRANSFERABLE, vec![], MintRefusal::NonTransferable),
            (
                ext::DEFAULT_ACCOUNT_STATE,
                vec![2],
                MintRefusal::FrozenByDefault,
            ),
            (
                ext::TRANSFER_HOOK,
                [vec![0; 32], vec![9; 32]].concat(),
                MintRefusal::TransferHook,
            ),
            (
                ext::PAUSABLE,
                [vec![0; 32], vec![1]].concat(),
                MintRefusal::Paused,
            ),
            (
                ext::CONFIDENTIAL_MINT_BURN,
                vec![0; 196],
                MintRefusal::Confidential,
            ),
            (2, vec![0; 8], MintRefusal::UnknownExtension),
            (28, vec![0; 32], MintRefusal::UnknownExtension),
            (500, vec![], MintRefusal::UnknownExtension),
        ];
        for (kind, value, why) in refused {
            assert_eq!(
                check_mint_data(&mint_2022(&[(kind, value)]), true),
                Err(why),
                "{kind}"
            );
        }
        assert_eq!(
            check_mint_data(&mint_2022(&[(ext::DEFAULT_ACCOUNT_STATE, vec![1])]), true),
            Ok(6)
        );
        // Malformed: a length past the end; nonzero padding; not a mint's account type; a multisig.
        let mut bad = mint_2022(&[(ext::PERMANENT_DELEGATE, vec![2; 32])]);
        bad.truncate(bad.len() - 1);
        assert_eq!(check_mint_data(&bad, true), Err(MintRefusal::NotAMint));
        let mut bad = mint_2022(&[(ext::PERMANENT_DELEGATE, vec![2; 32])]);
        bad[100] = 1;
        assert_eq!(check_mint_data(&bad, true), Err(MintRefusal::NotAMint));
        let mut bad = mint_2022(&[(ext::PERMANENT_DELEGATE, vec![2; 32])]);
        bad[ACCOUNT_LEN] = ACCOUNT_TYPE_ACCOUNT;
        assert_eq!(check_mint_data(&bad, true), Err(MintRefusal::NotAMint));
        let mut multisig = mint_2022(&[]);
        multisig.resize(MULTISIG_LEN, 0);
        assert_eq!(check_mint_data(&multisig, true), Err(MintRefusal::NotAMint));
        // The entries end at an `Uninitialized` type (what follows is never read) or one byte
        // short of a type.
        let mut end = mint_2022(&[(ext::PERMANENT_DELEGATE, vec![2; 32])]);
        end.extend_from_slice(&[0, 0, 9, 9, 1]);
        assert_eq!(check_mint_data(&end, true), Ok(6));
        let mut short = mint_2022(&[(ext::PERMANENT_DELEGATE, vec![2; 32])]);
        short.push(1);
        assert_eq!(check_mint_data(&short, true), Ok(6));
    }

    #[test]
    fn program_indices_round_trip() {
        assert_eq!(program_index(&TOKEN_PROGRAM_ID), Some(PROGRAM_SPL_TOKEN));
        assert_eq!(
            program_index(&TOKEN_2022_PROGRAM_ID),
            Some(PROGRAM_TOKEN_2022)
        );
        assert_eq!(program_index(&crate::ID), None);
        assert_eq!(program_id(PROGRAM_SPL_TOKEN), TOKEN_PROGRAM_ID);
        assert_eq!(program_id(PROGRAM_TOKEN_2022), TOKEN_2022_PROGRAM_ID);
    }

    #[test]
    fn the_token_ids_are_the_bridges() {
        use bordrless_bridge::constants as b;
        assert_eq!(TOKEN_PROGRAM_ID, b::TOKEN_PROGRAM_ID);
        assert_eq!(TOKEN_2022_PROGRAM_ID, b::TOKEN_2022_PROGRAM_ID);
        assert_eq!(ATA_PROGRAM_ID, b::ATA_PROGRAM_ID);
        assert_eq!(NATIVE_MINT, b::NATIVE_MINT);
    }
}
