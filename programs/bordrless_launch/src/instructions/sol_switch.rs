//! The SOL switch (`docs/token-quotes.md` §7.5.2): a coin quoted in tokens whose launcher chose SOL
//! (`Launch.reserved[19]` bit 0) has the tokens meant for its launcher's part, a companion's
//! holders' and launcher's parts and its holder rewards sold to SOL on each token's reference pool,
//! by one shared instruction:
//!
//! - `register_quote_pool` (anyone): a quote token's reference, `["quote-ref", quote_mint]`, one per
//!   token, shared by every coin quoted in it: a Raydium AMM v4, CPMM or CLMM pool or a Pump AMM pool
//!   of the token against wrapped SOL or USDC (USDC's own reference then values it), swapping, its
//!   vaults its own, its fee 0.01% to 3%, at least 100 SOL deep. A registered reference is replaced
//!   by a pool at least 25% deeper (both measured now), at most once an hour, or by any qualifying
//!   pool once the registered one fails a check now. `pause_quote_reference`: the admin, for
//!   emergencies.
//! - `swap_to_sol(amount_in)`: only a custodian of a coin with the switch on sells, signing as the
//!   owner of the tokens: the coin's kit config (its holder rewards in SOL, `swap_rewards_to_sol`) or
//!   its companion's creator address (`swap_bucket_to_sol`); the coin's launch sells through
//!   `swap_creator_fees` (`NotACustodian` otherwise). The price is the reference pool's: its spot,
//!   raised to 97% of its time-weighted price where it keeps one (CPMM, CLMM; from written
//!   observations only, which a sale's own transaction can't move); through USDC, the token's price
//!   in USDC times USDC's in SOL. What all sellers of a token sell per 8 slots is capped at half the
//!   pool's fee times its depth (in lamports): a push of the price costs about the fee on both legs
//!   of what it moves, so no push within the cap pays. The sale takes `min(amount_in, room)`, refuses
//!   one worth under 0.01 SOL, and requires at least the reference's value less the fee, the
//!   constant-product impact and 1%, from the venue (its own minimum out) and again here. The
//!   wrapped SOL lands in the seller's associated account (made by the crank), which is closed into
//!   the crank, who passes the SOL on to the seller (keeping the account's rent, which it paid).
//! - `swap_creator_fees(index, amount_in)` (anyone): a wallet creator's leg fees sold to SOL onto the
//!   launch account, where `claim_creator_fees_sol` pays them (`creator_fees_accrued` counts them).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program::{invoke_signed, set_return_data};
use anchor_lang::system_program;
use bordrless_swap::spl;

use crate::constants::*;
use crate::error::LaunchError;
use crate::events::{QuotePoolRegistered, QuoteReferencePaused, SwappedToSol};
use crate::instructions::launch::LaunchSeeds;
use crate::state::*;
use crate::venues::math::*;
use crate::venues::{read_market, swap_ix, Market, SaleKeys, USDC_MINT};

/// The error code an Anchor error carries.
fn code_of(e: &Error) -> Option<u32> {
    match e {
        Error::AnchorError(a) => Some(a.error_code_number),
        _ => None,
    }
}

/// Invokes `ix` with the account infos of its keys and program, found among `all`.
fn invoke_from<'info>(
    ix: &Instruction,
    all: &[AccountInfo<'info>],
    seeds: &[&[&[u8]]],
) -> Result<()> {
    let find = |key: &Pubkey| {
        all.iter()
            .find(|a| a.key == key)
            .cloned()
            .ok_or_else(|| error!(LaunchError::WrongVenueAccounts))
    };
    let mut infos = Vec::with_capacity(ix.accounts.len() + 1);
    for meta in &ix.accounts {
        infos.push(find(&meta.pubkey)?);
    }
    infos.push(find(&ix.program_id)?);
    invoke_signed(ix, &infos, seeds)?;
    Ok(())
}

/// A token account's balance under `program` (0 for one that does not exist).
fn balance(info: &AccountInfo, program: &Pubkey) -> u64 {
    spl::read_token_account(info, program).map_or(0, |t| t.amount)
}

/// Whether a reference's market qualifies now: its fee within bounds and its depth (lamports) at
/// least the minimum.
fn qualifies(m: &Market, depth_lamports: u128) -> bool {
    (MIN_REFERENCE_FEE_PPM..=MAX_REFERENCE_FEE_PPM).contains(&m.fee_ppm)
        && depth_lamports >= MIN_REFERENCE_DEPTH_LAMPORTS
}

