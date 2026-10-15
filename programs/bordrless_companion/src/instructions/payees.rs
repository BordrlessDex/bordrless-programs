//! Payees (2026-10-13): up to `MAX_PAYEES` wallets that share a companion's beneficiary part, each
//! a fixed share of it in basis points, set once before the launch by whoever holds the mint
//! (`set_payees`) and paid as SOL by every `withdraw`; the beneficiary keeps the rest.
//!
//! - **Set once.** `set_payees` makes `PDA(["payees", mint])`: the mint signs (as at `create`, so
//!   only the launcher sets them), the companion not launched and without payees yet. The
//!   companion's `version` becomes `VERSION_WITH_PAYEES`, so every `withdraw` must take the payees
//!   from then on (`PayeesMissing`): nobody can pay the beneficiary around them.
//! - **Paid by `withdraw`.** What the beneficiary part gained since the last `withdraw` is split:
//!   each payee its basis points, rounded down, the beneficiary the rest. A payee whose account was
//!   passed writable, can take SOL and keeps its rent-exempt minimum is paid what it is owed with
//!   it; any other is owed it, kept in the companion's `pending_beneficiary` (the bridged SOL stays
//!   in the creator's holding), and paid at a later `withdraw`. The beneficiary likewise: an account
//!   that can't take its part (and the spare SOL) leaves it owed, so it never blocks the payees.
//!   Nothing is ever lost or redirected.
//! - **Not** the dev bag (`release` sends it to the beneficiary) nor the launch's spare SOL (the
//!   beneficiary's, as before).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::system_program;

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::{PayeePaid, PayeeShare, PayeesSet};
use crate::instructions::payout::payable;
use crate::instructions::steps::find;
use crate::instructions::CreatorSeeds;
use crate::invoke::invoke_built;
use crate::state::*;

/// One payee of `set_payees`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PayeeArgs {
    pub wallet: Pubkey,
    /// Its part of the beneficiary part: 1 to 10,000 basis points.
    pub bps: u16,
}

/// Accounts of `set_payees`.
#[event_cpi]
#[derive(Accounts)]
pub struct SetPayees<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The launch's mint, the keypair that made the companion: only its holder sets payees.
    pub mint: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, mint.key().as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    #[account(init, payer = payer, space = Payees::LEN, seeds = [PAYEES_SEED, mint.key().as_ref()], bump)]
    pub payees: Box<Account<'info, Payees>>,
    pub system_program: Program<'info, System>,
}

/// Whether `entries` can share `companion`'s beneficiary part: 1 to `MAX_PAYEES`, each 1 to 10,000
/// basis points and 10,000 in all at most, distinct, none of them the default key, the
/// beneficiary, the creator address or the companion.
pub fn payees_valid(
    entries: &[PayeeArgs],
    beneficiary: &Pubkey,
    creator: &Pubkey,
    companion: &Pubkey,
) -> bool {
    let total: u64 = entries.iter().map(|e| u64::from(e.bps)).sum();
    (1..=MAX_PAYEES).contains(&entries.len())
        && total <= BPS
        && entries.iter().enumerate().all(|(i, e)| {
            (1..=BPS as u16).contains(&e.bps)
                && e.wallet != Pubkey::default()
                && e.wallet != *beneficiary
                && e.wallet != *creator
                && e.wallet != *companion
                && !entries[..i].iter().any(|f| f.wallet == e.wallet)
        })
}

/// `set_payees`: the payees of the beneficiary part, once, before the launch.
pub fn process_set_payees(ctx: Context<SetPayees>, entries: Vec<PayeeArgs>) -> Result<()> {
    let c = &ctx.accounts.companion;
    require!(!c.launched, CompanionError::AlreadyLaunched);
    let mint = ctx.accounts.mint.key();
    let creator = Companion::creator(&mint).0;
    require!(
        c.version == VERSION && payees_valid(&entries, &c.beneficiary, &creator, &c.key()),
        CompanionError::BadPayees
    );
    let p = &mut ctx.accounts.payees;
    p.version = PAYEES_VERSION;
    p.bump = ctx.bumps.payees;
    p.mint = mint;
    p.count = entries.len() as u8;
    for (slot, e) in p.payees.iter_mut().zip(&entries) {
        *slot = Payee {
            wallet: e.wallet,
            bps: e.bps,
            owed: 0,
            paid: 0,
        };
    }
    let companion = ctx.accounts.companion.key();
    ctx.accounts.companion.version = VERSION_WITH_PAYEES;
    emit_cpi!(PayeesSet {
        companion,
        payees: entries
            .iter()
            .map(|e| PayeeShare {
                wallet: e.wallet,
                bps: e.bps,
            })
            .collect(),
    });
    Ok(())
}

