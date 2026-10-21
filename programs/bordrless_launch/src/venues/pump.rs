//! Pump AMM (`pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`) as a reference venue
//! (`docs/token-quotes.md` §7.5.2), from pump-fun/pump-public-docs @ 639b87fb (`idl/pump_amm.json`,
//! `idl/pump_fees.json`, `docs/FEE_PROGRAM_README.md`, `docs/VIRTUAL_QUOTE_RESERVES_FEE_ADJUSTMENT.md`)
//! and the SDK sources they point to (`@pump-fun/pump-swap-sdk` 2.1.0 `src/sdk/fees.ts`,
//! `pump-rust-client` 0.4.1 `src/math/fees.rs`), each checked against live pools and a real sell to
//! the unit on 21 Oct 2026.
//!
//! A pool's base is the token sold, its quote wrapped SOL or USDC. Its price is the quote side's
//! effective reserve (the vault plus the pool's signed `virtual_quote_reserves`) over the base
//! vault; its depth the least of that and the real reserve (the vault less the fee buckets it
//! holds). Its fee is the fee program's (`FeeConfig`): the market-cap tier of a canonical pool's
//! schedule (wrapped SOL: `fee_tiers`, USDC: `stable_fee_tiers`, another quote: `exotic_flat_fees`),
//! `flat_fees` for any other pool, a pool's own creator fee when the global config lets pools set
//! one, no creator fee without a coin creator. Pump AMM keeps no price history: its reference is
//! its spot under the cap, as Raydium AMM v4's. Refused: a cashback coin (its sale would make the
//! seller pay the rent of a volume accumulator; a PDA seller can't) and sells disabled.
//!
//! The sale is v1 `sell` (it never charges the seller lamports on a pool without cashback, and takes
//! a PDA seller: mainnet tx CPbNwMAz… does it): its 21 accounts, then `pool_v2` when the sale pays a
//! creator fee, the buyback wallet and its quote account.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_swap::spl;

use super::math::{mul_div, PPM};
use super::{key_at, u64_at, vault_balance, Market, SaleKeys, USDC_MINT};
use crate::error::LaunchError;

/// Pump AMM.
pub const PROGRAM: Pubkey = super::PUMP_AMM;
/// The pump.fun bonding-curve program: a canonical (graduated) pool's creator is its
/// `["pool-authority", base_mint]`.
pub const PUMP_BONDING: Pubkey =
    Pubkey::from_str_const("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
/// Pump's fee program.
pub const FEE_PROGRAM: Pubkey =
    Pubkey::from_str_const("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
/// `["global_config"]` under Pump AMM.
pub const GLOBAL_CONFIG: Pubkey =
    Pubkey::from_str_const("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw");
/// `["fee_config", PUMP_AMM]` under the fee program.
pub const FEE_CONFIG: Pubkey =
    Pubkey::from_str_const("5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx");
/// Pump AMM's `["__event_authority"]`.
pub const EVENT_AUTHORITY: Pubkey =
    Pubkey::from_str_const("GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR");
/// Discriminators: `account:Pool`, `account:GlobalConfig`, `account:FeeConfig`, `global:sell`.
pub const POOL_DISCRIMINATOR: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];
pub const GLOBAL_CONFIG_DISCRIMINATOR: [u8; 8] = [0x95, 0x08, 0x9c, 0xca, 0xa0, 0xfc, 0xb0, 0xd9];
pub const FEE_CONFIG_DISCRIMINATOR: [u8; 8] = [0x8f, 0x34, 0x92, 0xbb, 0xdb, 0x7b, 0x4c, 0x9b];
pub const SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
/// The supply a mayhem pool's market cap counts.
pub const MAYHEM_SUPPLY: u128 = 1_000_000_000_000_000;

/// The fee rates of a sale, basis points of the quote it brings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fees {
    pub lp: u64,
    pub protocol: u64,
    pub creator: u64,
}

fn fees_at(data: &[u8], at: usize) -> Option<Fees> {
    Some(Fees {
        lp: u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?),
        protocol: u64::from_le_bytes(data.get(at + 8..at + 16)?.try_into().ok()?),
        creator: u64::from_le_bytes(data.get(at + 16..at + 24)?.try_into().ok()?),
    })
}

