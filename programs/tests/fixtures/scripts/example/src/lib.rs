//! A test-only companion script (native, never deployed; `companion_script.rs`). The companion calls
//! it with the creator address `PDA(["creator", mint], companion)` signing, which it checks; no
//! other account ever signs (the companion forwards none).
//!
//! - `companion_init` (`[creator (s, w), mint, state (w), system program, aux, aux2, ...]`): the
//!   state is the companion's `PDA(["script-state", mint])`, made by the companion (zeroed, owned by
//!   this program, its rent paid by the launcher) or the system program's id when the companion made
//!   none. It takes a state as Anchor's `#[account(zero)]` does (ours, and still all zeros) and stores
//!   the init data at its start. An init whose mode is `MODE_TAKE_LAMPORTS` or `MODE_ASSIGN` also
//!   does that misdeed (to `aux`); `MODE_TAKE_PAYER` tries to move `param` lamports out of `aux`
//!   (the launcher's wallet, if a client passed it) without its signature.
//! - `companion_step` (`[creator (s, w), mint, launch, companion, pool, state (w), aux, ...]`): does
//!   what the state's first byte (the mode) says, with the `u64` after it as its parameter: a buy or
//!   a sell on the launch's pool, nothing, or one of the misdeeds the companion must refuse. `aux` is
//!   the attacker's account (a holding or a wallet); whatever else a call needs (the swap's accounts,
//!   the token program, the system program...) is found among the accounts by key. Without a state
//!   (the system program's id in its place) a step does nothing.
//!
//! On a coin quoted in tokens (`token_quotes_builders.rs`) the `MODE_LEG_*` steps trade a leg with
//! the creator's tokens of it (`swap_spl`), pay a token account in a leg's token, or do one of the
//! misdeeds on the creator's token accounts the companion refuses.
//!
//! Every call fails (`FOREIGN_SIGNER`) when an account other than the creator signs. The suites
//! rewrite the state directly to switch modes between steps.

#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program::invoke;
use anchor_lang::solana_program::system_instruction;
use bordrless_launch::client as launch_client;
use bordrless_launch::state::{Launch, Legs};
use bordrless_token::client as token_client;

/// The script's id (test-only; its keypair is not kept).
pub const ID: Pubkey = Pubkey::from_str_const("FEj72qKFsTjmdUKB5ZKQnJzsYYKu2h5PTvXbVE9aK6S7");
/// The companion, whose creator address signs every call, and which makes the script's state.
pub const COMPANION_ID: Pubkey =
    Pubkey::from_str_const("6ZUM1gWBH9hBBNoJoaVAGwSftyZ6CUda6vUZTW9MsJuo");
/// `sha256("global:companion_init")[..8]` and `sha256("global:companion_step")[..8]`.
pub const INIT: [u8; 8] = [0x76, 0xa1, 0xbb, 0xb4, 0x57, 0x56, 0xdc, 0x88];
pub const STEP: [u8; 8] = [0x5e, 0x15, 0xaa, 0xa1, 0xf6, 0x6e, 0x15, 0x28];
/// `PDA(["script-state", mint], companion)`: the script's state the companion makes for a mint.
pub const STATE_SEED: &[u8] = b"script-state";
/// The error of a call in which an account other than the creator signs: the companion forwards
/// no other signature (the payer's, the mint's, the sender's).
pub const FOREIGN_SIGNER: u32 = 7_001;
/// The error of an init handed a state that is not the companion's for the mint, not ours, or not
/// all zeros (what Anchor's `#[account(zero)]` refuses).
pub const BAD_STATE: u32 = 7_002;