/// USDC's reference, read from `info` (owner, discriminator, its address), its market among
/// `venue` (its registered pool, against wrapped SOL), not paused: the reference, the market and
/// its price of a raw unit of USDC in lamports.
fn usdc_market<'info>(
    info: &AccountInfo<'info>,
    usdc_mint: &AccountInfo<'info>,
    venue: &[AccountInfo<'info>],
    now: i64,
) -> Result<(QuoteReference, Market, u128)> {
    require_keys_eq!(*usdc_mint.key, USDC_MINT, LaunchError::WrongVenueAccounts);
    require_keys_eq!(
        *info.key,
        QuoteReference::address(&USDC_MINT).0,
        LaunchError::QuoteReferenceMissing
    );
    require_keys_eq!(*info.owner, crate::ID, LaunchError::QuoteReferenceMissing);
    let r = QuoteReference::try_deserialize(&mut &info.try_borrow_data()?[..])?;
    require!(!r.paused, LaunchError::ReferencePaused);
    let m = read_market(venue, usdc_mint, now)?;
    require!(
        m.pool == r.pool && m.other_mint == spl::NATIVE_MINT,
        LaunchError::WrongVenueAccounts
    );
    let price = m.reference();
    Ok((r, m, price))
}

// ---- register_quote_pool --------------------------------------------------------------------------------

/// Accounts of `register_quote_pool`. Remaining: the pool's venue accounts (as `swap_to_sol` takes
/// them, without a CLMM's tick arrays); when it is against USDC, or when replacing a reference that
/// is, USDC's reference and its pool's venue accounts; when replacing, the registered pool's venue
/// accounts last.
#[event_cpi]
#[derive(Accounts)]
pub struct RegisterQuotePool<'info> {
    /// Pays the reference's rent the first time.
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: `["quote-ref", quote_mint]` (seeds-checked), made here or replaced.
    #[account(mut, seeds = [QUOTE_REF_SEED, quote_mint.key().as_ref()], bump)]
    pub quote_reference: UncheckedAccount<'info>,
    /// CHECK: the quote token: an SPL Token or Token-2022 mint (checked in the handler).
    pub quote_mint: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `register_quote_pool()`: see the module.