/// A `Vec<FeeTier>` at `at` (a u32 length, then 40 bytes each: a u128 market-cap threshold in the
/// quote's raw units, then the three fees): the tier of `mcap` (below the first threshold the first,
/// else the last whose threshold `mcap` reaches), and where the vector ends.
fn tier_of(data: &[u8], at: usize, mcap: u128) -> Option<(Option<Fees>, usize)> {
    let n = usize::try_from(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?)).ok()?;
    let start = at + 4;
    let end = start.checked_add(n.checked_mul(40)?)?;
    if end > data.len() {
        return None;
    }
    let threshold = |i: usize| {
        u128::from_le_bytes(
            data[start + 40 * i..start + 40 * i + 16]
                .try_into()
                .expect("16"),
        )
    };
    let fees = |i: usize| fees_at(data, start + 40 * i + 16);
    if n == 0 {
        return Some((None, end));
    }
    let tier = if mcap < threshold(0) {
        fees(0)
    } else {
        (0..n).rev().find(|i| mcap >= threshold(*i)).and_then(fees)
    };
    Some((tier, end))
}

/// The fees `FeeConfig` (`data`) sets a pool of `quote` (`canonical`: a graduated coin's) at market
/// cap `mcap`.
pub fn schedule_fees(data: &[u8], canonical: bool, quote: &Pubkey, mcap: u128) -> Option<Fees> {
    let flat = fees_at(data, 41)?;
    let (tiers, stable_at) = tier_of(data, 65, mcap)?;
    let (stable, exotic_at) = tier_of(data, stable_at, mcap)?;
    let exotic = fees_at(data, exotic_at).unwrap_or_default();
    if !canonical {
        return Some(flat);
    }
    if *quote == spl::NATIVE_MINT || *quote == spl::NATIVE_MINT_2022 || *quote == Pubkey::default()
    {
        tiers
    } else if *quote == USDC_MINT {
        stable
    } else if exotic != Fees::default() {
        Some(exotic)
    } else {
        Some(flat)
    }
}

/// The venue's accounts: [program, pool (w), global config, base vault (w), quote vault (w),
/// protocol fee recipient, its quote account (w), event authority, coin creator's vault account (w),
/// coin creator's vault authority, fee config, fee program, (`pool_v2` when the sale pays a creator
/// fee), buyback wallet, its quote account (w)]. `mint` is the token's mint (its supply prices the
/// market cap).
pub fn read(accounts: &[AccountInfo], mint: &AccountInfo, token: &Pubkey) -> Result<Market> {
    require!(accounts.len() >= 14, LaunchError::WrongVenueAccounts);
    require_keys_eq!(
        *accounts[2].key,
        GLOBAL_CONFIG,
        LaunchError::WrongVenueAccounts
    );
    require_keys_eq!(
        *accounts[7].key,
        EVENT_AUTHORITY,
        LaunchError::WrongVenueAccounts
    );
    require_keys_eq!(
        *accounts[10].key,
        FEE_CONFIG,
        LaunchError::WrongVenueAccounts
    );
    require_keys_eq!(
        *accounts[11].key,
        FEE_PROGRAM,
        LaunchError::WrongVenueAccounts
    );
    require_keys_eq!(*mint.key, *token, LaunchError::WrongVenueAccounts);
    let pool = &accounts[1];
    require_keys_eq!(*pool.owner, PROGRAM, LaunchError::ReferencePoolInvalid);
    let data = pool.try_borrow_data()?;
    require!(
        data.len() >= 243 && data[..8] == POOL_DISCRIMINATOR,
        LaunchError::ReferencePoolInvalid
    );
    let byte = |at: usize| data.get(at).copied().unwrap_or(0);
    let tail_u64 = |at: usize| {
        data.get(at..at + 8)
            .map_or(0, |b| u64::from_le_bytes(b.try_into().expect("8")))
    };
    let (base_mint, quote_mint) = (key_at(&data, 43), key_at(&data, 75));
    require_keys_eq!(base_mint, *token, LaunchError::ReferencePoolInvalid);
    require!(
        quote_mint == spl::NATIVE_MINT || quote_mint == USDC_MINT,
        LaunchError::ReferencePoolInvalid
    );
    let (mayhem, cashback) = (byte(243) != 0, byte(244) != 0);
    require!(!cashback, LaunchError::ReferencePoolInvalid);
    let virtual_quote = data
        .get(245..261)
        .map_or(0, |b| i128::from_le_bytes(b.try_into().expect("16")));
    let pool_creator_fee = tail_u64(261);
    let buckets = tail_u64(271).saturating_add(tail_u64(279));
    let creator = key_at(&data, 11);
    let coin_creator = key_at(&data, 211);
    let base = vault_balance(&accounts[3], &key_at(&data, 139), &base_mint, false)?;
    let quote = vault_balance(&accounts[4], &key_at(&data, 171), &quote_mint, true)?;
    drop(data);
    let effective = i128::from(quote)
        .checked_add(virtual_quote)
        .filter(|q| *q > 0)
        .ok_or(LaunchError::ReferencePoolInvalid)? as u128;
    let real = u128::from(quote.saturating_sub(buckets));
    require!(base > 0, LaunchError::ReferencePoolInvalid);
    // The global config: sells on (bit 4 of `disable_flags`), a pool's own creator fee allowed?
    let config = &accounts[2];
    require_keys_eq!(*config.owner, PROGRAM, LaunchError::ReferencePoolInvalid);
    let configurable = {
        let g = config.try_borrow_data()?;
        require!(
            g.len() >= 57 && g[..8] == GLOBAL_CONFIG_DISCRIMINATOR,
            LaunchError::ReferencePoolInvalid
        );
        require!(g[56] & 0b1_0000 == 0, LaunchError::ReferencePoolInvalid);
        g.get(940).copied().unwrap_or(0) != 0
    };
    // The market cap the fee program prices tiers by: the effective quote over the base vault,
    // times the supply (a mayhem pool's fixed).
    let supply = if mayhem {
        MAYHEM_SUPPLY
    } else {
        let m = mint.try_borrow_data()?;
        require!(m.len() >= 44, LaunchError::ReferencePoolInvalid);
        u128::from(u64_at(&m, 36))
    };
    let mcap =
        mul_div(effective, supply, u128::from(base)).ok_or(LaunchError::ReferencePoolInvalid)?;
    let canonical = creator
        == Pubkey::find_program_address(&[b"pool-authority", base_mint.as_ref()], &PUMP_BONDING).0;
    let fee_config = &accounts[10];
    require_keys_eq!(
        *fee_config.owner,
        FEE_PROGRAM,
        LaunchError::ReferencePoolInvalid
    );
    let mut fees = {
        let f = fee_config.try_borrow_data()?;
        require!(
            f.len() >= 65 && f[..8] == FEE_CONFIG_DISCRIMINATOR,
            LaunchError::ReferencePoolInvalid
        );
        schedule_fees(&f, canonical, &quote_mint, mcap).ok_or(LaunchError::ReferencePoolInvalid)?
    };
    if configurable && pool_creator_fee != 0 {
        fees.creator = pool_creator_fee;
    }
    if coin_creator == Pubkey::default() {
        fees.creator = 0;
    }
    let fee_bps = fees
        .lp
        .saturating_add(fees.protocol)
        .saturating_add(fees.creator);
    // The accounts the sale takes after the fee program: `pool_v2` when it pays a creator fee.
    let pool_v2 = Pubkey::find_program_address(&[b"pool-v2", base_mint.as_ref()], &PROGRAM).0;
    let used = if *accounts[12].key == pool_v2 { 15 } else { 14 };
    require!(
        accounts.len() >= used && (fees.creator == 0 || used == 15),
        LaunchError::WrongVenueAccounts
    );
    Ok(Market {
        venue: super::venue::PUMP_AMM,
        pool: *pool.key,
        other_mint: quote_mint,
        token_is_0: true,
        spot: (effective
            .checked_shl(64)
            .ok_or(LaunchError::ReferencePoolInvalid)?)
            / u128::from(base),
        twap: None,
        fee_ppm: u128::from(fee_bps) * PPM / 10_000,
        depth: effective.min(real),
        accounts: used,
    })
}