/// Buy the token with `param` lamports of the creator's bridged SOL on the launch pool.
pub const MODE_BUY: u8 = 0;
/// Send `param` bridged SOL (0: all of it) from the creator's holding to the attacker's (`aux`).
pub const MODE_TAKE_SOL: u8 = 1;
/// Send `param` tokens (0: all of them) from the creator's holding to the attacker's (`aux`).
pub const MODE_TAKE_TOKENS: u8 = 2;
/// Nothing.
pub const MODE_NOTHING: u8 = 3;
/// Send `param` lamports from the creator address to the attacker (`aux`).
pub const MODE_TAKE_LAMPORTS: u8 = 4;
/// Make the attacker (`aux`) the delegate of the creator's bridged-SOL holding.
pub const MODE_DELEGATE: u8 = 5;
/// Claim the launch's creator fees into the creator's holding.
pub const MODE_CLAIM: u8 = 6;
/// Assign the creator address to this program.
pub const MODE_ASSIGN: u8 = 7;
/// Sell `param` tokens (0: all the creator holds) on the launch pool.
pub const MODE_SELL: u8 = 8;
/// At the init: move `param` lamports from `aux` (the launcher's wallet) to the creator address,
/// which needs the launcher's signature.
pub const MODE_TAKE_PAYER: u8 = 9;
/// Buy the token with `param` of the pot's SOL on the launch pool (`swap_v2` from the creator's
/// lamports on a native launch, the bridged `swap` otherwise), then burn `param2` of the creator's
/// tokens (0: all it bought) with the token program's `burn`, the creator signing. The state's
/// byte 17 is `n`: the token's hook slice (`[hook, hook signer, extras...]`, empty for a token with
/// no hook) is `others[1..1 + n]`, right after `aux`, used for both the swap and the burn. The burn
/// write-locks the mint, so the step needs the mint writable among its accounts.
pub const MODE_BUY_BURN: u8 = 10;
/// Burn `param2` of the creator's tokens (0: all it holds), no buy; the hook slice as for
/// `MODE_BUY_BURN`.
pub const MODE_BURN: u8 = 11;

// ---- Coins quoted in tokens (`token_quotes_builders.rs`, docs/token-quotes.md §7). The state is
// the mode, the `u64` parameter, the leg's index (byte 9) and, for a trade, the length of the
// coin's token-hook slice (byte 10; the slice is `others[1..1 + n]`, right after `aux`). The leg's
// quote mint, program and decimals are read from the launch's `Legs` (found among the accounts by
// address); the creator's account of the leg's token is its associated token account. ----

/// Buy the coin on leg `i` with `param` of the leg's token from the creator's account of it
/// (`swap_spl`), the coin delivered to the creator's holding.
pub const MODE_LEG_BUY: u8 = 12;
/// Sell `param` of the creator's coins (0: all it holds) on leg `i`, the leg's token to the
/// creator's account of it.
pub const MODE_LEG_SELL: u8 = 13;
/// Pay `aux` (a token account of leg `i`'s token) `param` of it from the creator's account
/// (`TransferChecked`, the creator signing).
pub const MODE_LEG_PAY: u8 = 14;
/// Close the creator's account of leg `i`'s token, its rent to `aux` (`CloseAccount`).
pub const MODE_LEG_CLOSE: u8 = 15;
/// Hand the creator's account of leg `i`'s token to `aux` (`SetAuthority`, the account's owner).
pub const MODE_LEG_HAND_OVER: u8 = 16;
/// Make `aux` the delegate of the creator's account of leg `i`'s token (`Approve`, `param`).
pub const MODE_LEG_DELEGATE: u8 = 17;
/// Make `aux` the close authority of the creator's account of leg `i`'s token (`SetAuthority`).
pub const MODE_LEG_CLOSE_AUTHORITY: u8 = 18;
/// review5: claim leg `i`'s creator fee into the creator's account of the leg's token, inside the
/// step (the launchpad's permissionless `claim_creator_fees_token`, the creator paying nothing: its
/// account exists).
pub const MODE_LEG_CLAIM: u8 = 19;

/// SPL Token and Token-2022 (a leg's `quote_program` 0 and 1).
const SPL_TOKEN: Pubkey = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN_2022: Pubkey = Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// A leg mode's state: the mode, the parameter, the leg and the hook slice's length.
pub fn leg_state_bytes(mode: u8, param: u64, leg: u8, slice: u8) -> Vec<u8> {
    let mut d = state_bytes(mode, param);
    d.push(leg);
    d.push(slice);
    d
}