pub fn process_register_quote_pool<'info>(
    ctx: Context<'info, RegisterQuotePool<'info>>,
) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let mint = ctx.accounts.quote_mint.to_account_info();
    require!(
        spl::program_index(mint.owner).is_some(),
        LaunchError::QuoteMintInvalid
    );
    let rest = ctx.remaining_accounts;
    let new = read_market(rest, &mint, now)?;
    let mut at = new.accounts;
    let info = ctx.accounts.quote_reference.to_account_info();
    let old = if *info.owner == crate::ID {
        Some(QuoteReference::try_deserialize(
            &mut &info.try_borrow_data()?[..],
        )?)
    } else {
        None
    };
    // USDC's price in lamports, when either side of the comparison is against USDC.
    let usdc_needed =
        new.other_mint == USDC_MINT || old.as_ref().is_some_and(|o| o.via == VIA_USDC);
    let usdc_price = if usdc_needed {
        require!(
            *mint.key != USDC_MINT && rest.len() > at + 1,
            LaunchError::QuoteReferenceMissing
        );
        let (_, m, price) = usdc_market(&rest[at], &rest[at + 1], &rest[at + 2..], now)?;
        at += 2 + m.accounts;
        price
    } else {
        0
    };
    let lamports_of = |m: &Market| -> u128 {
        if m.other_mint == USDC_MINT {
            mul_div(m.depth, usdc_price, Q64).unwrap_or(0)
        } else {
            m.depth
        }
    };
    let depth = lamports_of(&new);
    require!(
        (MIN_REFERENCE_FEE_PPM..=MAX_REFERENCE_FEE_PPM).contains(&new.fee_ppm),
        LaunchError::ReferencePoolInvalid
    );
    require!(
        depth >= MIN_REFERENCE_DEPTH_LAMPORTS,
        LaunchError::ReferencePoolShallow
    );
    if let Some(o) = &old {
        if o.pool != new.pool {
            // The registered pool, measured now: a failure of its own (a check it no longer
            // passes) lets any qualifying pool replace it; accounts that are not its own refuse
            // (they must name its venue and its pool before anything is read of them).
            let old_accounts = &rest[at.min(rest.len())..];
            require!(
                crate::venues::pool_of(old_accounts) == Some((o.venue, o.pool)),
                LaunchError::WrongVenueAccounts
            );
            let passes = match read_market(old_accounts, &mint, now) {
                Ok(m) => {
                    require_keys_eq!(m.pool, o.pool, LaunchError::WrongVenueAccounts);
                    let d = lamports_of(&m);
                    qualifies(&m, d).then_some(d)
                }
                Err(e) if code_of(&e) == Some(u32::from(LaunchError::WrongVenueAccounts)) => {
                    return Err(e)
                }
                Err(_) => None,
            };
            if let Some(old_depth) = passes {
                require!(
                    now.saturating_sub(o.registered_at) >= REPLACE_INTERVAL_SECS,
                    LaunchError::ReferenceReplacedRecently
                );
                require!(
                    depth * BPS >= old_depth * REPLACE_DEEPER_BPS,
                    LaunchError::ReferenceNotDeeper
                );
            }
        }
    }
    // Made the first time (the payer paying); written whole either way, its window and totals kept.
    // The same pool registered again keeps its registrant and its time (a refresh of its depth
    // only), so nobody holds off a deeper pool by registering the current one every hour.
    let bump = ctx.bumps.quote_reference;
    let same = old.as_ref().filter(|o| o.pool == new.pool);
    if old.is_none() {
        let seeds: &[&[u8]] = &[QUOTE_REF_SEED, mint.key.as_ref(), &[bump]];
        crate::instructions::token_launch::create_pda_account(
            &ctx.accounts.payer.to_account_info(),
            &info,
            &ctx.accounts.system_program.to_account_info(),
            QuoteReference::LEN,
            &crate::ID,
            seeds,
        )?;
    }
    let reference = QuoteReference {
        version: VERSION,
        bump,
        quote_mint: *mint.key,
        venue: new.venue,
        via: if new.other_mint == USDC_MINT {
            VIA_USDC
        } else {
            VIA_SOL
        },
        pool: new.pool,
        registrant: same.map_or(ctx.accounts.payer.key(), |o| o.registrant),
        registered_at: same.map_or(now, |o| o.registered_at),
        depth_at: u64::try_from(depth).unwrap_or(u64::MAX),
        window: old.as_ref().map_or(0, |o| o.window),
        window_value: old.as_ref().map_or(0, |o| o.window_value),
        paused: false,
        sold_total: old.as_ref().map_or(0, |o| o.sold_total),
        lamports_total: old.as_ref().map_or(0, |o| o.lamports_total),
        reserved: [0; 32],
    };
    {
        let mut data = info.try_borrow_mut_data()?;
        let mut w: &mut [u8] = &mut data;
        reference.try_serialize(&mut w)?;
    }
    emit_cpi!(QuotePoolRegistered {
        quote_mint: *mint.key,
        reference: info.key(),
        venue: reference.venue,
        via: reference.via,
        pool: reference.pool,
        replaced: old.map(|o| o.pool),
        depth_lamports: reference.depth_at,
        fee_ppm: u32::try_from(new.fee_ppm).unwrap_or(u32::MAX),
        registrant: reference.registrant,
        ts: now,
    });
    Ok(())
}

// ---- pause_quote_reference ----------------------------------------------------------------------------

/// Accounts of `pause_quote_reference`.
#[event_cpi]
#[derive(Accounts)]
pub struct PauseQuoteReference<'info> {
    pub admin: Signer<'info>,
    #[account(address = CONFIG_ADDRESS, has_one = admin @ LaunchError::NotAdmin)]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, seeds = [QUOTE_REF_SEED, quote_reference.quote_mint.as_ref()], bump = quote_reference.bump)]
    pub quote_reference: Box<Account<'info, QuoteReference>>,
}

/// `pause_quote_reference(paused)`: the admin stops (or resumes) every sale on a token's reference.
pub fn process_pause_quote_reference(
    ctx: Context<PauseQuoteReference>,
    paused: bool,
) -> Result<()> {
    let r = &mut ctx.accounts.quote_reference;
    r.paused = paused;
    emit_cpi!(QuoteReferencePaused {
        quote_mint: r.quote_mint,
        paused,
        ts: Clock::get()?.unix_timestamp,
    });
    Ok(())
}

// ---- The sale ---------------------------------------------------------------------------------------------