/// Pump AMM's v1 `sell(base_amount_in, min_quote_amount_out)` of the token for the quote, the seller
/// `keys.owner`.
pub fn sell_ix(
    m: &Market,
    accounts: &[AccountInfo],
    keys: &SaleKeys,
    amount_in: u64,
    min_out: u64,
) -> Result<Instruction> {
    let ro = |k: Pubkey| AccountMeta::new_readonly(k, false);
    let rw = |k: Pubkey| AccountMeta::new(k, false);
    let k = |i: usize| *accounts[i].key;
    let mut data = SELL_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());
    let mut metas = vec![
        rw(k(1)),
        AccountMeta::new(keys.owner, true),
        ro(k(2)),
        ro(keys.token_mint),
        ro(keys.other_mint),
        rw(keys.source),
        rw(keys.destination),
        rw(k(3)),
        rw(k(4)),
        ro(k(5)),
        rw(k(6)),
        ro(keys.token_program),
        ro(spl::TOKEN_PROGRAM_ID),
        ro(anchor_lang::system_program::ID),
        ro(spl::ATA_PROGRAM_ID),
        ro(k(7)),
        ro(PROGRAM),
        rw(k(8)),
        ro(k(9)),
        ro(k(10)),
        ro(k(11)),
    ];
    if m.accounts == 15 {
        metas.push(ro(k(12)));
    }
    metas.push(ro(k(m.accounts - 2)));
    metas.push(rw(k(m.accounts - 1)));
    Ok(Instruction {
        program_id: PROGRAM,
        accounts: metas,
        data,
    })
}
