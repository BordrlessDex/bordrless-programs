//! Scripts with a launcher's part on a coin quoted in tokens (`docs/token-quotes.md` §7.6): the
//! Lossback pair, and any Workshop script whose split leaves the launcher a part. Their parts are
//! kept per leg in the companion's `CompanionLegs` (as the presets' are): every leg's fee is
//! claimed and split by `claim_fees_leg`, the pot's part to the leg's `pending_pot`, the launcher's
//! to its `pending_beneficiary` (`withdraw_leg` pays it).
//!
//! - **A step holds each leg to its pot.** The creator address's account of each leg's token the
//!   step was handed is read before and after the script runs: the leg's pot moves by what the
//!   account gained or lost (down by what the script spent, up by what it sold), and never below
//!   zero (`ScriptOverspent`): the other parts set aside in that account (the launcher's, the
//!   buyback's, the holders') stay. Tokens that reached the account unsplit (a fee claimed by
//!   someone else) stay unsplit for `claim_fees_leg`.
//! - **Payouts in the legs' tokens** (`PayoutAnswerLegs`, version 3): each entry names a holding,
//!   its leg and an amount of that leg's token, paid to the holding's owner's associated token
//!   account of it (which must be passed, writable and able to take it: `PayoutTarget`) from that
//!   leg's pot. The terms bound each leg as they bound a SOL pot: `step_bps` of the leg's pot at the
//!   step's start, `window_bps` of the leg's pot over the window (its pot at the window's first
//!   step in `LegPot.prize`, what the window paid in `prize_paid`), `holding_bps` of the leg's step
//!   budget per holding. The hook records each one (`record_payout(amount, tag)`), as for a SOL
//!   payout. The sender's bounty stays SOL, from the SOL pot.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::Discriminator;
use bordrless_game::eligible;
use bordrless_hook::token_flags;
use bordrless_swap::spl;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::LegScriptPayout;
use crate::instructions::legs::receivable;
use crate::instructions::payout::{load_terms, step_budget, StepPayouts};
use crate::instructions::steps::find;
use crate::instructions::token_games::{LegToken, TokenPots};
use crate::state::*;

/// What the creator address's account of each leg's token holds, for the legs whose account is
/// among `all` (`None` for a leg whose account was not handed to the step: the script can't
/// touch it).
pub(crate) fn leg_balances(
    all: &[AccountInfo],
    pots: &TokenPots,
    creator: &Pubkey,
) -> [Option<u64>; MAX_COMPANION_LEGS] {
    let mut out = [None; MAX_COMPANION_LEGS];
    for (i, b) in out.iter_mut().enumerate().take(pots.count()) {
        let pot = &pots.legs.legs[i];
        let program = spl::program_id(pot.quote_program);
        let key = spl::associated_token_address(creator, &pot.quote_mint, &program);
        *b = all
            .iter()
            .find(|a| *a.key == key)
            .and_then(|a| spl::read_token_account(a, &program))
            .map(|t| t.amount);
    }
    out
}

/// Each leg's pot moved by what its account gained or lost while the script ran; `ScriptOverspent`
/// when a leg's account lost more than the leg's pot.
pub(crate) fn settle_leg_pots(
    pots: &mut TokenPots,
    before: &[Option<u64>; MAX_COMPANION_LEGS],
    after: &[Option<u64>; MAX_COMPANION_LEGS],
) -> Result<()> {
    for i in 0..pots.count() {
        let (Some(b), Some(a)) = (before[i], after[i]) else {
            continue;
        };
        let p = &mut pots.legs.legs[i];
        p.pending_pot = if a >= b {
            p.pending_pot
                .checked_add(a - b)
                .ok_or(CompanionError::MathOverflow)?
        } else {
            p.pending_pot
                .checked_sub(b - a)
                .ok_or(CompanionError::ScriptOverspent)?
        };
    }
    Ok(())
}

/// One checked entry of a version-3 answer.
struct LegPayoutEntry<'a, 'info> {
    holding: &'a AccountInfo<'info>,
    owner: Pubkey,
    leg: usize,
    to: Option<&'a AccountInfo<'info>>,
    amount: u64,
    tag: u32,
}