/// What a sale names (`swap_to_sol`'s accounts, or `swap_creator_fees`'), the seller among them.
pub struct SaleAccounts<'a, 'info> {
    pub crank: &'a AccountInfo<'info>,
    pub authority: &'a AccountInfo<'info>,
    pub launch: Pubkey,
    pub mint: Pubkey,
    pub quote_mint: &'a AccountInfo<'info>,
    pub source: &'a AccountInfo<'info>,
    pub reference: &'a mut QuoteReference,
    pub wsol_account: &'a AccountInfo<'info>,
    pub wsol_mint: &'a AccountInfo<'info>,
    pub source_token_program: &'a AccountInfo<'info>,
    pub spl_token_program: &'a AccountInfo<'info>,
    pub associated_token_program: &'a AccountInfo<'info>,
    pub system_program: &'a AccountInfo<'info>,
    /// The venue accounts (and USDC's, through USDC).
    pub rest: &'a [AccountInfo<'info>],
}

/// What a sale did.
pub struct Sale {
    pub amount_in: u64,
    pub lamports: u64,
    pub min_out: u64,
    /// Lamports per raw unit of the token, Q64.64.
    pub price: u128,
}

/// The authority's associated token account of an SPL Token `mint` (`info`), made when missing (the
/// crank paying): its balance before the sale and whether it was made here.
fn ensure_ata<'info>(
    a: &SaleAccounts<'_, 'info>,
    info: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
) -> Result<(u64, bool)> {
    require_keys_eq!(
        *info.key,
        spl::associated_token_address(a.authority.key, mint.key, &spl::TOKEN_PROGRAM_ID),
        LaunchError::WrongVenueAccounts
    );
    let made = *info.owner == anchor_lang::system_program::ID && info.data_is_empty();
    spl::create_ata_idempotent(
        a.crank,
        info,
        a.authority,
        mint,
        a.system_program,
        a.spl_token_program,
        a.associated_token_program,
    )?;
    Ok((balance(info, &spl::TOKEN_PROGRAM_ID), made))
}

/// SPL Token `CloseAccount` (9) of the authority's `account` into the crank.
fn close_into_crank<'info>(
    a: &SaleAccounts<'_, 'info>,
    account: &AccountInfo<'info>,
    seeds: &[&[&[u8]]],
) -> Result<()> {
    let ix = Instruction {
        program_id: spl::TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*account.key, false),
            AccountMeta::new(*a.crank.key, false),
            AccountMeta::new_readonly(*a.authority.key, true),
        ],
        data: vec![9],
    };
    invoke_signed(
        &ix,
        &[
            account.clone(),
            a.crank.clone(),
            a.authority.clone(),
            a.spl_token_program.clone(),
        ],
        seeds,
    )?;
    Ok(())
}

