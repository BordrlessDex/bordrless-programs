//! The reference pools of the SOL switch (`docs/token-quotes.md` §7.5.2): read a quote token's pool
//! on Raydium AMM v4, Raydium CPMM, Raydium CLMM or Pump AMM against wrapped SOL or USDC (its
//! price, its time-weighted price where the venue keeps one, its fee and its depth), and build the
//! venue's own swap instruction for a sale of the token. Every read holds the account to its owner,
//! its discriminator (AMM v4: its size) and its size, and every vault to the pool's own, so a
//! venue's upgrade that moves a layout fails closed (refused, the sale waits) rather than misreads.
//!
//! Layouts, verified 21 Oct 2026 against the sources and live mainnet accounts (and a swap of each
//! simulated or replayed to the unit):
//!
//! - Raydium AMM v4 `675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8`, raydium-io/raydium-amm @
//!   91739d8d (`program/src/state.rs` L600-674 `AmmInfo`, 752 bytes packed, no discriminator;
//!   `instruction.rs` L463-470 `SwapBaseInV2` tag 16; `processor.rs` L2266-2453; `math.rs`
//!   L238-250 the reserves less `need_take_pnl`). SPL Token only.
//! - Raydium CPMM `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C`, raydium-io/raydium-cp-swap @
//!   f4b9b263 (`states/pool.rs` L64-130 `PoolState` 637 bytes, L225-246 the vaults less the
//!   protocol, fund and creator fees; `states/config.rs` L6-39 `AmmConfig` 236 bytes;
//!   `states/oracle.rs` L17-141 `ObservationState` 4,075 bytes; `instructions/swap_base_input.rs`).
//! - Raydium CLMM `CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK`, raydium-io/raydium-clmm @
//!   edfc5c0a (`states/pool.rs` L57-142 `PoolState` 1,544 bytes packed; `states/config.rs` L4-35
//!   `AmmConfig` 117 bytes; `states/oracle.rs` L8-114 `ObservationState` 4,483 bytes;
//!   `instructions/swap_v2.rs` L14-378; `libraries/tick_math.rs`).
//! - Pump AMM `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA` (`pump.rs`).

pub mod math;
pub mod pump;

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_swap::spl;

use crate::error::LaunchError;
use math::*;