/// A burn mode's state: the mode, the buy, the burn and the hook slice's length.
pub fn burn_state_bytes(mode: u8, buy: u64, burn: u64, slice: u8) -> Vec<u8> {
    let mut d = state_bytes(mode, buy);
    d.extend_from_slice(&burn.to_le_bytes());
    d.push(slice);
    d
}

/// The script's state for `mint`, as the companion makes it.
pub fn state_address(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[STATE_SEED, mint.as_ref()], &COMPANION_ID).0
}

/// A state's bytes: the mode, then the parameter.
pub fn state_bytes(mode: u8, param: u64) -> Vec<u8> {
    let mut d = vec![mode];
    d.extend_from_slice(&param.to_le_bytes());
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
    // Only the companion can sign for the creator address of `mint`.
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

/// The `u64` after a mode byte.
fn param_of(bytes: &[u8]) -> core::result::Result<u64, ProgramError> {
    let param: [u8; 8] = bytes
        .get(1..9)
        .and_then(|b| b.try_into().ok())
        .ok_or(ProgramError::InvalidInstructionData)?;
    Ok(u64::from_le_bytes(param))
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
    let [creator, _mint, state, _system, others @ ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if *state.key != anchor_lang::system_program::ID {
        // Anchor's `#[account(zero)]`: the companion's state for the mint, ours, all zeros.
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
    // A misdeed at the init, when the init data asks for one.
    match (init_data.first().copied(), others.first()) {
        (Some(MODE_TAKE_LAMPORTS), Some(aux)) => invoke(
            &system_instruction::transfer(creator.key, aux.key, param_of(init_data)?),
            accounts,
        ),
        (Some(MODE_TAKE_PAYER), Some(payer)) => invoke(
            &system_instruction::transfer(payer.key, creator.key, param_of(init_data)?),
            accounts,
        ),
        (Some(MODE_ASSIGN), _) => invoke(&system_instruction::assign(creator.key, &ID), accounts),
        _ => Ok(()),
    }
}

/// The balance of the holding at `key` among `accounts`.
fn amount_of(accounts: &[AccountInfo], key: &Pubkey) -> core::result::Result<u64, ProgramError> {
    let info = accounts
        .iter()
        .find(|a| a.key == key)
        .ok_or(ProgramError::NotEnoughAccountKeys)?;
    token_client::read_holding(info)
        .map(|h| h.amount)
        .map_err(|_| ProgramError::InvalidAccountData)
}

fn step(accounts: &[AccountInfo], mint: &Pubkey) -> core::result::Result<(), ProgramError> {
    let [creator, _mint, launch_info, _companion, _pool, state, others @ ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    // Without a state there is nothing to do.
    if *state.key == anchor_lang::system_program::ID {
        return Ok(());
    }
    if *state.key != state_address(mint) || *state.owner != ID {
        return Err(ProgramError::Custom(BAD_STATE));
    }
    let (mode, param) = {
        let d = state.try_borrow_data()?;
        (
            *d.first().ok_or(ProgramError::InvalidAccountData)?,
            param_of(&d)?,
        )
    };
    let aux = others.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    if (MODE_LEG_BUY..=MODE_LEG_CLAIM).contains(&mode) {
        let (leg, n) = {
            let d = state.try_borrow_data()?;
            (
                *d.get(9).ok_or(ProgramError::InvalidAccountData)? as usize,
                *d.get(10).ok_or(ProgramError::InvalidAccountData)? as usize,
            )
        };
        let slice: Vec<AccountMeta> = others
            .get(1..1 + n)
            .ok_or(ProgramError::NotEnoughAccountKeys)?
            .iter()
            .map(|a| AccountMeta {
                pubkey: *a.key,
                is_signer: false,
                is_writable: a.is_writable,
            })
            .collect();
        return leg_step(
            accounts,
            launch_info,
            mint,
            *creator.key,
            *aux.key,
            (mode, param, leg),
            slice,
        );
    }
    if mode == MODE_BUY_BURN || mode == MODE_BURN {
        let d = state.try_borrow_data()?;
        let burn: [u8; 8] = d
            .get(9..17)
            .and_then(|b| b.try_into().ok())
            .ok_or(ProgramError::InvalidAccountData)?;
        let n = *d.get(17).ok_or(ProgramError::InvalidAccountData)? as usize;
        drop(d);
        let slice: Vec<AccountMeta> = others
            .get(1..1 + n)
            .ok_or(ProgramError::NotEnoughAccountKeys)?
            .iter()
            .map(|a| AccountMeta {
                pubkey: *a.key,
                is_signer: false,
                is_writable: a.is_writable,
            })
            .collect();
        return buy_burn(
            accounts,
            launch_info,
            mint,
            *creator.key,
            if mode == MODE_BUY_BURN { param } else { 0 },
            u64::from_le_bytes(burn),
            slice,
        );
    }
    let launch = {
        let d = launch_info.try_borrow_data()?;
        Launch::try_deserialize(&mut &d[..]).map_err(|_| ProgramError::InvalidAccountData)?
    };
    let keys = launch_client::LaunchKeys::of(&launch);
    let quote = launch.quote_mint;
    let me = *creator.key;
    let ix: Instruction = match mode {
        MODE_BUY => launch_client::swap(&keys, me, me, 1, param, 1),
        MODE_SELL => {
            let held = token_client::holding_address(mint, &me);
            let amount = if param == 0 {
                amount_of(accounts, &held)?
            } else {
                param
            };
            launch_client::swap(&keys, me, me, 0, amount, 1)
        }
        MODE_TAKE_SOL | MODE_TAKE_TOKENS => {
            let token = if mode == MODE_TAKE_SOL { quote } else { *mint };
            let source = token_client::holding_address(&token, &me);
            let amount = if param == 0 {
                amount_of(accounts, &source)?
            } else {
                param
            };
            token_client::transfer_with(me, source, *aux.key, token, None, vec![], amount)
        }
        MODE_NOTHING => return Ok(()),
        MODE_TAKE_LAMPORTS => system_instruction::transfer(&me, aux.key, param),
        MODE_DELEGATE => token_client::approve(
            me,
            token_client::holding_address(&quote, &me),
            *aux.key,
            u64::MAX,
        ),
        MODE_CLAIM => launch_client::claim_creator_fees(me, *mint, quote),
        MODE_ASSIGN => system_instruction::assign(&me, &ID),
        _ => return Err(ProgramError::InvalidInstructionData),
    };
    invoke(&ix, accounts)
}

/// `MODE_BUY_BURN` and `MODE_BURN`: a buy of `buy` (none at 0), then a burn of `burn` of the
/// creator's tokens (0: what it bought, or all it holds without a buy).
fn buy_burn(
    accounts: &[AccountInfo],
    launch_info: &AccountInfo,
    mint: &Pubkey,
    me: Pubkey,
    buy: u64,
    burn: u64,
    slice: Vec<AccountMeta>,
) -> core::result::Result<(), ProgramError> {
    let launch = {
        let d = launch_info.try_borrow_data()?;
        Launch::try_deserialize(&mut &d[..]).map_err(|_| ProgramError::InvalidAccountData)?
    };
    let keys = launch_client::LaunchKeys::of(&launch);
    let held = token_client::holding_address(mint, &me);
    let before = amount_of(accounts, &held)?;
    if buy > 0 {
        let ix = if launch.is_native() {
            launch_client::swap_v2_with_base_slice(&keys, me, me, 1, buy, 1, slice.clone())
        } else {
            launch_client::swap_with_base_slice(&keys, me, me, 1, buy, 1, slice.clone())
        };
        invoke(&ix, accounts)?;
    }
    let after = amount_of(accounts, &held)?;
    let amount = match (burn, buy) {
        (0, 0) => after,
        (0, _) => after.saturating_sub(before),
        (n, _) => n,
    };
    let (hook, extras) = match slice.as_slice() {
        [program, signer, extras @ ..] => (
            Some(token_client::Hook {
                program: program.pubkey,
                signer: signer.pubkey,
            }),
            extras.to_vec(),
        ),
        _ => (None, vec![]),
    };
    let ix = token_client::burn_with(me, held, *mint, hook, extras, amount);
    invoke(&ix, accounts)
}

/// A leg mode (`MODE_LEG_*`) on a coin quoted in tokens: `what` is the mode, its parameter and the
/// leg's index; `slice` the coin's token-hook slice for a trade.
fn leg_step(
    accounts: &[AccountInfo],
    launch_info: &AccountInfo,
    mint: &Pubkey,
    me: Pubkey,
    aux: Pubkey,
    what: (u8, u64, usize),
    slice: Vec<AccountMeta>,
) -> core::result::Result<(), ProgramError> {
    let (mode, param, leg) = what;
    let launch = {
        let d = launch_info.try_borrow_data()?;
        Launch::try_deserialize(&mut &d[..]).map_err(|_| ProgramError::InvalidAccountData)?
    };
    // The leg's quote mint, program and decimals, read in place from the launch's `Legs`.
    let legs_key = launch_client::legs_address(mint);
    let legs_info = accounts
        .iter()
        .find(|a| *a.key == legs_key)
        .ok_or(ProgramError::NotEnoughAccountKeys)?;
    let (quote_mint, quote_program, decimals) = {
        let d = legs_info.try_borrow_data()?;
        let at = Legs::LEGS_OFFSET + leg * Legs::LEG_LEN;
        let raw = d.get(at..at + 34).ok_or(ProgramError::InvalidAccountData)?;
        let key: [u8; 32] = raw[..32]
            .try_into()
            .map_err(|_| ProgramError::InvalidAccountData)?;
        let program = if raw[32] == 1 { TOKEN_2022 } else { SPL_TOKEN };
        (Pubkey::new_from_array(key), program, raw[33])
    };
    let keys = launch_client::LegKeys {
        mint: *mint,
        quote_mint,
        quote_program,
        lp_fee_bps: launch.lp_fee_bps,
    };
    let account = keys.quote_account(&me);
    let held = token_client::holding_address(mint, &me);
    let spl = |data: Vec<u8>, metas: Vec<AccountMeta>| Instruction {
        program_id: quote_program,
        accounts: metas,
        data,
    };
    let set_authority = |kind: u8| {
        let mut data = vec![6, kind, 1];
        data.extend_from_slice(aux.as_ref());
        spl(
            data,
            vec![
                AccountMeta::new(account, false),
                AccountMeta::new_readonly(me, true),
            ],
        )
    };
    let ix = match mode {
        MODE_LEG_BUY | MODE_LEG_SELL => {
            let (direction, amount) = if mode == MODE_LEG_BUY {
                (1, param)
            } else if param == 0 {
                (0, amount_of(accounts, &held)?)
            } else {
                (0, param)
            };
            launch_client::leg_swap_with_slice(
                &keys,
                me,
                held,
                account,
                direction,
                amount,
                1,
                slice,
                launch.rules.burns(),
            )
        }
        MODE_LEG_PAY => {
            let mut data = vec![12];
            data.extend_from_slice(&param.to_le_bytes());
            data.push(decimals);
            spl(
                data,
                vec![
                    AccountMeta::new(account, false),
                    AccountMeta::new_readonly(quote_mint, false),
                    AccountMeta::new(aux, false),
                    AccountMeta::new_readonly(me, true),
                ],
            )
        }
        MODE_LEG_CLOSE => spl(
            vec![9],
            vec![
                AccountMeta::new(account, false),
                AccountMeta::new(aux, false),
                AccountMeta::new_readonly(me, true),
            ],
        ),
        MODE_LEG_HAND_OVER => set_authority(2),
        MODE_LEG_DELEGATE => {
            let mut data = vec![4];
            data.extend_from_slice(&param.to_le_bytes());
            spl(
                data,
                vec![
                    AccountMeta::new(account, false),
                    AccountMeta::new_readonly(aux, false),
                    AccountMeta::new_readonly(me, true),
                ],
            )
        }
        MODE_LEG_CLOSE_AUTHORITY => set_authority(3),
        MODE_LEG_CLAIM => launch_client::claim_creator_fees_token(me, me, &keys, leg as u8),
        _ => return Err(ProgramError::InvalidInstructionData),
    };
    invoke(&ix, accounts)
}