/// The sale of up to `amount_in` of leg `leg`'s token from `a.source` (the authority's) to SOL, the
/// authority signing (`seeds` for a PDA of this program): see the module. The SOL ends on the
/// authority.
pub fn sell<'info>(
    a: SaleAccounts<'_, 'info>,
    leg: &Leg,
    amount_in: u64,
    seeds: &[&[&[u8]]],
) -> Result<Sale> {
    let clock = Clock::get()?;
    let (now, window) = (clock.unix_timestamp, clock.slot / SWAP_WINDOW_SLOTS);
    require!(!a.reference.paused, LaunchError::ReferencePaused);
    require_keys_eq!(
        a.reference.quote_mint,
        leg.quote_mint,
        LaunchError::QuoteReferenceMissing
    );
    require_keys_eq!(
        *a.wsol_mint.key,
        spl::NATIVE_MINT,
        LaunchError::WrongVenueAccounts
    );
    let program = spl::program_id(leg.quote_program);
    require_keys_eq!(
        *a.source_token_program.key,
        program,
        LaunchError::WrongLegAccount
    );
    let source = spl::read_token_account(a.source, &program).ok_or(LaunchError::WrongLegAccount)?;
    require!(
        source.mint == leg.quote_mint && source.owner == *a.authority.key,
        LaunchError::NotACustodian
    );
    // The markets: the token's reference pool, and through USDC USDC's.
    let via_usdc = a.reference.via == VIA_USDC;
    let (hop1_at, usdc) = if via_usdc {
        require!(a.rest.len() > 3, LaunchError::QuoteReferenceMissing);
        (3, Some((&a.rest[0], &a.rest[1], &a.rest[2])))
    } else {
        (0, None)
    };
    let m1 = read_market(&a.rest[hop1_at..], a.quote_mint, now)?;
    require!(
        m1.pool == a.reference.pool
            && m1.other_mint
                == if via_usdc {
                    USDC_MINT
                } else {
                    spl::NATIVE_MINT
                },
        LaunchError::WrongVenueAccounts
    );
    let hop2_at = hop1_at + m1.accounts;
    let mut usdc_ref: Option<(QuoteReference, Market, u128)> = None;
    if let Some((usdc_mint, _, usdc_reference)) = usdc {
        usdc_ref = Some(usdc_market(
            usdc_reference,
            usdc_mint,
            &a.rest[hop2_at.min(a.rest.len())..],
            now,
        )?);
    }
    // The prices and the depth, in lamports.
    let p1 = m1.reference();
    let (price, depth) = match &usdc_ref {
        Some((_, _, p_usdc)) => (
            mul_q64(p1, *p_usdc).ok_or(LaunchError::ReferencePoolInvalid)?,
            mul_div(m1.depth, *p_usdc, Q64).unwrap_or(0),
        ),
        None => (p1, m1.depth),
    };
    require!(
        (MIN_REFERENCE_FEE_PPM..=MAX_REFERENCE_FEE_PPM).contains(&m1.fee_ppm),
        LaunchError::ReferencePoolInvalid
    );
    require!(
        depth >= MIN_REFERENCE_DEPTH_LAMPORTS,
        LaunchError::ReferencePoolShallow
    );
    // The window: what every seller of the token may still sell in these 8 slots (and, through
    // USDC, what USDC's reference may).
    if a.reference.window != window {
        a.reference.window = window;
        a.reference.window_value = 0;
    }
    let mut room =
        window_cap(m1.fee_ppm, depth).saturating_sub(u128::from(a.reference.window_value));
    if let Some((r, m2, p_usdc)) = &mut usdc_ref {
        if r.window != window {
            r.window = window;
            r.window_value = 0;
        }
        let _ = p_usdc;
        room =
            room.min(window_cap(m2.fee_ppm, m2.depth).saturating_sub(u128::from(r.window_value)));
    }
    require!(room >= MIN_SWAP_LAMPORTS, LaunchError::SwapWindowFull);
    let mut amount = amount_in.min(source.amount);
    if value_of(amount, price) > room {
        amount = u64::try_from(amount_for(room, price).unwrap_or(0)).unwrap_or(u64::MAX);
    }
    let value = value_of(amount, price);
    require!(
        amount > 0 && value >= MIN_SWAP_LAMPORTS,
        LaunchError::SwapTooSmall
    );
    // The first hop: the token for the pool's other side, into the authority's account of it.
    let mut all: Vec<AccountInfo<'info>> = vec![
        a.authority.clone(),
        a.source.clone(),
        a.quote_mint.clone(),
        a.wsol_account.clone(),
        a.wsol_mint.clone(),
        a.source_token_program.clone(),
        a.spl_token_program.clone(),
        a.associated_token_program.clone(),
        a.system_program.clone(),
    ];
    all.extend(a.rest.iter().cloned());
    let (wsol_before, _) = ensure_ata(&a, a.wsol_account, a.wsol_mint)?;
    let min1 =
        u64::try_from(min_out(value_of(amount, p1), m1.fee_ppm, m1.depth)).unwrap_or(u64::MAX);
    let (out, min_final) = match (usdc, &usdc_ref) {
        (Some((usdc_mint, usdc_account, _)), Some((_, m2, p_usdc))) => {
            let (usdc_before, usdc_made) = ensure_ata(&a, usdc_account, usdc_mint)?;
            let ix = swap_ix(
                &m1,
                &a.rest[hop1_at..],
                &SaleKeys {
                    owner: *a.authority.key,
                    source: *a.source.key,
                    destination: *usdc_account.key,
                    token_mint: leg.quote_mint,
                    token_program: program,
                    other_mint: USDC_MINT,
                },
                amount,
                min1,
            )?;
            invoke_from(&ix, &all, seeds)?;
            let got = balance(usdc_account, &spl::TOKEN_PROGRAM_ID)
                .checked_sub(usdc_before)
                .ok_or(LaunchError::MathOverflow)?;
            require!(got >= min1, LaunchError::SwapBelowMinimum);
            let min2 = u64::try_from(min_out(value_of(got, *p_usdc), m2.fee_ppm, m2.depth))
                .unwrap_or(u64::MAX);
            let ix = swap_ix(
                m2,
                &a.rest[hop2_at..],
                &SaleKeys {
                    owner: *a.authority.key,
                    source: *usdc_account.key,
                    destination: *a.wsol_account.key,
                    token_mint: USDC_MINT,
                    token_program: spl::TOKEN_PROGRAM_ID,
                    other_mint: spl::NATIVE_MINT,
                },
                got,
                min2,
            )?;
            invoke_from(&ix, &all, seeds)?;
            if usdc_made && balance(usdc_account, &spl::TOKEN_PROGRAM_ID) == 0 {
                close_into_crank(&a, usdc_account, seeds)?;
            }
            let out = balance(a.wsol_account, &spl::TOKEN_PROGRAM_ID)
                .checked_sub(wsol_before)
                .ok_or(LaunchError::MathOverflow)?;
            (out, min2)
        }
        _ => {
            let ix = swap_ix(
                &m1,
                &a.rest[hop1_at..],
                &SaleKeys {
                    owner: *a.authority.key,
                    source: *a.source.key,
                    destination: *a.wsol_account.key,
                    token_mint: leg.quote_mint,
                    token_program: program,
                    other_mint: spl::NATIVE_MINT,
                },
                amount,
                min1,
            )?;
            invoke_from(&ix, &all, seeds)?;
            let out = balance(a.wsol_account, &spl::TOKEN_PROGRAM_ID)
                .checked_sub(wsol_before)
                .ok_or(LaunchError::MathOverflow)?;
            (out, min1)
        }
    };
    require!(out >= min_final, LaunchError::SwapBelowMinimum);
    let sold = source
        .amount
        .checked_sub(balance(a.source, &program))
        .ok_or(LaunchError::MathOverflow)?;
    // The wrapped SOL to the authority as SOL: the account closed into the crank (all its lamports:
    // the SOL and its rent), the SOL passed on (the crank keeps the rent; it paid it when it made the
    // account here).
    let wsol = balance(a.wsol_account, &spl::TOKEN_PROGRAM_ID);
    close_into_crank(&a, a.wsol_account, seeds)?;
    system_program::transfer(
        CpiContext::new(
            *a.system_program.key,
            system_program::Transfer {
                from: a.crank.clone(),
                to: a.authority.clone(),
            },
        ),
        wsol,
    )?;
    // The windows and totals.
    a.reference.window_value = a
        .reference
        .window_value
        .saturating_add(u64::try_from(value).unwrap_or(u64::MAX));
    a.reference.sold_total = a.reference.sold_total.saturating_add(sold);
    a.reference.lamports_total = a.reference.lamports_total.saturating_add(out);
    if let (Some((_, _, usdc_reference)), Some((mut r, _, _))) = (usdc, usdc_ref) {
        r.window_value = r
            .window_value
            .saturating_add(u64::try_from(value).unwrap_or(u64::MAX));
        r.lamports_total = r.lamports_total.saturating_add(out);
        let mut data = usdc_reference.try_borrow_mut_data()?;
        let mut w: &mut [u8] = &mut data;
        r.try_serialize(&mut w)?;
    }
    Ok(Sale {
        amount_in: sold,
        lamports: out,
        min_out: min_final,
        price,
    })
}