/// Raydium AMM v4.
pub const RAYDIUM_AMM_V4: Pubkey =
    Pubkey::from_str_const("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
/// Raydium CPMM.
pub const RAYDIUM_CPMM: Pubkey =
    Pubkey::from_str_const("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
/// Raydium CLMM.
pub const RAYDIUM_CLMM: Pubkey =
    Pubkey::from_str_const("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");
/// Pump AMM.
pub const PUMP_AMM: Pubkey = Pubkey::from_str_const("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
/// The memo program (CLMM's `swap_v2` names it).
pub const MEMO_PROGRAM: Pubkey =
    Pubkey::from_str_const("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
/// USDC (SPL Token, 6 decimals): the other side a reference may have besides wrapped SOL.
pub const USDC_MINT: Pubkey =
    Pubkey::from_str_const("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
/// CPMM's `["vault_and_lp_mint_auth_seed"]`, the same for every pool.
pub const CPMM_AUTHORITY: Pubkey =
    Pubkey::from_str_const("GpMZbSM2GgvTKHJirzeGfMFoaZ8UR2X7F4v8vHTvxFbL");

/// `QuoteReference.venue`.
pub mod venue {
    pub const AMM_V4: u8 = 1;
    pub const CPMM: u8 = 2;
    pub const CLMM: u8 = 3;
    pub const PUMP_AMM: u8 = 4;
}

/// The venue of a pool owned by `program`.
pub fn venue_of(program: &Pubkey) -> Option<u8> {
    match *program {
        RAYDIUM_AMM_V4 => Some(venue::AMM_V4),
        RAYDIUM_CPMM => Some(venue::CPMM),
        RAYDIUM_CLMM => Some(venue::CLMM),
        PUMP_AMM => Some(venue::PUMP_AMM),
        _ => None,
    }
}

/// A reference pool's history must cover this long (seconds) when it can: the newest written
/// observation against the latest at least this long before it.
pub const TWAP_WINDOW_SECS: u64 = 1_800;
/// The shortest history a time-weighted price is taken over (a pool whose ring is younger refuses:
/// `ReferenceStale`).
pub const TWAP_MIN_WINDOW_SECS: u64 = 300;

/// What a sale of `token_mint` reads of a reference pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Market {
    pub venue: u8,
    pub pool: Pubkey,
    /// The pool's other mint: wrapped SOL or USDC.
    pub other_mint: Pubkey,
    /// Whether the token is the pool's first side (AMM v4's coin, CPMM's and CLMM's token 0, Pump
    /// AMM's base).
    pub token_is_0: bool,
    /// The other side's raw units per raw unit of the token now, Q64.64.
    pub spot: u128,
    /// The same, time-weighted (CPMM, CLMM), from written observations only.
    pub twap: Option<u128>,
    /// The pool's fee on a sale, millionths.
    pub fee_ppm: u128,
    /// The other side's depth, raw units: its reserve (AMM v4, CPMM, Pump AMM: net of the fees the
    /// vault holds), or a CLMM's virtual reserve at the current price, at most its vault.
    pub depth: u128,
    /// How many of the venue's accounts the market used (the rest are a CLMM's tick arrays).
    pub accounts: usize,
}

impl Market {
    /// The reference price of a sale: the spot raised to 97% of the time-weighted price.
    pub fn reference(&self) -> u128 {
        reference_price(self.spot, self.twap)
    }
}

fn key_at(data: &[u8], at: usize) -> Pubkey {
    Pubkey::new_from_array(data[at..at + 32].try_into().expect("32"))
}

fn u64_at(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().expect("8"))
}

fn u128_at(data: &[u8], at: usize) -> u128 {
    u128::from_le_bytes(data[at..at + 16].try_into().expect("16"))
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().expect("4"))
}

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(data[at..at + 2].try_into().expect("2"))
}

/// A token account the pool holds: its key the pool's `expected`, of `mint`, under SPL Token or
/// Token-2022 (`only_spl`: SPL Token alone); its balance.
fn vault_balance(
    info: &AccountInfo,
    expected: &Pubkey,
    mint: &Pubkey,
    only_spl: bool,
) -> Result<u64> {
    require_keys_eq!(*info.key, *expected, LaunchError::WrongVenueAccounts);
    let program = *info.owner;
    let ok_program =
        program == spl::TOKEN_PROGRAM_ID || (!only_spl && program == spl::TOKEN_2022_PROGRAM_ID);
    require!(ok_program, LaunchError::ReferencePoolInvalid);
    let t = spl::read_token_account(info, &program).ok_or(LaunchError::ReferencePoolInvalid)?;
    require_keys_eq!(t.mint, *mint, LaunchError::ReferencePoolInvalid);
    Ok(t.amount)
}

/// The two mints of a pool, the token's side and the other: the token one of them, the other
/// wrapped SOL or USDC (`ReferencePoolInvalid` otherwise). Answers whether the token is the first.
fn sides(first: &Pubkey, second: &Pubkey, token: &Pubkey) -> Result<(bool, Pubkey)> {
    let other_ok = |m: &Pubkey| *m == spl::NATIVE_MINT || *m == USDC_MINT;
    if first == token && other_ok(second) {
        Ok((true, *second))
    } else if second == token && other_ok(first) {
        Ok((false, *first))
    } else {
        err!(LaunchError::ReferencePoolInvalid)
    }
}

/// The pool named by a venue's accounts (by the venue's layout: AMM v4's and Pump AMM's second,
/// CLMM's third, CPMM's fourth), and its venue; `None` for no venue's program.
pub fn pool_of(accounts: &[AccountInfo]) -> Option<(u8, Pubkey)> {
    let venue = venue_of(accounts.first()?.key)?;
    let at = match venue {
        venue::AMM_V4 | venue::PUMP_AMM => 1,
        venue::CLMM => 2,
        _ => 3,
    };
    Some((venue, *accounts.get(at)?.key))
}

/// Reads the reference pool whose venue accounts are `accounts` (the venue's program first, as its
/// swap takes them, [`swap_ix`]) for a sale of the token `mint`, at `now`.
pub fn read_market(accounts: &[AccountInfo], mint: &AccountInfo, now: i64) -> Result<Market> {
    let program = accounts.first().ok_or(LaunchError::WrongVenueAccounts)?;
    let token = mint.key;
    match venue_of(program.key) {
        Some(venue::AMM_V4) => read_amm_v4(accounts, token, now),
        Some(venue::CPMM) => read_cpmm(accounts, token, now),
        Some(venue::CLMM) => read_clmm(accounts, token, now),
        Some(venue::PUMP_AMM) => pump::read(accounts, mint, token),
        _ => err!(LaunchError::ReferencePoolInvalid),
    }
}

// ---- Raydium AMM v4: [program, pool (w), authority, coin vault (w), pc vault (w)] -----------------

/// `AmmInfo`'s size.
pub const AMM_V4_LEN: usize = 752;

fn read_amm_v4(accounts: &[AccountInfo], token: &Pubkey, now: i64) -> Result<Market> {
    require!(accounts.len() >= 5, LaunchError::WrongVenueAccounts);
    let pool = &accounts[1];
    require_keys_eq!(
        *pool.owner,
        RAYDIUM_AMM_V4,
        LaunchError::ReferencePoolInvalid
    );
    let data = pool.try_borrow_data()?;
    require!(data.len() == AMM_V4_LEN, LaunchError::ReferencePoolInvalid);
    // Status 1 Initialized, 6 SwapOnly, 7 WaitingTrade once open (`swap_permission`).
    let status = u64_at(&data, 0);
    let open = match status {
        1 | 6 => true,
        7 => now >= 0 && u64::try_from(now).unwrap_or(0) >= u64_at(&data, 224),
        _ => false,
    };
    require!(open, LaunchError::ReferencePoolInvalid);
    let nonce = u64_at(&data, 8);
    let authority = Pubkey::create_program_address(
        &[
            b"amm authority",
            &[u8::try_from(nonce).map_err(|_| LaunchError::ReferencePoolInvalid)?],
        ],
        &RAYDIUM_AMM_V4,
    )
    .map_err(|_| LaunchError::ReferencePoolInvalid)?;
    require_keys_eq!(*accounts[2].key, authority, LaunchError::WrongVenueAccounts);
    let (fee_num, fee_den) = (u64_at(&data, 176), u64_at(&data, 184));
    require!(fee_den > 0, LaunchError::ReferencePoolInvalid);
    let (coin_mint, pc_mint) = (key_at(&data, 400), key_at(&data, 432));
    let (token_is_0, other_mint) = sides(&coin_mint, &pc_mint, token)?;
    let coin = vault_balance(&accounts[3], &key_at(&data, 336), &coin_mint, true)?
        .checked_sub(u64_at(&data, 192))
        .ok_or(LaunchError::ReferencePoolInvalid)?;
    let pc = vault_balance(&accounts[4], &key_at(&data, 368), &pc_mint, true)?
        .checked_sub(u64_at(&data, 200))
        .ok_or(LaunchError::ReferencePoolInvalid)?;
    let (token_side, other_side) = if token_is_0 { (coin, pc) } else { (pc, coin) };
    Ok(Market {
        venue: venue::AMM_V4,
        pool: *pool.key,
        other_mint,
        token_is_0,
        spot: ratio_q64(other_side, token_side).ok_or(LaunchError::ReferencePoolInvalid)?,
        twap: None,
        fee_ppm: u128::from(fee_num) * PPM / u128::from(fee_den),
        depth: u128::from(other_side),
        accounts: 5,
    })
}

// ---- Raydium CPMM: [program, authority, amm config, pool (w), token-0 vault (w), token-1 vault (w),
// observation (w)] -----------------------------------------------------------------------------------

/// `sha256("account:PoolState")[..8]` (CPMM's and CLMM's: the owner tells them apart).
pub const POOL_STATE_DISCRIMINATOR: [u8; 8] = [247, 237, 227, 245, 215, 195, 222, 70];
/// `sha256("account:AmmConfig")[..8]`.
pub const AMM_CONFIG_DISCRIMINATOR: [u8; 8] = [218, 244, 33, 104, 203, 203, 43, 111];
/// `sha256("account:ObservationState")[..8]`.
pub const OBSERVATION_DISCRIMINATOR: [u8; 8] = [122, 174, 197, 53, 129, 9, 165, 132];
/// CPMM `PoolState`, `AmmConfig` and `ObservationState` sizes.
pub const CPMM_POOL_LEN: usize = 637;
pub const CPMM_CONFIG_LEN: usize = 236;
pub const CPMM_OBSERVATION_LEN: usize = 4_075;

fn anchor_account<'a>(
    info: &'a AccountInfo,
    owner: &Pubkey,
    discriminator: &[u8; 8],
    len: usize,
) -> Result<std::cell::Ref<'a, &'a mut [u8]>> {
    require_keys_eq!(*info.owner, *owner, LaunchError::ReferencePoolInvalid);
    let data = info.try_borrow_data()?;
    require!(
        data.len() == len && data[..8] == *discriminator,
        LaunchError::ReferencePoolInvalid
    );
    Ok(data)
}

