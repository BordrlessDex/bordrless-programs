//! A test-only companion script (native, never deployed; `companion_payouts.rs`) whose steps answer
//! whatever its state says. The companion calls it with the creator address
//! `PDA(["creator", mint], companion)` signing, which it checks; no other account ever signs.
//!
//! - `companion_init` (`[creator (s, w), mint, state (w), system program, ...]`): the state is the
//!   companion's `PDA(["script-state", mint])`, made by the companion (zeroed, owned by this
//!   program). It stores the init data at its start.
//! - `companion_step` (`[creator (s, w), mint, launch, companion, pool, state (w), ...]`): does what
//!   the state says ([`state_bytes`]): nothing; sets the answer's bytes as its return data, exactly
//!   (so a malformed answer is as easy as a good one); buys `param` lamports of the token on the
//!   launch pool first, then answers (when the answer has bytes); or invokes the companion program
//!   with the bytes as its data (a re-entry the runtime refuses). Whatever a call needs (the swap's
//!   accounts, the companion program) is found among the accounts by key.
//!
//! The suites rewrite the state directly between steps.

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program::{invoke, set_return_data};
use bordrless_hook::{hook_accounts_address, HookAccountList};
use bordrless_launch::client as launch_client;
use bordrless_launch::state::Launch;
use bordrless_token::client as token_client;

/// The script's id (test-only; its keypair is not kept).
pub const ID: Pubkey = Pubkey::from_str_const("FYNpq7cScJhgG1E1xFr5PpnectoE2UxzCCA9ADtxe52G");
/// The companion, whose creator address signs every call, and which makes the script's state.
pub const COMPANION_ID: Pubkey =
    Pubkey::from_str_const("6ZUM1gWBH9hBBNoJoaVAGwSftyZ6CUda6vUZTW9MsJuo");
/// `sha256("global:companion_init")[..8]` and `sha256("global:companion_step")[..8]`.
pub const INIT: [u8; 8] = [0x76, 0xa1, 0xbb, 0xb4, 0x57, 0x56, 0xdc, 0x88];
pub const STEP: [u8; 8] = [0x5e, 0x15, 0xaa, 0xa1, 0xf6, 0x6e, 0x15, 0x28];
/// `PDA(["script-state", mint], companion)`: the script's state the companion makes for a mint.
pub const STATE_SEED: &[u8] = b"script-state";
/// The error of a call in which an account other than the creator signs.
pub const FOREIGN_SIGNER: u32 = 7_001;
/// The error of a call handed a state that is not the companion's for the mint, or not ours.
pub const BAD_STATE: u32 = 7_002;
/// The state's size: the mode, the parameter, the answer's length and up to 300 bytes of answer.
pub const STATE_LEN: u32 = 1 + 8 + 4 + 300;

/// Nothing: no return data.
pub const MODE_NOTHING: u8 = 0;
/// The answer's bytes as the return data, exactly.
pub const MODE_ANSWER: u8 = 1;
/// A buy of `param` lamports on the launch pool, then the answer (when it has bytes).
pub const MODE_BUY_THEN_ANSWER: u8 = 2;
/// A call into the companion program with the answer's bytes as its data and no accounts (the
/// runtime refuses the re-entry).
pub const MODE_REENTER: u8 = 3;

/// The script's state for `mint`, as the companion makes it.
pub fn state_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[STATE_SEED, mint.as_ref()], &COMPANION_ID).0
}

/// A state's bytes: the mode, the parameter, then the answer's length and bytes.
pub fn state_bytes(mode: u8, param: u64, answer: &[u8]) -> Vec<u8> {
    let mut d = vec![mode];
    d.extend_from_slice(&param.to_le_bytes());
    d.extend_from_slice(&(answer.len() as u32).to_le_bytes());
    d.extend_from_slice(answer);
    d
}

#[cfg(not(feature = "no-entrypoint"))]
anchor_lang::solana_program::entrypoint!(process);