/// `(sold, lamports)` as return data: what the callers (the kit, the companion) may read.
fn answer(sale: &Sale) {
    let mut data = [0u8; 16];
    data[..8].copy_from_slice(&sale.amount_in.to_le_bytes());
    data[8..].copy_from_slice(&sale.lamports.to_le_bytes());
    set_return_data(&data);
}

// ---- swap_to_sol ----------------------------------------------------------------------------------------------

/// Accounts of `swap_to_sol(amount_in)`. Remaining: the reference pool's venue accounts (a CLMM's
/// bitmap extension and tick arrays after them); through USDC, first USDC's mint, the authority's
/// USDC account (w) and USDC's reference (w), then the token's pool's venue accounts, then USDC's
/// pool's.
#[event_cpi]
#[derive(Accounts)]
pub struct SwapToSol<'info> {
    /// Pays the wrapped-SOL account's rent and gets it back.
    #[account(mut)]
    pub crank: Signer<'info>,
    /// The tokens' owner: the coin's kit config or its companion's creator address, signing through
    /// its program. Receives the SOL.
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(seeds = [LAUNCH_SEED, launch.mint.as_ref()], bump = launch.bump)]
    pub launch: Box<Account<'info, Launch>>,
    #[account(seeds = [LEGS_SEED, launch.mint.as_ref()], bump = legs.bump)]
    pub legs: Box<Account<'info, Legs>>,
    /// CHECK: one of the legs' quote mints (checked in the handler).
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: the authority's token account of it (checked in the handler).
    #[account(mut)]
    pub source: UncheckedAccount<'info>,
    #[account(mut, seeds = [QUOTE_REF_SEED, quote_mint.key().as_ref()], bump = quote_reference.bump)]
    pub quote_reference: Box<Account<'info, QuoteReference>>,
    /// CHECK: the authority's associated wrapped-SOL account (made here, closed here).
    #[account(mut)]
    pub wsol_account: UncheckedAccount<'info>,
    /// CHECK: wrapped SOL.
    #[account(address = spl::NATIVE_MINT @ LaunchError::WrongVenueAccounts)]
    pub wsol_mint: UncheckedAccount<'info>,
    /// CHECK: the leg token's program (checked in the handler).
    pub source_token_program: UncheckedAccount<'info>,
    /// CHECK: SPL Token.
    #[account(address = spl::TOKEN_PROGRAM_ID @ LaunchError::WrongProgram)]
    pub spl_token_program: UncheckedAccount<'info>,
    /// CHECK: the associated token account program.
    #[account(address = spl::ATA_PROGRAM_ID @ LaunchError::WrongProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// The leg of `legs` quoted in `quote_mint` (`WrongLegAccount` for a token that is none of its).