fn read_cpmm(accounts: &[AccountInfo], token: &Pubkey, now: i64) -> Result<Market> {
    require!(accounts.len() >= 7, LaunchError::WrongVenueAccounts);
    require_keys_eq!(
        *accounts[1].key,
        CPMM_AUTHORITY,
        LaunchError::WrongVenueAccounts
    );
    let pool = &accounts[3];
    let data = anchor_account(
        pool,
        &RAYDIUM_CPMM,
        &POOL_STATE_DISCRIMINATOR,
        CPMM_POOL_LEN,
    )?;
    // Status bit 2 disables swaps; a swap waits for `open_time`.
    require!(
        data[329] & 0b100 == 0 && u64::try_from(now).unwrap_or(0) >= u64_at(&data, 373),
        LaunchError::ReferencePoolInvalid
    );
    require_keys_eq!(
        *accounts[2].key,
        key_at(&data, 8),
        LaunchError::WrongVenueAccounts
    );
    require_keys_eq!(
        *accounts[6].key,
        key_at(&data, 296),
        LaunchError::WrongVenueAccounts
    );
    let (mint0, mint1) = (key_at(&data, 168), key_at(&data, 200));
    let (token_is_0, other_mint) = sides(&mint0, &mint1, token)?;
    let enable_creator_fee = data[390] != 0;
    let fees0 = u64_at(&data, 341)
        .saturating_add(u64_at(&data, 357))
        .saturating_add(u64_at(&data, 397));
    let fees1 = u64_at(&data, 349)
        .saturating_add(u64_at(&data, 365))
        .saturating_add(u64_at(&data, 405));
    let net0 = vault_balance(&accounts[4], &key_at(&data, 72), &mint0, false)?
        .checked_sub(fees0)
        .ok_or(LaunchError::ReferencePoolInvalid)?;
    let net1 = vault_balance(&accounts[5], &key_at(&data, 104), &mint1, false)?
        .checked_sub(fees1)
        .ok_or(LaunchError::ReferencePoolInvalid)?;
    drop(data);
    let config = anchor_account(
        &accounts[2],
        &RAYDIUM_CPMM,
        &AMM_CONFIG_DISCRIMINATOR,
        CPMM_CONFIG_LEN,
    )?;
    let mut fee_ppm = u128::from(u64_at(&config, 12));
    if enable_creator_fee {
        fee_ppm += u128::from(u64_at(&config, 108));
    }
    drop(config);
    let (token_side, other_side) = if token_is_0 {
        (net0, net1)
    } else {
        (net1, net0)
    };
    let twap = cpmm_twap(&accounts[6], pool.key, token_is_0, now)?;
    Ok(Market {
        venue: venue::CPMM,
        pool: *pool.key,
        other_mint,
        token_is_0,
        spot: ratio_q64(other_side, token_side).ok_or(LaunchError::ReferencePoolInvalid)?,
        twap: Some(twap),
        fee_ppm,
        depth: u128::from(other_side),
        accounts: 7,
    })
}

