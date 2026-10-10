//! A test-only strategy (native, so the suites can load it at any id): `plan` and `entitle` answer
//! as its config account says. The config is the first extra account (owned by this program):
//! `[plan_mode u8, plan_param u64, entitle_mode u8, entitle_param u64]`.

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::{invoke, set_return_data};
use bordrless_strategy::{pro_rata, EntitleArgs, PlanArgs, ENTITLE, PLAN};

/// The tester's id (test-only; its keypair is not kept).
pub const ID: Pubkey = Pubkey::from_str_const("wSdaAfQoqv79cR6yWbRyScM8FSdqFwXpHWFQRVYJymX");

/// Answer pro rata: `plan` answers `param` basis points of `budget_max` (0: all of it), `entitle`
/// `budget * weight / total`, at most `max_amount`.
pub const MODE_PRO_RATA: u8 = 0;
/// Fail with an error.
pub const MODE_ERROR: u8 = 1;
/// Loop until the compute runs out.
pub const MODE_LOOP: u8 = 2;
/// Answer 7 bytes.
pub const MODE_SHORT: u8 = 3;
/// Answer 9 bytes.
pub const MODE_LONG: u8 = 4;
/// Answer nothing.
pub const MODE_NONE: u8 = 5;
/// Answer `u64::MAX`.
pub const MODE_MAX: u8 = 6;
/// Burn about `param` compute units, then answer pro rata.
pub const MODE_BURN: u8 = 7;
/// Write to the config account (passed read-only).
pub const MODE_WRITE: u8 = 8;
/// Answer `param` (a flat amount, or a flat budget).
pub const MODE_FLAT: u8 = 9;
/// Call itself (recursion), the inner call answering pro rata.
pub const MODE_RECURSE: u8 = 10;
/// Call the companion (not passed: refused by the runtime).
pub const MODE_CALL_COMPANION: u8 = 11;
/// Fail unless every account came read-only and none signed.
pub const MODE_CHECK_PRIVILEGES: u8 = 12;
/// Answer pro rata, but an extra amount for a fixed favourite owner (`param` as the extra).
pub const MODE_FAVOURITE: u8 = 13;

/// The favourite of `MODE_FAVOURITE`.
pub const FAVOURITE: Pubkey = Pubkey::from_str_const("Fav1111111111111111111111111111111111111111");

/// The config account's bytes.
pub fn config_bytes(plan_mode: u8, plan_param: u64, entitle_mode: u8, entitle_param: u64) -> Vec<u8> {
    let mut d = vec![plan_mode];
    d.extend_from_slice(&plan_param.to_le_bytes());
    d.push(entitle_mode);
    d.extend_from_slice(&entitle_param.to_le_bytes());
    d
}

#[cfg(not(feature = "no-entrypoint"))]
anchor_lang::solana_program::entrypoint!(process);

fn answer(value: u64) {
    set_return_data(&value.to_le_bytes());
}

fn burn(units: u64) -> u64 {
    // About 10 compute units an iteration.
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    for i in 0..units / 10 {
        x ^= x << 13;
        x ^= x >> 7;
        x = x.wrapping_add(i);
        x = core::hint::black_box(x);
    }
    x
}

/// The program's entrypoint.
pub fn process(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> core::result::Result<(), ProgramError> {
    if data.len() < 8 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let (disc, rest) = data.split_at(8);
    let is_plan = disc == PLAN;
    if !is_plan && disc != ENTITLE {
        return Err(ProgramError::InvalidInstructionData);
    }
    // The config: the first account after the 5-account prefix.
    let config = accounts.get(5).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let (mode, param) = {
        let c = config.try_borrow_data()?;
        if c.len() < 18 {
            return Err(ProgramError::InvalidAccountData);
        }
        if is_plan {
            (c[0], u64::from_le_bytes(c[1..9].try_into().unwrap()))
        } else {
            (c[9], u64::from_le_bytes(c[10..18].try_into().unwrap()))
        }
    };
    let pro = |rest: &[u8]| -> core::result::Result<u64, ProgramError> {
        if is_plan {
            let a = PlanArgs::try_from_slice(rest).map_err(|_| ProgramError::InvalidArgument)?;
            Ok(if param == 0 || mode != MODE_PRO_RATA {
                a.budget_max
            } else {
                pro_rata(a.budget_max, param, 10_000)
            })
        } else {
            let a = EntitleArgs::try_from_slice(rest).map_err(|_| ProgramError::InvalidArgument)?;
            // A well-written strategy keeps within what it may answer.
            Ok(pro_rata(a.budget, a.weight, a.total).min(a.max_amount))
        }
    };
    match mode {
        MODE_PRO_RATA => answer(pro(rest)?),
        MODE_ERROR => return Err(ProgramError::Custom(77)),
        MODE_LOOP => {
            let mut x = 1u64;
            loop {
                x = core::hint::black_box(x.wrapping_mul(3));
            }
        }
        MODE_SHORT => set_return_data(&[1, 2, 3, 4, 5, 6, 7]),
        MODE_LONG => set_return_data(&[1, 2, 3, 4, 5, 6, 7, 8, 9]),
        MODE_NONE => {}
        MODE_MAX => answer(u64::MAX),
        MODE_BURN => {
            let _ = burn(param);
            answer(pro(rest)?);
        }
        MODE_WRITE => {
            let mut c = config.try_borrow_mut_data()?;
            c[17] ^= 1;
            drop(c);
            answer(pro(rest)?);
        }
        MODE_FLAT => answer(param),
        MODE_RECURSE => {
            // Call itself with the same accounts and a config that answers pro rata: the config
            // is read-only, so the inner call reads the same mode; it answers when it is the inner
            // one (stack height above the caller's).
            let height = anchor_lang::solana_program::instruction::get_stack_height();
            if height > 2 {
                answer(pro(rest)?);
            } else {
                let ix = Instruction {
                    program_id: *program_id,
                    accounts: accounts
                        .iter()
                        .map(|a| AccountMeta::new_readonly(*a.key, false))
                        .collect(),
                    data: data.to_vec(),
                };
                let mut infos = accounts.to_vec();
                if let Some(me) = accounts.iter().find(|a| a.key == program_id) {
                    infos.push(me.clone());
                }
                invoke(&ix, &infos)?;
            }
        }
        MODE_CALL_COMPANION => {
            let companion =
                Pubkey::from_str_const("6ZUM1gWBH9hBBNoJoaVAGwSftyZ6CUda6vUZTW9MsJuo");
            let ix = Instruction {
                program_id: companion,
                accounts: vec![],
                data: vec![0; 8],
            };
            invoke(&ix, accounts)?;
            answer(pro(rest)?);
        }
        MODE_CHECK_PRIVILEGES => {
            if accounts.iter().any(|a| a.is_writable || a.is_signer) {
                return Err(ProgramError::Custom(99));
            }
            answer(pro(rest)?);
        }
        MODE_FAVOURITE => {
            let a = EntitleArgs::try_from_slice(rest).map_err(|_| ProgramError::InvalidArgument)?;
            let base = pro_rata(a.budget, a.weight, a.total).min(a.max_amount);
            answer(if a.owner == FAVOURITE {
                base.saturating_add(param)
            } else {
                base
            });
        }
        _ => return Err(ProgramError::InvalidInstructionData),
    }
    Ok(())
}