fn leg_of(legs: &Legs, quote_mint: &Pubkey) -> Result<Leg> {
    legs.in_use()
        .iter()
        .find(|l| l.quote_mint == *quote_mint)
        .copied()
        .ok_or_else(|| error!(LaunchError::WrongLegAccount))
}

/// `swap_to_sol(amount_in)`: see the module.
pub fn process_swap_to_sol<'info>(
    ctx: Context<'info, SwapToSol<'info>>,
    amount_in: u64,
) -> Result<()> {
    let a = &ctx.accounts;
    let launch = &a.launch;
    require!(launch.sol_switch(), LaunchError::SolSwitchOff);
    // A custodian of this coin's tokens: its kit config, or its companion's creator address.
    let authority = a.authority.key();
    require!(
        authority == launch.kit_config || is_companion_creator(&authority, &launch.mint),
        LaunchError::NotACustodian
    );
    let leg = leg_of(&a.legs, a.quote_mint.key)?;
    let (launch_key, mint) = (launch.key(), launch.mint);
    let crank = a.crank.to_account_info();
    let authority_info = a.authority.to_account_info();
    let quote_mint = a.quote_mint.to_account_info();
    let source = a.source.to_account_info();
    let wsol_account = a.wsol_account.to_account_info();
    let wsol_mint = a.wsol_mint.to_account_info();
    let source_token_program = a.source_token_program.to_account_info();
    let spl_token_program = a.spl_token_program.to_account_info();
    let associated_token_program = a.associated_token_program.to_account_info();
    let system = a.system_program.to_account_info();
    let reference = &mut ctx.accounts.quote_reference;
    let sale = sell(
        SaleAccounts {
            crank: &crank,
            authority: &authority_info,
            launch: launch_key,
            mint,
            quote_mint: &quote_mint,
            source: &source,
            reference,
            wsol_account: &wsol_account,
            wsol_mint: &wsol_mint,
            source_token_program: &source_token_program,
            spl_token_program: &spl_token_program,
            associated_token_program: &associated_token_program,
            system_program: &system,
            rest: ctx.remaining_accounts,
        },
        &leg,
        amount_in,
        &[],
    )?;
    let r = &ctx.accounts.quote_reference;
    emit_cpi!(SwappedToSol {
        launch: launch_key,
        mint,
        quote_mint: leg.quote_mint,
        authority,
        venue: r.venue,
        via: r.via,
        amount_in: sale.amount_in,
        lamports_out: sale.lamports,
        min_out: sale.min_out,
        price: sale.price,
        window_value: r.window_value,
        ts: Clock::get()?.unix_timestamp,
    });
    answer(&sale);
    Ok(())
}

// ---- swap_creator_fees -----------------------------------------------------------------------------------