/// A CPMM observation ring's time-weighted price of the token (token 0 in token 1 when
/// `token_is_0`, else the reverse), Q64.64: from the newest observation (its cumulative is as of
/// the ring's `last_update_timestamp`) back to the latest observation at least
/// [`TWAP_WINDOW_SECS`] before, or the oldest when the ring is younger (at least
/// [`TWAP_MIN_WINDOW_SECS`], else `ReferenceStale`). The cumulatives are prices before each swap
/// (`update` runs before the swap moves the pool), so a sale's own transaction can't move them. A
/// price too coarse in Q32 for the token's side (below 2^24) is taken from the other side's
/// cumulative and inverted (its harmonic mean: lower, so the floor it sets only loosens).
pub fn cpmm_twap(info: &AccountInfo, pool: &Pubkey, token_is_0: bool, now: i64) -> Result<u128> {
    let data = anchor_account(
        info,
        &RAYDIUM_CPMM,
        &OBSERVATION_DISCRIMINATOR,
        CPMM_OBSERVATION_LEN,
    )?;
    require!(data[8] != 0, LaunchError::ReferenceStale);
    require_keys_eq!(key_at(&data, 11), *pool, LaunchError::WrongVenueAccounts);
    let idx = usize::from(u16_at(&data, 9));
    require!(idx < 100, LaunchError::ReferenceStale);
    let obs = |i: usize| {
        let at = 43 + 40 * i;
        (
            u64_at(&data, at),
            u128_at(&data, at + 8),
            u128_at(&data, at + 24),
        )
    };
    let end_ts = u64_at(&data, 4043);
    require!(
        end_ts > 0 && i64::try_from(end_ts).unwrap_or(i64::MAX) <= now,
        LaunchError::ReferenceStale
    );
    let (_, end0, end1) = obs(idx);
    let (start_ts, start0, start1) = ring_start(idx, 100, end_ts, |i| obs(i).0, obs)?;
    let dt = u128::from(end_ts - start_ts);
    let (mine, theirs) = if token_is_0 {
        (end0.wrapping_sub(start0), end1.wrapping_sub(start1))
    } else {
        (end1.wrapping_sub(start1), end0.wrapping_sub(start0))
    };
    let mean_x32 = mine / dt;
    if mean_x32 >= 1 << 24 {
        return Ok(mean_x32 << 32);
    }
    let other_x32 = theirs / dt;
    require!(other_x32 > 0, LaunchError::ReferenceStale);
    Ok((1u128 << 96) / other_x32)
}