/// Pays and records a version-3 answer (one entry at least) from the legs' pots (`pots`, after the
/// script ran; `pots_before` each leg's pot at the step's start): the terms, the window per leg
/// (rolled at its first step), then every entry checked before any payment; then the payments, the
/// records and the accounting (the leg's pot and window, the terms' counters, one
/// `LegScriptPayout` per entry). The sender's SOL bounty is paid by the caller.
pub(crate) fn pay_leg_holdings<'a, 'info>(
    p: &StepPayouts<'a, 'info>,
    pots: &mut TokenPots<'a, 'info>,
    pots_before: &[u64; MAX_COMPANION_LEGS],
    answer: &PayoutAnswerLegs,
) -> Result<()> {
    let mint = p.companion.mint;
    // 1. The terms and the hook.
    let (terms_key, terms_info, mut t) = load_terms(p)?;
    let hook = p
        .launch
        .custom_hook
        .filter(|_| p.launch.custom_hook_flags & token_flags::WRITES_HOOK_DATA != 0)
        .ok_or(CompanionError::PayoutsNeedHook)?;
    // 2. The window, per leg, rolled at its first step; each leg's budget and holding cap.
    let count = pots.count();
    let window_start = t.window_start_at(p.now);
    if t.window_start != window_start {
        t.window_start = window_start;
        t.window_pot = 0;
        t.window_paid = 0;
        for i in 0..count {
            let leg = &mut pots.legs.legs[i];
            leg.prize = leg.pending_pot;
            leg.prize_paid = 0;
        }
    }
    let mut budget = [0u64; MAX_COMPANION_LEGS];
    let mut cap = [0u64; MAX_COMPANION_LEGS];
    for i in 0..count {
        let leg = &pots.legs.legs[i];
        budget[i] = step_budget(
            pots_before[i],
            leg.pending_pot,
            0,
            leg.prize,
            leg.prize_paid,
            t.step_bps,
            t.window_bps,
        );
        cap[i] = bps_of(budget[i], u64::from(t.holding_bps));
    }
    // 3. Each entry.
    let excluded = [
        p.launch_key,
        p.launch.pool,
        p.creator,
        p.companion_key,
        p.script.program,
        p.script_key,
        p.script_state_key,
        terms_key,
        hook,
    ];
    let mut entries: Vec<LegPayoutEntry<'a, 'info>> = Vec::with_capacity(answer.entries.len());
    let mut total = [0u64; MAX_COMPANION_LEGS];
    for (i, e) in answer.entries.iter().enumerate() {
        let Some(info) = p.remaining.get(usize::from(e.account)) else {
            msg!("payout {}: no account at index {}", i, e.account);
            return err!(CompanionError::BadPayoutAnswer);
        };
        let holding = if *info.owner == TOKEN_ID {
            token_client::read_holding(info)
                .ok()
                .filter(|h| h.mint == mint)
        } else {
            None
        };
        let Some(h) = holding else {
            msg!(
                "payout {}: account {} is no holding of the mint",
                i,
                e.account
            );
            return err!(CompanionError::BadPayoutAnswer);
        };
        if entries.iter().any(|q| q.holding.key == info.key) {
            msg!("payout {}: holding named twice", i);
            return err!(CompanionError::BadPayoutAnswer);
        }
        let leg = usize::from(e.leg);
        require!(leg < count, CompanionError::PayoutLegInvalid);
        let sum = total[leg].checked_add(e.amount);
        if e.amount > cap[leg] || sum.is_none_or(|s| s > budget[leg]) {
            msg!(
                "payout {}: {} of leg {} over the bound (holding cap {}, budget {}, paid before it {})",
                i,
                e.amount,
                leg,
                cap[leg],
                budget[leg],
                total[leg]
            );
            return err!(CompanionError::PayoutOverBound);
        }
        total[leg] = sum.unwrap_or(total[leg]);
        if !eligible(&h.owner, &excluded) {
            msg!("payout {}: the owner can't be paid", i);
            return err!(CompanionError::PayoutTarget);
        }
        require!(
            h.owner != p.companion.beneficiary,
            CompanionError::PayoutToBeneficiary
        );
        let to = if e.amount > 0 {
            let pot = &pots.legs.legs[leg];
            let program = spl::program_id(pot.quote_program);
            let key = spl::associated_token_address(&h.owner, &pot.quote_mint, &program);
            let Some(dest) = p.remaining.iter().find(|a| *a.key == key) else {
                msg!(
                    "payout {}: the owner's account of leg {}'s token was not passed",
                    i,
                    leg
                );
                return err!(CompanionError::PayoutLegInvalid);
            };
            if !receivable(dest, &program, &pot.quote_mint, &h.owner) {
                msg!(
                    "payout {}: the owner's account of leg {}'s token can't take it",
                    i,
                    leg
                );
                return err!(CompanionError::PayoutTarget);
            }
            Some(dest)
        } else {
            None
        };
        entries.push(LegPayoutEntry {
            holding: info,
            owner: h.owner,
            leg,
            to,
            amount: e.amount,
            tag: e.tag,
        });
    }
    // 4. The payments, from each leg's account, the creator address signing.
    let creator = find(p.all, &p.creator)?;
    for q in &entries {
        if let Some(to) = q.to {
            let pot = pots.legs.legs[q.leg];
            LegToken::of(p.all, &pot, &p.creator)?.pay(creator, p.seeds, to, q.amount)?;
        }
    }
    // 5. The records, the hook writing each one with the payout authority signing, and the
    // accounting.
    let state = bordrless_game::state_address(&hook, &mint).0;
    let hook_authority = bordrless_hook::hook_authority(&hook).0;
    let token_event_authority = bordrless_token::EVENT_AUTHORITY_AND_BUMP.0;
    let fixed = [
        find(p.all, &PAYOUT_AUTHORITY)?.clone(),
        find(p.all, &mint)?.clone(),
        find(p.all, &state)?.clone(),
        find(p.all, &hook_authority)?.clone(),
        find(p.all, &TOKEN_ID)?.clone(),
        find(p.all, &token_event_authority)?.clone(),
        find(p.all, &hook)?.clone(),
    ];
    let mut record = Instruction {
        program_id: hook,
        accounts: vec![
            AccountMeta::new_readonly(PAYOUT_AUTHORITY, true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(Pubkey::default(), false),
            AccountMeta::new(state, false),
            AccountMeta::new_readonly(hook_authority, false),
            AccountMeta::new_readonly(TOKEN_ID, false),
            AccountMeta::new_readonly(token_event_authority, false),
        ],
        data: vec![0u8; 8 + 8 + 4],
    };
    record.data[..8].copy_from_slice(&RECORD_PAYOUT_DISCRIMINATOR);
    let mut event = Instruction {
        program_id: crate::ID,
        accounts: vec![AccountMeta::new_readonly(*p.event_authority.key, true)],
        data: Vec::with_capacity(8 + 8 + 3 * 32 + 1 + 8 + 4),
    };
    for q in &entries {
        record.accounts[2].pubkey = *q.holding.key;
        record.data[8..16].copy_from_slice(&q.amount.to_le_bytes());
        record.data[16..20].copy_from_slice(&q.tag.to_le_bytes());
        let infos = [
            fixed[0].clone(),
            fixed[1].clone(),
            q.holding.clone(),
            fixed[2].clone(),
            fixed[3].clone(),
            fixed[4].clone(),
            fixed[5].clone(),
            fixed[6].clone(),
        ];
        invoke_signed(
            &record,
            &infos,
            &[&[PAYOUT_AUTHORITY_SEED, &[PAYOUT_AUTHORITY_BUMP]]],
        )?;
        let leg = &mut pots.legs.legs[q.leg];
        leg.pending_pot = leg
            .pending_pot
            .checked_sub(q.amount)
            .ok_or(CompanionError::MathOverflow)?;
        leg.prize_paid = leg.prize_paid.saturating_add(q.amount);
        t.payouts = t.payouts.saturating_add(1);
        event.data.clear();
        event
            .data
            .extend_from_slice(anchor_lang::event::EVENT_IX_TAG_LE);
        event.data.extend_from_slice(LegScriptPayout::DISCRIMINATOR);
        LegScriptPayout {
            companion: p.companion_key,
            index: q.leg as u8,
            holding: *q.holding.key,
            owner: q.owner,
            amount: q.amount,
            tag: q.tag,
        }
        .serialize(&mut event.data)?;
        invoke_signed(
            &event,
            core::slice::from_ref(p.event_authority),
            &[&[b"__event_authority", &[crate::EVENT_AUTHORITY_AND_BUMP.1]]],
        )?;
    }
    t.last_paid_at = p.now;
    let mut data = terms_info.try_borrow_mut_data()?;
    let mut writer: &mut [u8] = &mut data;
    t.try_serialize(&mut writer)?;
    Ok(())
}