/// Accounts of `swap_creator_fees(index, amount_in)`: `swap_to_sol`'s with the launch (w) as the
/// seller and the leg's fee account as the source. Remaining: as `swap_to_sol`'s.
#[event_cpi]
#[derive(Accounts)]
pub struct SwapCreatorFees<'info> {
    /// Pays the wrapped-SOL account's rent and gets it back.
    #[account(mut)]
    pub crank: Signer<'info>,
    #[account(mut, seeds = [LAUNCH_SEED, launch.mint.as_ref()], bump = launch.bump)]
    pub launch: Box<Account<'info, Launch>>,
    #[account(seeds = [LEGS_SEED, launch.mint.as_ref()], bump = legs.bump)]
    pub legs: Box<Account<'info, Legs>>,
    /// CHECK: the leg's quote mint (checked in the handler).
    pub quote_mint: UncheckedAccount<'info>,
    /// CHECK: the leg's fee account (checked in the handler).
    #[account(mut)]
    pub leg_fees: UncheckedAccount<'info>,
    #[account(mut, seeds = [QUOTE_REF_SEED, quote_mint.key().as_ref()], bump = quote_reference.bump)]
    pub quote_reference: Box<Account<'info, QuoteReference>>,
    /// CHECK: the launch's associated wrapped-SOL account (made here, closed here).
    #[account(mut)]
    pub wsol_account: UncheckedAccount<'info>,
    /// CHECK: wrapped SOL.
    #[account(address = spl::NATIVE_MINT @ LaunchError::WrongVenueAccounts)]
    pub wsol_mint: UncheckedAccount<'info>,
    /// CHECK: the leg token's program (checked in the handler).
    pub source_token_program: UncheckedAccount<'info>,
    /// CHECK: SPL Token.
    #[account(address = spl::TOKEN_PROGRAM_ID @ LaunchError::WrongProgram)]
    pub spl_token_program: UncheckedAccount<'info>,
    /// CHECK: the associated token account program.
    #[account(address = spl::ATA_PROGRAM_ID @ LaunchError::WrongProgram)]
    pub associated_token_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// `swap_creator_fees(index, amount_in)`: leg `index`'s creator fees (its fee account, the launch
/// signing) sold to SOL onto the launch account, for a wallet creator with the switch on
/// (`CompanionCreator` for a companion's: the companion sells its own parts).
pub fn process_swap_creator_fees<'info>(
    ctx: Context<'info, SwapCreatorFees<'info>>,
    index: u8,
    amount_in: u64,
) -> Result<()> {
    let a = &ctx.accounts;
    let launch = &a.launch;
    require!(launch.sol_switch(), LaunchError::SolSwitchOff);
    require!(
        !is_companion_creator(&launch.creator, &launch.mint),
        LaunchError::CompanionCreator
    );
    let i = usize::from(index);
    require!(i < usize::from(a.legs.count), LaunchError::NoSuchLeg);
    let leg = a.legs.legs[i];
    require!(
        a.quote_mint.key() == leg.quote_mint && a.leg_fees.key() == leg.fee_account,
        LaunchError::WrongLegAccount
    );
    let (launch_key, mint, bump) = (launch.key(), launch.mint, launch.bump);
    let crank = a.crank.to_account_info();
    let launch_info = a.launch.to_account_info();
    let quote_mint = a.quote_mint.to_account_info();
    let source = a.leg_fees.to_account_info();
    let wsol_account = a.wsol_account.to_account_info();
    let wsol_mint = a.wsol_mint.to_account_info();
    let source_token_program = a.source_token_program.to_account_info();
    let spl_token_program = a.spl_token_program.to_account_info();
    let associated_token_program = a.associated_token_program.to_account_info();
    let system = a.system_program.to_account_info();
    let launch_seeds = LaunchSeeds::new(mint, bump);
    let reference = &mut ctx.accounts.quote_reference;
    let sale = sell(
        SaleAccounts {
            crank: &crank,
            authority: &launch_info,
            launch: launch_key,
            mint,
            quote_mint: &quote_mint,
            source: &source,
            reference,
            wsol_account: &wsol_account,
            wsol_mint: &wsol_mint,
            source_token_program: &source_token_program,
            spl_token_program: &spl_token_program,
            associated_token_program: &associated_token_program,
            system_program: &system,
            rest: ctx.remaining_accounts,
        },
        &leg,
        amount_in,
        &[&launch_seeds.seeds()],
    )?;
    let l = &mut ctx.accounts.launch;
    l.creator_fees_accrued = l.creator_fees_accrued.saturating_add(sale.lamports);
    let r = &ctx.accounts.quote_reference;
    emit_cpi!(SwappedToSol {
        launch: launch_key,
        mint,
        quote_mint: leg.quote_mint,
        authority: launch_key,
        venue: r.venue,
        via: r.via,
        amount_in: sale.amount_in,
        lamports_out: sale.lamports,
        min_out: sale.min_out,
        price: sale.price,
        window_value: r.window_value,
        ts: Clock::get()?.unix_timestamp,
    });
    answer(&sale);
    Ok(())
}
