//! The one call a game hook makes: writing a holding's hook data outside a transfer, through the
//! token program's `write_hook_data`, signed by the hook's own `["hook-authority"]` PDA. A hook's
//! `enter` (registering a holding that has not traded this round or epoch) needs it; its callbacks
//! never call anything (they run at stack height 5 under a companion launch, Solana's limit, and
//! answer hook data in their return instead).
//!
//! Studio's checks allow this function, outside the callbacks, as a game hook's only CPI.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::invoke_signed;

use crate::HOOK_DATA_LEN;

/// The Bordrless token program.
pub const TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("2XoEWp8cF3kRXg74eVwPAyTFhVCAztn3V88komxAvr22");
/// The token program's event authority, `PDA(["__event_authority"], token program)`.
pub const TOKEN_EVENT_AUTHORITY: Pubkey =
    Pubkey::from_str_const("69vhpnkYjtiJgdfU7QsA5Ww7V8FWtvPcZ2r4znuyByvq");
/// A hook's authority seed: `PDA(["hook-authority"], hook)` signs the token program's
/// `write_hook_data` for the mints whose hook it is.
pub const HOOK_AUTHORITY_SEED: &[u8] = b"hook-authority";
/// Anchor's discriminator of the token program's `write_hook_data`:
/// `sha256("global:write_hook_data")[..8]`.
pub const WRITE_HOOK_DATA_DISCRIMINATOR: [u8; 8] = [38, 149, 238, 78, 60, 216, 173, 177];

/// `hook`'s authority, `PDA(["hook-authority"], hook)`, and its bump.
pub fn hook_authority_address(hook: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[HOOK_AUTHORITY_SEED], hook)
}

/// The token program's `write_hook_data(data)` for `holding` of `mint`, signed by `authority` (a
/// hook's `["hook-authority"]`): exactly what the token program's own client builds.
pub fn write_hook_data_ix(
    authority: Pubkey,
    mint: Pubkey,
    holding: Pubkey,
    data: [u8; HOOK_DATA_LEN],
) -> Instruction {
    let mut bytes = Vec::with_capacity(8 + HOOK_DATA_LEN);
    bytes.extend_from_slice(&WRITE_HOOK_DATA_DISCRIMINATOR);
    bytes.extend_from_slice(&data);
    Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(authority, true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(holding, false),
            AccountMeta::new_readonly(TOKEN_EVENT_AUTHORITY, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data: bytes,
    }
}

/// Writes `data` as `holding`'s hook data through the token program, signed by this program's
/// `["hook-authority"]` (`program_id` is the calling hook's own id, `crate::ID` in an Anchor
/// program). Refuses (`InvalidArgument`) a `hook_authority`, `token_program` or
/// `token_event_authority` that is not the expected one. The token program checks that the mint's
/// hook is `program_id` and writes hook data, and that the holding is of the mint. The only CPI a
/// game hook makes, and never from a callback.
pub fn write_own_hook_data<'info>(
    program_id: &Pubkey,
    hook_authority: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    holding: &AccountInfo<'info>,
    token_event_authority: &AccountInfo<'info>,
    token_program: &AccountInfo<'info>,
    data: [u8; HOOK_DATA_LEN],
) -> Result<()> {
    let (authority, bump) = hook_authority_address(program_id);
    if *hook_authority.key != authority
        || *token_program.key != TOKEN_PROGRAM_ID
        || *token_event_authority.key != TOKEN_EVENT_AUTHORITY
    {
        return Err(ProgramError::InvalidArgument.into());
    }
    let ix = write_hook_data_ix(authority, *mint.key, *holding.key, data);
    invoke_signed(
        &ix,
        &[
            hook_authority.clone(),
            mint.clone(),
            holding.clone(),
            token_event_authority.clone(),
            token_program.clone(),
        ],
        &[&[HOOK_AUTHORITY_SEED, &[bump]]],
    )?;
    Ok(())
}