/// The start of a ring's window ending at `end_ts`: walking back from the newest (`idx`, of `n`),
/// the first observation at least [`TWAP_WINDOW_SECS`] older, else the oldest written one when it
/// is at least [`TWAP_MIN_WINDOW_SECS`] older (`ReferenceStale` otherwise). A slot never written
/// (timestamp 0) or a later timestamp ends the walk (the ring wrapped).
fn ring_start<T>(
    idx: usize,
    n: usize,
    end_ts: u64,
    ts: impl Fn(usize) -> u64,
    at: impl Fn(usize) -> T,
) -> Result<T> {
    let mut best: Option<usize> = None;
    let mut prev = ts(idx);
    for step in 1..n {
        let i = (idx + n - step) % n;
        let t = ts(i);
        if t == 0 || t > prev {
            break;
        }
        best = Some(i);
        prev = t;
        if end_ts.saturating_sub(t) >= TWAP_WINDOW_SECS {
            break;
        }
    }
    let i = best.ok_or(LaunchError::ReferenceStale)?;
    require!(
        end_ts.saturating_sub(ts(i)) >= TWAP_MIN_WINDOW_SECS,
        LaunchError::ReferenceStale
    );
    Ok(at(i))
}

// ---- Raydium CLMM: [program, amm config, pool (w), token-0 vault (w), token-1 vault (w), observation
// (w), Token-2022, memo, then the bitmap extension and tick arrays (w)] -------------------------------

/// CLMM `PoolState`, `AmmConfig` and `ObservationState` sizes.
pub const CLMM_POOL_LEN: usize = 1_544;
pub const CLMM_CONFIG_LEN: usize = 117;
pub const CLMM_OBSERVATION_LEN: usize = 4_483;
/// A CLMM tick array's and bitmap extension's sizes (the swap takes them after its named accounts).
pub const CLMM_TICK_ARRAY_LEN: usize = 10_240;
pub const CLMM_BITMAP_EXTENSION_LEN: usize = 1_832;