/// The program's entrypoint.
pub fn process(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> core::result::Result<(), ProgramError> {
    if data.len() < 8 + 32 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let (disc, rest) = data.split_at(8);
    let (mint_bytes, rest) = rest.split_at(32);
    let mint = Pubkey::new_from_array(
        mint_bytes
            .try_into()
            .map_err(|_| ProgramError::InvalidInstructionData)?,
    );
    let creator = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let expected = Pubkey::find_program_address(&[b"creator", mint.as_ref()], &COMPANION_ID).0;
    if *creator.key != expected || !creator.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if accounts.iter().any(|a| a.is_signer && a.key != creator.key) {
        return Err(ProgramError::Custom(FOREIGN_SIGNER));
    }
    if disc == INIT {
        init(accounts, &mint, rest)
    } else if disc == STEP {
        step(accounts, &mint)
    } else {
        Err(ProgramError::InvalidInstructionData)
    }
}

fn init(
    accounts: &[AccountInfo],
    mint: &Pubkey,
    rest: &[u8],
) -> core::result::Result<(), ProgramError> {
    let len_bytes: [u8; 4] = rest
        .get(..4)
        .and_then(|b| b.try_into().ok())
        .ok_or(ProgramError::InvalidInstructionData)?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    let init_data = rest
        .get(4..4 + len)
        .ok_or(ProgramError::InvalidInstructionData)?;
    let [_creator, _mint, state, _system, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if *state.key != anchor_lang::system_program::ID {
        let mut data = state.try_borrow_mut_data()?;
        if *state.key != state_address(mint)
            || *state.owner != ID
            || !state.is_writable
            || data.iter().any(|b| *b != 0)
            || data.len() < init_data.len()
        {
            return Err(ProgramError::Custom(BAD_STATE));
        }
        data[..init_data.len()].copy_from_slice(init_data);
    }
    Ok(())
}

/// The state's mode, parameter and answer bytes.
fn parse_state(d: &[u8]) -> core::result::Result<(u8, u64, Vec<u8>), ProgramError> {
    let mode = *d.first().ok_or(ProgramError::InvalidAccountData)?;
    let param: [u8; 8] = d
        .get(1..9)
        .and_then(|b| b.try_into().ok())
        .ok_or(ProgramError::InvalidAccountData)?;
    let len: [u8; 4] = d
        .get(9..13)
        .and_then(|b| b.try_into().ok())
        .ok_or(ProgramError::InvalidAccountData)?;
    let len = u32::from_le_bytes(len) as usize;
    let answer = d
        .get(13..13 + len)
        .ok_or(ProgramError::InvalidAccountData)?
        .to_vec();
    Ok((mode, u64::from_le_bytes(param), answer))
}

fn step(accounts: &[AccountInfo], mint: &Pubkey) -> core::result::Result<(), ProgramError> {
    let [creator, _mint, launch_info, _companion, _pool, state, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if *state.key != state_address(mint) || *state.owner != ID {
        return Err(ProgramError::Custom(BAD_STATE));
    }
    let (mode, param, answer) = {
        let d = state.try_borrow_data()?;
        parse_state(&d)?
    };
    match mode {
        MODE_NOTHING => Ok(()),
        MODE_ANSWER => {
            set_return_data(&answer);
            Ok(())
        }
        MODE_BUY_THEN_ANSWER => {
            let launch = {
                let d = launch_info.try_borrow_data()?;
                Launch::try_deserialize(&mut &d[..])
                    .map_err(|_| ProgramError::InvalidAccountData)?
            };
            let keys = launch_client::LaunchKeys::of(&launch);
            let me = *creator.key;
            // A custom-hook token's swap carries the hook's slice, resolved from its registry
            // (among the accounts) for the pool vault's transfer to the creator, as the companion
            // resolves it for its own buys.
            let slice = match launch.custom_hook {
                None => vec![],
                Some(hook) => {
                    let registry_key = hook_accounts_address(&hook, mint).0;
                    let registry = accounts
                        .iter()
                        .find(|a| *a.key == registry_key)
                        .ok_or(ProgramError::NotEnoughAccountKeys)?;
                    let list = HookAccountList::decode(&registry.try_borrow_data()?)
                        .ok_or(ProgramError::InvalidAccountData)?;
                    let prefix = [
                        token_client::hook_signer(&hook),
                        *mint,
                        token_client::holding_address(mint, &launch.pool),
                        token_client::holding_address(mint, &me),
                        launch.pool,
                    ];
                    let extras = list
                        .resolve(&prefix, &launch.pool, &me)
                        .ok_or(ProgramError::InvalidAccountData)?;
                    launch_client::custom_hook_slice(hook, extras)
                }
            };
            let ix: Instruction =
                launch_client::swap_with_base_slice(&keys, me, me, 1, param, 1, slice);
            invoke(&ix, accounts)?;
            if !answer.is_empty() {
                set_return_data(&answer);
            }
            Ok(())
        }
        MODE_REENTER => {
            let ix = Instruction {
                program_id: COMPANION_ID,
                accounts: vec![],
                data: answer,
            };
            invoke(&ix, accounts)
        }
        _ => Err(ProgramError::InvalidInstructionData),
    }
}