/// One event CPI of this program (as `emit_cpi!` sends it), from a helper without the `Context`.
fn emit_event<'info, E: anchor_lang::Event>(
    event_authority: &AccountInfo<'info>,
    event: &E,
) -> Result<()> {
    let ix = Instruction {
        program_id: crate::ID,
        accounts: vec![AccountMeta::new_readonly(*event_authority.key, true)],
        data: [anchor_lang::event::EVENT_IX_TAG_LE, &event.data()].concat(),
    };
    invoke_signed(
        &ix,
        core::slice::from_ref(event_authority),
        &[&[b"__event_authority", &[crate::EVENT_AUTHORITY_AND_BUMP.1]]],
    )?;
    Ok(())
}

/// What a `withdraw` pays out of a companion with payees.
pub(crate) struct PayeesOutcome {
    /// Of `pending`, what the beneficiary is paid now.
    pub to_beneficiary: u64,
    /// Whether the creator address's spare SOL goes to the beneficiary now (with its part).
    pub pay_spare: bool,
    /// What stays owed, payees' and beneficiary's: the new `pending_beneficiary`.
    pub owed: u64,
    /// What the payees were paid now.
    pub paid: u64,
}

/// What a `withdraw` of a companion with payees does with `pending` (its `pending_beneficiary`,
/// above zero) and `spare` (the creator address's SOL above its rent): what the beneficiary part
/// gained since the last withdraw is split, each payee its basis points (rounded down), the
/// beneficiary the rest. Each payee that can take what it is due (its part and what it was owed)
/// is paid it as SOL, from one `unwrap_sol`; one that can't is owed it. The beneficiary likewise:
/// when its account can't take its due and the spare SOL (not payable, or it would stay below its
/// rent-exempt minimum), both wait, so it never blocks the payees. One `PayeePaid` per payee due
/// anything. The payees account must be among `all`, writable (`PayeesMissing`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn pay_payees<'info>(
    all: &[AccountInfo<'info>],
    seeds: &CreatorSeeds,
    creator: Pubkey,
    companion: Pubkey,
    mint: &Pubkey,
    pending: u64,
    beneficiary: &AccountInfo<'info>,
    spare: u64,
) -> Result<PayeesOutcome> {
    let key = Payees::address(mint).0;
    let info = all
        .iter()
        .find(|a| *a.key == key && a.is_writable && *a.owner == crate::ID)
        .ok_or(CompanionError::PayeesMissing)?;
    let mut p = {
        let data = info.try_borrow_data()?;
        Payees::try_deserialize(&mut &data[..])
            .map_err(|_| error!(CompanionError::PayeesMissing))?
    };
    require!(p.mint == *mint, CompanionError::PayeesMissing);
    // What the beneficiary part gained since the last withdraw: what was owed stays owed, on top.
    let new = pending
        .saturating_sub(p.owed())
        .saturating_sub(p.beneficiary_owed);
    let count = usize::from(p.count).min(MAX_PAYEES);
    let (shares, rest) = payee_shares(new, &p.payees[..count]);
    let rent = Rent::get()?;
    let can_take = |a: &AccountInfo, lamports: u64| {
        lamports == 0
            || (a.is_writable
                && payable(a)
                && a.lamports()
                    .checked_add(lamports)
                    .is_some_and(|l| l >= rent.minimum_balance(a.data_len())))
    };
    // The payees.
    let mut pays: Vec<(AccountInfo<'info>, u64)> = Vec::with_capacity(count);
    let mut owed_after = 0u64;
    let mut events: Vec<PayeePaid> = Vec::with_capacity(count);
    for (payee, share) in p.payees[..count].iter_mut().zip(shares) {
        let due = payee.owed.saturating_add(share);
        if due == 0 {
            continue;
        }
        match all.iter().find(|a| *a.key == payee.wallet) {
            Some(a) if can_take(a, due) => {
                pays.push((a.clone(), due));
                payee.owed = 0;
                payee.paid = payee.paid.saturating_add(due);
                events.push(PayeePaid {
                    companion,
                    wallet: payee.wallet,
                    lamports: due,
                    owed: 0,
                });
            }
            _ => {
                payee.owed = due;
                owed_after = owed_after.saturating_add(due);
                events.push(PayeePaid {
                    companion,
                    wallet: payee.wallet,
                    lamports: 0,
                    owed: due,
                });
            }
        }
    }
    // The beneficiary: its rest and what it was owed, with the spare SOL, or all of it waits.
    let beneficiary_due = p.beneficiary_owed.saturating_add(rest);
    let (to_beneficiary, pay_spare) =
        if can_take(beneficiary, beneficiary_due.saturating_add(spare)) {
            p.beneficiary_owed = 0;
            (beneficiary_due, true)
        } else {
            p.beneficiary_owed = beneficiary_due;
            owed_after = owed_after.saturating_add(beneficiary_due);
            (0, false)
        };
    let paid: u64 = pays.iter().map(|(_, l)| *l).sum();
    if paid > 0 {
        if !seeds.native {
            invoke_built(
                &bordrless_bridge::client::unwrap_sol(creator, paid),
                all,
                &[&seeds.seeds()],
            )?;
        }
        let from = find(all, &creator)?.clone();
        let system = find(all, &system_program::ID)?.clone();
        for (to, lamports) in &pays {
            system_program::transfer(
                CpiContext::new_with_signer(
                    *system.key,
                    system_program::Transfer {
                        from: from.clone(),
                        to: to.clone(),
                    },
                    &[&seeds.seeds()],
                ),
                *lamports,
            )?;
        }
    }
    {
        let mut data = info.try_borrow_mut_data()?;
        let mut writer: &mut [u8] = &mut data;
        p.try_serialize(&mut writer)?;
    }
    let event_authority = find(all, &crate::EVENT_AUTHORITY_AND_BUMP.0)?.clone();
    for event in &events {
        emit_event(&event_authority, event)?;
    }
    Ok(PayeesOutcome {
        to_beneficiary,
        pay_spare,
        owed: owed_after,
        paid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(wallet: Pubkey, bps: u16) -> PayeeArgs {
        PayeeArgs { wallet, bps }
    }

    #[test]
    fn payees_are_few_distinct_and_within_the_part() {
        let (b, cr, co) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let w: Vec<Pubkey> = (0..11).map(|_| Pubkey::new_unique()).collect();
        assert!(payees_valid(&[e(w[0], 5_000)], &b, &cr, &co));
        assert!(payees_valid(
            &[e(w[0], 5_000), e(w[1], 5_000)],
            &b,
            &cr,
            &co
        ));
        let ten: Vec<PayeeArgs> = w[..10].iter().map(|x| e(*x, 1_000)).collect();
        assert!(payees_valid(&ten, &b, &cr, &co));
        let eleven: Vec<PayeeArgs> = w.iter().map(|x| e(*x, 100)).collect();
        assert!(!payees_valid(&eleven, &b, &cr, &co));
        assert!(!payees_valid(&[], &b, &cr, &co));
        assert!(!payees_valid(
            &[e(w[0], 5_001), e(w[1], 5_000)],
            &b,
            &cr,
            &co
        ));
        assert!(!payees_valid(&[e(w[0], 0)], &b, &cr, &co));
        assert!(!payees_valid(&[e(w[0], 10_001)], &b, &cr, &co));
        assert!(!payees_valid(&[e(w[0], 1), e(w[0], 1)], &b, &cr, &co));
        for bad in [b, cr, co, Pubkey::default()] {
            assert!(!payees_valid(&[e(bad, 1)], &b, &cr, &co));
        }
    }

    #[test]
    fn the_part_splits_down_and_the_beneficiary_keeps_the_rest() {
        let payee = |bps| Payee {
            wallet: Pubkey::new_unique(),
            bps,
            owed: 0,
            paid: 0,
        };
        let (shares, rest) = payee_shares(1_001, &[payee(3_333), payee(3_333)]);
        assert_eq!(shares, vec![333, 333]);
        assert_eq!(rest, 335);
        let (shares, rest) = payee_shares(1_000, &[payee(10_000)]);
        assert_eq!((shares, rest), (vec![1_000], 0));
        let (shares, rest) = payee_shares(0, &[payee(5_000)]);
        assert_eq!((shares, rest), (vec![0], 0));
        assert_eq!(Payees::LEN, 8 + 1 + 1 + 32 + 1 + 8 + 10 * (32 + 2 + 8 + 8));
    }
}