fn read_clmm(accounts: &[AccountInfo], token: &Pubkey, _now: i64) -> Result<Market> {
    require!(accounts.len() >= 8, LaunchError::WrongVenueAccounts);
    require_keys_eq!(
        *accounts[6].key,
        spl::TOKEN_2022_PROGRAM_ID,
        LaunchError::WrongVenueAccounts
    );
    require_keys_eq!(
        *accounts[7].key,
        MEMO_PROGRAM,
        LaunchError::WrongVenueAccounts
    );
    let pool = &accounts[2];
    let data = anchor_account(
        pool,
        &RAYDIUM_CLMM,
        &POOL_STATE_DISCRIMINATOR,
        CLMM_POOL_LEN,
    )?;
    // Status bit 4 disables swaps.
    require!(data[389] & 0b1_0000 == 0, LaunchError::ReferencePoolInvalid);
    require_keys_eq!(
        *accounts[1].key,
        key_at(&data, 9),
        LaunchError::WrongVenueAccounts
    );
    require_keys_eq!(
        *accounts[5].key,
        key_at(&data, 201),
        LaunchError::WrongVenueAccounts
    );
    let (mint0, mint1) = (key_at(&data, 73), key_at(&data, 105));
    let (token_is_0, other_mint) = sides(&mint0, &mint1, token)?;
    let liquidity = u128_at(&data, 237);
    let sqrt_price = u128_at(&data, 253);
    let vault0 = vault_balance(&accounts[3], &key_at(&data, 137), &mint0, false)?;
    let vault1 = vault_balance(&accounts[4], &key_at(&data, 169), &mint1, false)?;
    drop(data);
    let config = anchor_account(
        &accounts[1],
        &RAYDIUM_CLMM,
        &AMM_CONFIG_DISCRIMINATOR,
        CLMM_CONFIG_LEN,
    )?;
    let fee_ppm = u128::from(u32_at(&config, 47));
    drop(config);
    // The other side's virtual reserve at the current price (token 1: L·√P; token 0: L/√P), at
    // most what its vault holds.
    let (virtual_other, vault_other) = if token_is_0 {
        (mul_div(liquidity, sqrt_price, Q64), vault1)
    } else {
        (mul_div(liquidity, Q64, sqrt_price.max(1)), vault0)
    };
    let depth = virtual_other
        .unwrap_or(u128::MAX)
        .min(u128::from(vault_other));
    let spot = price_from_sqrt(sqrt_price, token_is_0).ok_or(LaunchError::ReferencePoolInvalid)?;
    let twap = clmm_twap(&accounts[5], pool.key, token_is_0)?;
    // The bitmap extension and tick arrays the swap takes: CLMM-owned, of their sizes, writable.
    let mut used = 8;
    while let Some(a) = accounts.get(used) {
        let len = a.data_len();
        if *a.owner != RAYDIUM_CLMM
            || !(len == CLMM_TICK_ARRAY_LEN || len == CLMM_BITMAP_EXTENSION_LEN)
        {
            break;
        }
        used += 1;
    }
    Ok(Market {
        venue: venue::CLMM,
        pool: *pool.key,
        other_mint,
        token_is_0,
        spot,
        twap: Some(twap),
        fee_ppm,
        depth,
        accounts: used,
    })
}

/// A CLMM observation ring's time-weighted price of the token, Q64.64: the mean tick from the
/// newest written observation back [`TWAP_WINDOW_SECS`] (or the oldest, at least
/// [`TWAP_MIN_WINDOW_SECS`] back), never extrapolated with the live tick. Raydium writes an
/// observation at most every 15 s, on a swap that moves the tick, crediting the tick before the
/// swap for the whole gap: a quiet pool's newest observation may be old, its price unchanged since.
pub fn clmm_twap(info: &AccountInfo, pool: &Pubkey, token_is_0: bool) -> Result<u128> {
    let data = anchor_account(
        info,
        &RAYDIUM_CLMM,
        &OBSERVATION_DISCRIMINATOR,
        CLMM_OBSERVATION_LEN,
    )?;
    require!(data[8] != 0, LaunchError::ReferenceStale);
    require_keys_eq!(key_at(&data, 19), *pool, LaunchError::WrongVenueAccounts);
    let idx = usize::from(u16_at(&data, 17));
    require!(idx < 100, LaunchError::ReferenceStale);
    let obs = |i: usize| {
        let at = 51 + 44 * i;
        (
            u64::from(u32_at(&data, at)),
            i64::from_le_bytes(data[at + 4..at + 12].try_into().expect("8")),
        )
    };
    let (end_ts, end_cum) = obs(idx);
    require!(end_ts > 0, LaunchError::ReferenceStale);
    let (start_ts, start_cum) = ring_start(idx, 100, end_ts, |i| obs(i).0, obs)?;
    let dt = i64::try_from(end_ts - start_ts).map_err(|_| LaunchError::ReferenceStale)?;
    let mean = end_cum.wrapping_sub(start_cum).div_euclid(dt);
    let tick = i32::try_from(mean).map_err(|_| LaunchError::ReferenceStale)?;
    let sqrt = sqrt_price_at_tick(tick).ok_or(LaunchError::ReferenceStale)?;
    price_from_sqrt(sqrt, token_is_0).ok_or_else(|| error!(LaunchError::ReferenceStale))
}

// ---- The swap -----------------------------------------------------------------------------------------

/// What a sale names besides the venue's accounts: the seller (the tokens' owner, signing), its
/// account of the token sold (`source`) and of the other side (`destination`), the token's mint and
/// program, the other side's mint (wrapped SOL or USDC, both SPL Token) and SPL Token.
pub struct SaleKeys {
    pub owner: Pubkey,
    pub source: Pubkey,
    pub destination: Pubkey,
    pub token_mint: Pubkey,
    pub token_program: Pubkey,
    pub other_mint: Pubkey,
}

/// The venue's own exact-in swap of `amount_in` of the token for at least `min_out` of the other
/// side, for the market `m` read from `accounts` (the venue's accounts, its program first).
pub fn swap_ix(
    m: &Market,
    accounts: &[AccountInfo],
    keys: &SaleKeys,
    amount_in: u64,
    min_out: u64,
) -> Result<Instruction> {
    let ro = |k: Pubkey| AccountMeta::new_readonly(k, false);
    let rw = |k: Pubkey| AccountMeta::new(k, false);
    let k = |i: usize| *accounts[i].key;
    match m.venue {
        venue::AMM_V4 => {
            let mut data = Vec::with_capacity(17);
            data.push(16);
            data.extend_from_slice(&amount_in.to_le_bytes());
            data.extend_from_slice(&min_out.to_le_bytes());
            Ok(Instruction {
                program_id: RAYDIUM_AMM_V4,
                accounts: vec![
                    ro(spl::TOKEN_PROGRAM_ID),
                    rw(k(1)),
                    ro(k(2)),
                    rw(k(3)),
                    rw(k(4)),
                    rw(keys.source),
                    rw(keys.destination),
                    AccountMeta::new_readonly(keys.owner, true),
                ],
                data,
            })
        }
        venue::CPMM => {
            let (input_vault, output_vault) = if m.token_is_0 {
                (k(4), k(5))
            } else {
                (k(5), k(4))
            };
            let mut data = vec![143, 190, 90, 218, 196, 30, 51, 222];
            data.extend_from_slice(&amount_in.to_le_bytes());
            data.extend_from_slice(&min_out.to_le_bytes());
            Ok(Instruction {
                program_id: RAYDIUM_CPMM,
                accounts: vec![
                    AccountMeta::new_readonly(keys.owner, true),
                    ro(k(1)),
                    ro(k(2)),
                    rw(k(3)),
                    rw(keys.source),
                    rw(keys.destination),
                    rw(input_vault),
                    rw(output_vault),
                    ro(keys.token_program),
                    ro(spl::TOKEN_PROGRAM_ID),
                    ro(keys.token_mint),
                    ro(keys.other_mint),
                    rw(k(6)),
                ],
                data,
            })
        }
        venue::CLMM => {
            let (input_vault, output_vault, input_mint, output_mint) = if m.token_is_0 {
                (k(3), k(4), keys.token_mint, keys.other_mint)
            } else {
                (k(4), k(3), keys.token_mint, keys.other_mint)
            };
            let mut data = vec![43, 4, 237, 11, 26, 201, 30, 98];
            data.extend_from_slice(&amount_in.to_le_bytes());
            data.extend_from_slice(&min_out.to_le_bytes());
            data.extend_from_slice(&0u128.to_le_bytes());
            data.push(1);
            let mut metas = vec![
                AccountMeta::new_readonly(keys.owner, true),
                ro(k(1)),
                rw(k(2)),
                rw(keys.source),
                rw(keys.destination),
                rw(input_vault),
                rw(output_vault),
                rw(k(5)),
                ro(spl::TOKEN_PROGRAM_ID),
                ro(spl::TOKEN_2022_PROGRAM_ID),
                ro(MEMO_PROGRAM),
                ro(input_mint),
                ro(output_mint),
            ];
            metas.extend(accounts[8..m.accounts].iter().map(|a| rw(*a.key)));
            Ok(Instruction {
                program_id: RAYDIUM_CLMM,
                accounts: metas,
                data,
            })
        }
        venue::PUMP_AMM => pump::sell_ix(m, accounts, keys, amount_in, min_out),
        _ => err!(LaunchError::ReferencePoolInvalid),
    }
}
