//! Payouts (Loss cover, 2026-10-12; `docs/companions.md`, "Payouts"): a script names holdings in
//! its return data and the companion pays their owners from the pot, within the script's terms,
//! once the launch's hook has recorded each one. The script names, the companion pays, as the
//! token program's deltas and the strategies' `entitle` already work.
//!
//! - **The answer.** After `script_step`'s call returns, the return data is read: none, empty, or
//!   set by another program (a trade's hooks leave theirs) is no answer, today's step. The
//!   script's own must decode as a [`PayoutAnswer`] (`version` 1, at most `MAX_PAYOUTS_PER_STEP`
//!   entries): `BadPayoutAnswer` otherwise, and the step fails whole. Each entry names a holding by
//!   its index among the step's remaining accounts, lamports (0: a touch, recorded and not paid)
//!   and a tag the hook gets untouched.
//! - **The terms** (`PayoutTerms`, made by `create_script_v2`; a script made with `create_script`
//!   has none: `PayoutsOff`) bound every step: a share of the pot per step, a share of the step
//!   per holding, a share of the pot per window. The launch's mint must have a custom hook that
//!   writes hook data (`PayoutsNeedHook`): a kit token or a hook without hook data can carry no
//!   record, so it can't be paid.
//! - **The checks, in order**, all before any payment: the budget (`step_budget`), then each
//!   entry: the index, a holding of the mint, named once, within the holding's cap and the
//!   budget (`PayoutOverBound`), its owner eligible (never the launch, its pool, the creator
//!   address, the companion, the script, its state, the terms, the hook, the default key or an
//!   address off the curve: `PayoutTarget`) and not the launcher (`PayoutToBeneficiary`); with
//!   lamports, the owner's account among the remaining accounts, writable, payable and not left
//!   below its rent-exempt minimum (`PayoutTarget`). A touch needs no owner account.
//! - **The payments**: one `unwrap_sol` of the total (the bounty with it), a transfer per paid
//!   entry. **The records**: a CPI per entry into the hook, `record_payout(lamports, tag)`, signed
//!   by `PDA(["payout-authority"])`, a program-wide address that signs nothing else; the hook
//!   writes its record through the token program's `write_hook_data`. A hook that can't record an
//!   entry fails the step. **The accounting**: the pot, the window's and the terms' counters, one
//!   `ScriptPayout` per entry.
//! - **The bounty** of a step with an answer of `n` entries is `SCRIPT_BOUNTY * min(n, 8) / 8`: an
//!   empty answer pays nothing, a full step the bounty. A step with no answer keeps the flat one.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::{get_return_data, invoke_signed, set_return_data};
use anchor_lang::system_program;
use anchor_lang::Discriminator;
use bordrless_game::eligible;
use bordrless_hook::token_flags;
use bordrless_launch::state::Launch;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::{ScriptPayout, ScriptWalletPayout};
use crate::instructions::steps::find;
use crate::instructions::CreatorSeeds;
use crate::invoke::invoke_built;
use crate::state::*;

/// The terms of `create_script_v2`: each share 1 to 10,000 basis points, the window 10 seconds to
/// a day.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PayoutTermsArgs {
    /// The most one step pays, of the pot at the step's start.
    pub step_bps: u16,
    /// The most one holding gets of a step's budget.
    pub holding_bps: u16,
    /// The most a window pays, of the pot as if the window had paid nothing (`window_base`).
    pub window_bps: u16,
    /// The window: `floor(now / window_secs)`.
    pub window_secs: u32,
}

impl PayoutTermsArgs {
    /// Whether the terms are within their bounds.
    pub fn valid(&self) -> bool {
        let share = 1..=BPS as u16;
        share.contains(&self.step_bps)
            && share.contains(&self.holding_bps)
            && share.contains(&self.window_bps)
            && (MIN_PAYOUT_WINDOW_SECS..=MAX_PAYOUT_WINDOW_SECS).contains(&self.window_secs)
    }
}

/// Clears the return data before a script is called, so only what the script itself sets is read.
pub(crate) fn clear_answer() {
    set_return_data(&[]);
}

/// The script's answer, read from the return data after it ran (and cleared): `None` without one
/// (no return data, empty, or data another program set: a trade's hooks leave theirs behind, so an
/// old script that trades answers nothing); the script's own decoded, or `BadPayoutAnswer`.
pub(crate) fn read_answer(script: &Pubkey) -> Result<Option<ScriptAnswer>> {
    let answer = match get_return_data() {
        Some((program, bytes)) if program == *script && !bytes.is_empty() => {
            Some(decode_script_answer(&bytes)?)
        }
        _ => None,
    };
    set_return_data(&[]);
    Ok(answer)
}

/// A script's answer: holdings to pay (version 1), wallets (version 2), or holdings to pay in
/// the legs' tokens (version 3, a coin quoted in tokens whose parts are kept per leg).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptAnswer {
    Holdings(PayoutAnswer),
    Wallets(WalletPayoutAnswer),
    LegHoldings(PayoutAnswerLegs),
}

impl ScriptAnswer {
    /// Its number of entries.
    pub fn len(&self) -> usize {
        match self {
            ScriptAnswer::Holdings(a) => a.entries.len(),
            ScriptAnswer::Wallets(a) => a.entries.len(),
            ScriptAnswer::LegHoldings(a) => a.entries.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `bytes` as a script's answer by its first byte, the version: a [`PayoutAnswer`] (1), a
/// [`WalletPayoutAnswer`] (2) or a [`PayoutAnswerLegs`] (3, `docs/token-quotes.md` §7.6), exactly
/// its Borsh, at most `MAX_PAYOUTS_PER_STEP` entries; `BadPayoutAnswer` otherwise.
pub fn decode_script_answer(bytes: &[u8]) -> Result<ScriptAnswer> {
    match bytes.first() {
        Some(&PAYOUT_ANSWER_WALLETS_VERSION) => {
            let answer = WalletPayoutAnswer::try_from_slice(bytes)
                .map_err(|_| error!(CompanionError::BadPayoutAnswer))?;
            require!(
                answer.entries.len() <= MAX_PAYOUTS_PER_STEP,
                CompanionError::BadPayoutAnswer
            );
            Ok(ScriptAnswer::Wallets(answer))
        }
        Some(&PAYOUT_ANSWER_LEGS_VERSION) => {
            let answer = PayoutAnswerLegs::try_from_slice(bytes)
                .map_err(|_| error!(CompanionError::BadPayoutAnswer))?;
            require!(
                answer.entries.len() <= MAX_PAYOUTS_PER_STEP,
                CompanionError::BadPayoutAnswer
            );
            Ok(ScriptAnswer::LegHoldings(answer))
        }
        _ => Ok(ScriptAnswer::Holdings(decode_answer(bytes)?)),
    }
}

/// `bytes` as a [`PayoutAnswer`]: exactly its Borsh, version `PAYOUT_ANSWER_VERSION`, at most
/// `MAX_PAYOUTS_PER_STEP` entries; `BadPayoutAnswer` otherwise.
pub fn decode_answer(bytes: &[u8]) -> Result<PayoutAnswer> {
    let answer =
        PayoutAnswer::try_from_slice(bytes).map_err(|_| error!(CompanionError::BadPayoutAnswer))?;
    require!(
        answer.version == PAYOUT_ANSWER_VERSION && answer.entries.len() <= MAX_PAYOUTS_PER_STEP,
        CompanionError::BadPayoutAnswer
    );
    Ok(answer)
}

/// What a step earns its sender: the flat `SCRIPT_BOUNTY` without an answer; with an answer of
/// `n` entries, `SCRIPT_BOUNTY * min(n, PAYOUT_BOUNTY_FULL) / PAYOUT_BOUNTY_FULL`, so an empty
/// answer pays nothing and a thin step less than its fee.
pub fn bounty_for(entries: Option<usize>) -> u64 {
    match entries {
        None => SCRIPT_BOUNTY,
        Some(n) => SCRIPT_BOUNTY * (n as u64).min(PAYOUT_BOUNTY_FULL) / PAYOUT_BOUNTY_FULL,
    }
}

/// The most a step may pay in all: `step_bps` of the pot at the step's start, what the window
/// may still pay, and the pot after the script ran less the bounty. The holding cap is
/// `holding_bps` of it. A script computes the same from the same numbers, since an answer is
/// refused, never clamped.
///
/// The window's share (2026-10-15) is `window_bps` of [`window_base`]: the live pot as if the
/// window had paid nothing (`pot_after_script + window_paid`), never less than the pot at the
/// window's first step (`window_pot`), less what the window paid. Paying moves lamports from the
/// pot to `window_paid` one for one, so payouts never shrink the window's cap; fees arriving
/// mid-window grow it. Never less than the old cap, so a script that computes the frozen one
/// answers within this one. Whatever the window's share, the pot less the bounty bounds the step.
pub fn step_budget(
    pot_before: u64,
    pot_after_script: u64,
    bounty: u64,
    window_pot: u64,
    window_paid: u64,
    step_bps: u16,
    window_bps: u16,
) -> u64 {
    bps_of(pot_before, u64::from(step_bps))
        .min(
            bps_of(
                window_base(pot_after_script, window_pot, window_paid),
                u64::from(window_bps),
            )
            .saturating_sub(window_paid),
        )
        .min(pot_after_script.saturating_sub(bounty))
}

/// The pot a window's share is measured against: the pot now (after the script ran) with what the
/// window has paid added back, i.e. the pot as if the window had paid nothing; at least the pot at
/// the window's first step, so a window's cap never falls below the frozen one it replaced.
pub fn window_base(pot_after_script: u64, window_pot: u64, window_paid: u64) -> u64 {
    pot_after_script.saturating_add(window_paid).max(window_pot)
}

/// Whether SOL can be paid to `account` (as `pay_strategy`'s `payable`): not executable, not a
/// sysvar, none of the runtime's reserved keys.
pub(crate) fn payable(account: &AccountInfo) -> bool {
    !account.executable
        && *account.owner != SYSVAR_PROGRAM_ID
        && !RESERVED_KEYS.contains(account.key)
}

/// The script's payout terms among the step's accounts (writable, the companion's, of the mint):
/// their address, their account and their counters; `PayoutsOff` without them.
pub(crate) fn load_terms<'a, 'info>(
    p: &StepPayouts<'a, 'info>,
) -> Result<(Pubkey, &'a AccountInfo<'info>, PayoutTerms)> {
    let mint = p.companion.mint;
    let terms_key = PayoutTerms::address(&mint).0;
    let terms_info = p
        .all
        .iter()
        .find(|a| *a.key == terms_key)
        .ok_or(CompanionError::PayoutsOff)?;
    require!(*terms_info.owner == crate::ID, CompanionError::PayoutsOff);
    require!(terms_info.is_writable, CompanionError::MissingAccount);
    let t = {
        let data = terms_info.try_borrow_data()?;
        PayoutTerms::try_deserialize(&mut &data[..])
            .map_err(|_| error!(CompanionError::PayoutsOff))?
    };
    require!(t.mint == mint, CompanionError::PayoutsOff);
    Ok((terms_key, terms_info, t))
}

/// The step's budget under `t` (its window rolled at its first step), and each entry's cap.
fn budget_of(p: &StepPayouts, t: &mut PayoutTerms) -> (u64, u64) {
    let window_start = t.window_start_at(p.now);
    if t.window_start != window_start {
        t.window_start = window_start;
        t.window_pot = p.pot_after_script;
        t.window_paid = 0;
    }
    let budget = step_budget(
        p.pot_before,
        p.pot_after_script,
        p.bounty,
        t.window_pot,
        t.window_paid,
        t.step_bps,
        t.window_bps,
    );
    (budget, bps_of(budget, u64::from(t.holding_bps)))
}

/// Accounts of `set_payout_terms`.
#[event_cpi]
#[derive(Accounts)]
pub struct SetPayoutTerms<'info> {
    /// The launch's beneficiary (`Companion.beneficiary`, the wallet that launched it: not the
    /// companion's creator PDA) or the script approver (`SCRIPT_APPROVER`).
    pub authority: Signer<'info>,
    #[account(seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    #[account(mut, seeds = [PAYOUT_TERMS_SEED, companion.mint.as_ref()], bump = terms.bump)]
    pub terms: Box<Account<'info, PayoutTerms>>,
}

/// `set_payout_terms` (2026-10-15): replaces a script's payout terms, before or after the launch,
/// within `create_script_v2`'s bounds (`BadPayoutTerms`), signed by the launch's beneficiary or the
/// script approver (`NotTermsAuthority`). The terms only bound how fast the script pays from the
/// pot: they name no recipient, and the wallet-payouts flag, the counters of what was paid and the
/// mint are kept. The window restarts: its counters clear, and the next step opens a new window at
/// the pot as it stands, under the new terms. `PayoutTermsSet`.
pub fn process_set_payout_terms(
    ctx: Context<SetPayoutTerms>,
    payout: PayoutTermsArgs,
) -> Result<()> {
    let signer = ctx.accounts.authority.key();
    require!(
        signer == ctx.accounts.companion.beneficiary || signer == SCRIPT_APPROVER,
        CompanionError::NotTermsAuthority
    );
    require!(payout.valid(), CompanionError::BadPayoutTerms);
    let mint = ctx.accounts.companion.mint;
    let t = &mut ctx.accounts.terms;
    require!(t.mint == mint, CompanionError::PayoutsOff);
    t.step_bps = payout.step_bps;
    t.holding_bps = payout.holding_bps;
    t.window_bps = payout.window_bps;
    t.window_secs = payout.window_secs;
    t.window_start = 0;
    t.window_pot = 0;
    t.window_paid = 0;
    let terms = t.key();
    emit_cpi!(crate::events::PayoutTermsSet {
        companion: ctx.accounts.companion.key(),
        mint,
        terms,
        step_bps: payout.step_bps,
        holding_bps: payout.holding_bps,
        window_bps: payout.window_bps,
        window_secs: payout.window_secs,
    });
    Ok(())
}

/// Accounts of `allow_wallet_payouts`.
#[event_cpi]
#[derive(Accounts)]
pub struct AllowWalletPayouts<'info> {
    /// The launch's mint, the keypair that made the companion: only its holder allows it.
    pub mint: Signer<'info>,
    #[account(seeds = [COMPANION_SEED, mint.key().as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    #[account(mut, seeds = [PAYOUT_TERMS_SEED, mint.key().as_ref()], bump = terms.bump)]
    pub terms: Box<Account<'info, PayoutTerms>>,
    #[account(seeds = [SCRIPT_SEED, mint.key().as_ref()], bump = script.bump)]
    pub script: Box<Account<'info, Script>>,
}

/// `allow_wallet_payouts`: before the launch, the launcher lets its script pay wallets as well as
/// holders (wallet answers, version 2), within the same terms. Once, for good: a buyer reads it on
/// chain before the first trade.
pub fn process_allow_wallet_payouts(ctx: Context<AllowWalletPayouts>) -> Result<()> {
    require!(
        !ctx.accounts.companion.launched,
        CompanionError::AlreadyLaunched
    );
    require!(
        ctx.accounts.companion.is_script(),
        CompanionError::NotAScript
    );
    ctx.accounts.terms.reserved[0] = WALLET_PAYOUTS_ALLOWED;
    emit_cpi!(crate::events::WalletPayoutsAllowed {
        companion: ctx.accounts.companion.key(),
        program: ctx.accounts.script.program,
    });
    Ok(())
}

/// The programs whose accounts a wallet payout never pays: an account of one of them is the
/// protocol's (a holding, a vault, a pool, a launch, the companion's own records), and lamports
/// sent there are stranded at best.
const PROTOCOL_OWNERS: [Pubkey; 6] = [
    crate::ID,
    TOKEN_ID,
    BRIDGE_ID,
    SWAP_ID,
    LAUNCH_ID,
    BPF_LOADER_UPGRADEABLE_ID,
];

/// Pays a wallet answer (`PAYOUT_ANSWER_WALLETS_VERSION`, 2026-10-13), for a script whose launcher
/// allowed it (`allow_wallet_payouts`, else `WalletPayoutsOff`). Each entry names the account paid
/// by its key, which must be among the step's remaining accounts, writable, able to take SOL (not
/// executable, not a sysvar, no reserved key), left at its rent-exempt minimum at least, owned by
/// none of the protocol's programs, and none of the launch, its pool, the creator address, the
/// companion, the script, its state, the terms, the hook, the mint, the payout authority, this
/// program's event authority or the default key (`WalletPayoutTarget`); named once, above zero
/// (`BadPayoutAnswer`); within the terms as a holding payout is (`holding_bps` of the step's budget
/// each, the step's and the window's budget in all: `PayoutOverBound`). All checked before any
/// payment; then one `unwrap_sol` of the total and the bounty, a transfer each, the counters and
/// one `ScriptWalletPayout` per entry. No hook records it: there is no holding.
fn pay_wallets<'info>(p: &StepPayouts<'_, 'info>, answer: &WalletPayoutAnswer) -> Result<u64> {
    let mint = p.companion.mint;
    let (terms_key, terms_info, mut t) = load_terms(p)?;
    require!(t.wallets_allowed(), CompanionError::WalletPayoutsOff);
    let (budget, cap) = budget_of(p, &mut t);
    let hook = p.launch.custom_hook.unwrap_or_default();
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
        mint,
        PAYOUT_AUTHORITY,
        crate::EVENT_AUTHORITY_AND_BUMP.0,
        // The bridge's SOL vault: system-owned, so no owner rule catches it; SOL sent there would
        // back no bridged SOL.
        bordrless_bridge::client::sol_vault_address(),
        Pubkey::default(),
    ];
    let rent = Rent::get()?;
    // Each payout, and whether it goes in bridged SOL: an account that is a holding of bridged SOL
    // (an old-style destination, a holder's or a keeper's) is paid in it, never lamports (which
    // would sit on the holding, stranded).
    let mut payouts: Vec<(&AccountInfo<'info>, u64, u32)> =
        Vec::with_capacity(answer.entries.len());
    let mut wrapped: Vec<bool> = Vec::with_capacity(answer.entries.len());
    let mut total = 0u64;
    for (i, e) in answer.entries.iter().enumerate() {
        if e.lamports == 0 || payouts.iter().any(|(q, _, _)| *q.key == e.to) {
            msg!("wallet payout {}: zero, or an account named twice", i);
            return err!(CompanionError::BadPayoutAnswer);
        }
        let sum = total.checked_add(e.lamports);
        if e.lamports > cap || sum.is_none_or(|s| s > budget) {
            msg!(
                "wallet payout {}: {} over the bound (cap {}, budget {}, paid before it {})",
                i,
                e.lamports,
                cap,
                budget,
                total
            );
            return err!(CompanionError::PayoutOverBound);
        }
        total = sum.unwrap_or(total);
        let Some(to) = p.remaining.iter().find(|a| *a.key == e.to) else {
            msg!("wallet payout {}: {} was not passed", i, e.to);
            return err!(CompanionError::WalletPayoutTarget);
        };
        // A holding of bridged SOL is paid in it only when its owner could be paid as a wallet:
        // not one of the accounts above, passed among the remaining accounts, and owned by none of
        // the protocol's programs (so never the creator's, the launch's, a vault's or a pool's).
        let bridged_holding = *to.owner == TOKEN_ID
            && token_client::read_holding(to).is_ok_and(|h| {
                h.mint == BRIDGED_SOL_MINT
                    && !excluded.contains(&h.owner)
                    && p.remaining.iter().any(|o| {
                        *o.key == h.owner && !PROTOCOL_OWNERS.contains(o.owner) && payable(o)
                    })
            });
        let funded = to
            .lamports()
            .checked_add(e.lamports)
            .is_some_and(|l| l >= rent.minimum_balance(to.data_len()));
        if excluded.contains(to.key)
            || !to.is_writable
            || (!bridged_holding && (PROTOCOL_OWNERS.contains(to.owner) || !payable(to) || !funded))
        {
            msg!("wallet payout {}: account {} can't be paid", i, to.key);
            return err!(CompanionError::WalletPayoutTarget);
        }
        payouts.push((to, e.lamports, e.tag));
        wrapped.push(bridged_holding);
    }
    // What leaves the pot: every payout and the bounty. Only the part paid as SOL is unwrapped
    // (a bridged companion); the part paid in bridged SOL moves as it is, or (a native companion)
    // is wrapped by the creator address into the holding.
    let unwrapped = total
        .checked_add(p.bounty)
        .ok_or(CompanionError::MathOverflow)?;
    let in_bridged: u64 = payouts
        .iter()
        .zip(&wrapped)
        .filter(|(_, w)| **w)
        .map(|((_, l, _), _)| *l)
        .sum();
    let as_sol = unwrapped - in_bridged;
    if as_sol > 0 && !p.seeds.native {
        invoke_built(
            &bordrless_bridge::client::unwrap_sol(p.creator, as_sol),
            p.all,
            &[&p.seeds.seeds()],
        )?;
    }
    let from = find(p.all, &p.creator)?.clone();
    let system = find(p.all, &system_program::ID)?.clone();
    let send = |to: &AccountInfo<'info>, lamports: u64| -> Result<()> {
        system_program::transfer(
            CpiContext::new_with_signer(
                *system.key,
                system_program::Transfer {
                    from: from.clone(),
                    to: to.clone(),
                },
                &[&p.seeds.seeds()],
            ),
            lamports,
        )
    };
    for ((to, lamports, _), w) in payouts.iter().zip(&wrapped) {
        if !*w {
            send(to, *lamports)?;
        } else if p.seeds.native {
            // The bridge's `wrap_sol` with the creator address its user, minting into the
            // holding (the bridge mints into any holding of bridged SOL it is given).
            let mut ix = bordrless_bridge::client::wrap_sol(p.creator, *lamports);
            ix.accounts[WRAP_SOL_HOLDING_INDEX] = AccountMeta::new(*to.key, false);
            invoke_built(&ix, p.all, &[&p.seeds.seeds()])?;
        } else {
            invoke_built(
                &token_client::transfer(
                    p.creator,
                    token_client::holding_address(&BRIDGED_SOL_MINT, &p.creator),
                    *to.key,
                    BRIDGED_SOL_MINT,
                    None,
                    vec![],
                    *lamports,
                ),
                p.all,
                &[&p.seeds.seeds()],
            )?;
        }
    }
    if p.bounty > 0 {
        send(p.cranker, p.bounty)?;
    }
    let pot_after = p
        .pot_after_script
        .checked_sub(unwrapped)
        .ok_or(CompanionError::MathOverflow)?;
    let mut event = Instruction {
        program_id: crate::ID,
        accounts: vec![AccountMeta::new_readonly(*p.event_authority.key, true)],
        data: Vec::with_capacity(8 + 8 + 3 * 32 + 8 + 4 + 3 * 8),
    };
    for (to, lamports, tag) in &payouts {
        t.window_paid = t
            .window_paid
            .checked_add(*lamports)
            .ok_or(CompanionError::MathOverflow)?;
        t.paid_total = t.paid_total.saturating_add(*lamports);
        t.payouts = t.payouts.saturating_add(1);
        event.data.clear();
        event
            .data
            .extend_from_slice(anchor_lang::event::EVENT_IX_TAG_LE);
        event
            .data
            .extend_from_slice(ScriptWalletPayout::DISCRIMINATOR);
        ScriptWalletPayout {
            companion: p.companion_key,
            program: p.script.program,
            to: *to.key,
            lamports: *lamports,
            tag: *tag,
            window_paid: t.window_paid,
            paid_total: t.paid_total,
            pot_after,
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
    Ok(total)
}

/// The index of the user's holding of bridged SOL in the bridge's `wrap_sol` accounts.
const WRAP_SOL_HOLDING_INDEX: usize = 5;

/// One checked entry: the holding, its owner, the owner's account when paid, what it gets, the
/// tag.
struct Payout<'a, 'info> {
    holding: &'a AccountInfo<'info>,
    owner: Pubkey,
    to: Option<AccountInfo<'info>>,
    lamports: u64,
    tag: u32,
}

/// What a step with an answer pays with.
pub(crate) struct StepPayouts<'a, 'info> {
    /// Every account of the step (the named ones, then the remaining).
    pub all: &'a [AccountInfo<'info>],
    /// The remaining accounts, which the answer's indices name.
    pub remaining: &'a [AccountInfo<'info>],
    pub companion: &'a Companion,
    pub companion_key: Pubkey,
    pub launch: &'a Launch,
    pub launch_key: Pubkey,
    pub script: &'a Script,
    pub script_key: Pubkey,
    pub script_state_key: Pubkey,
    pub creator: Pubkey,
    pub seeds: &'a CreatorSeeds,
    pub cranker: &'a AccountInfo<'info>,
    /// This program's event authority, which signs each `ScriptPayout`.
    pub event_authority: &'a AccountInfo<'info>,
    /// The pot at the step's start, and as the step computed it after the script ran.
    pub pot_before: u64,
    pub pot_after_script: u64,
    /// The sender's bounty (paid here, with the payouts' unwrap).
    pub bounty: u64,
    pub now: i64,
}

/// Pays and records `answer` (one entry at least): the checks in the module's order, every one
/// before any payment; then the payments, then each entry's record and event. Answers what was
/// paid in all, the terms written. The record's and the event's instructions are built once and
/// reused for every entry: the program's heap (32 KiB, never freed) can't afford one of each per
/// entry.
pub(crate) fn pay_answer<'info>(p: &StepPayouts<'_, 'info>, answer: &ScriptAnswer) -> Result<u64> {
    match answer {
        ScriptAnswer::Holdings(a) => pay_holdings(p, a),
        ScriptAnswer::Wallets(a) => pay_wallets(p, a),
        // Paid by `script_step` from the legs' pots (`token_scripts::pay_leg_holdings`): never here.
        ScriptAnswer::LegHoldings(_) => err!(CompanionError::BadPayoutAnswer),
    }
}

/// Pays and records a holdings answer (version 1); see [`pay_answer`].
fn pay_holdings<'info>(p: &StepPayouts<'_, 'info>, answer: &PayoutAnswer) -> Result<u64> {
    let mint = p.companion.mint;
    // 1. The terms and the hook.
    let (terms_key, terms_info, mut t) = load_terms(p)?;
    let hook = p
        .launch
        .custom_hook
        .filter(|_| p.launch.custom_hook_flags & token_flags::WRITES_HOOK_DATA != 0)
        .ok_or(CompanionError::PayoutsNeedHook)?;
    // 2. The budget: the window's counters roll at its first step.
    let (budget, holding_cap) = budget_of(p, &mut t);
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
    let rent_min = Rent::get()?.minimum_balance(0);
    let mut payouts: Vec<Payout<'_, 'info>> = Vec::with_capacity(answer.entries.len());
    let mut total = 0u64;
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
        if payouts.iter().any(|q| q.holding.key == info.key) {
            msg!("payout {}: holding named twice", i);
            return err!(CompanionError::BadPayoutAnswer);
        }
        let sum = total.checked_add(e.lamports);
        if e.lamports > holding_cap || sum.is_none() || sum.is_some_and(|s| s > budget) {
            msg!(
                "payout {}: {} over the bound (holding cap {}, budget {}, paid before it {})",
                i,
                e.lamports,
                holding_cap,
                budget,
                total
            );
            return err!(CompanionError::PayoutOverBound);
        }
        total = sum.unwrap_or(total);
        if !eligible(&h.owner, &excluded) {
            msg!("payout {}: the owner can't be paid", i);
            return err!(CompanionError::PayoutTarget);
        }
        require!(
            h.owner != p.companion.beneficiary,
            CompanionError::PayoutToBeneficiary
        );
        let to = if e.lamports > 0 {
            let Some(owner) = p.remaining.iter().find(|a| *a.key == h.owner) else {
                msg!("payout {}: the owner's account was not passed", i);
                return err!(CompanionError::PayoutTarget);
            };
            let funded = owner
                .lamports()
                .checked_add(e.lamports)
                .is_some_and(|l| l >= rent_min);
            if !owner.is_writable || !payable(owner) || !funded {
                msg!("payout {}: the owner's account can't take it", i);
                return err!(CompanionError::PayoutTarget);
            }
            Some(owner.clone())
        } else {
            None
        };
        payouts.push(Payout {
            holding: info,
            owner: h.owner,
            to,
            lamports: e.lamports,
            tag: e.tag,
        });
    }
    // 4, 5. The payments: one unwrap of the total and the bounty, then a transfer each.
    let unwrapped = total
        .checked_add(p.bounty)
        .ok_or(CompanionError::MathOverflow)?;
    if unwrapped > 0 && !p.seeds.native {
        invoke_built(
            &bordrless_bridge::client::unwrap_sol(p.creator, unwrapped),
            p.all,
            &[&p.seeds.seeds()],
        )?;
    }
    let from = find(p.all, &p.creator)?.clone();
    let system = find(p.all, &system_program::ID)?.clone();
    let send = |to: &AccountInfo<'info>, lamports: u64| -> Result<()> {
        system_program::transfer(
            CpiContext::new_with_signer(
                *system.key,
                system_program::Transfer {
                    from: from.clone(),
                    to: to.clone(),
                },
                &[&p.seeds.seeds()],
            ),
            lamports,
        )
    };
    for q in &payouts {
        if let Some(to) = &q.to {
            send(to, q.lamports)?;
        }
    }
    if p.bounty > 0 {
        send(p.cranker, p.bounty)?;
    }
    // 6, 7. The records, the hook writing each one with the payout authority signing, and the
    // accounting: the window's and the terms' counters, one event per entry. The record's
    // accounts are found once; the holding alone changes from one entry to the next.
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
        data: Vec::with_capacity(8 + 8 + 4 * 32 + 8 + 4 + 3 * 8),
    };
    let pot_after = p
        .pot_after_script
        .checked_sub(unwrapped)
        .ok_or(CompanionError::MathOverflow)?;
    for q in &payouts {
        record.accounts[2].pubkey = *q.holding.key;
        record.data[8..16].copy_from_slice(&q.lamports.to_le_bytes());
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
        t.window_paid = t
            .window_paid
            .checked_add(q.lamports)
            .ok_or(CompanionError::MathOverflow)?;
        t.paid_total = t.paid_total.saturating_add(q.lamports);
        t.payouts = t.payouts.saturating_add(1);
        event.data.clear();
        event
            .data
            .extend_from_slice(anchor_lang::event::EVENT_IX_TAG_LE);
        event.data.extend_from_slice(ScriptPayout::DISCRIMINATOR);
        ScriptPayout {
            companion: p.companion_key,
            program: p.script.program,
            holding: *q.holding.key,
            owner: q.owner,
            lamports: q.lamports,
            tag: q.tag,
            window_paid: t.window_paid,
            paid_total: t.paid_total,
            pot_after,
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
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(entries: Vec<PayoutEntry>) -> Vec<u8> {
        let mut out = Vec::new();
        PayoutAnswer {
            version: PAYOUT_ANSWER_VERSION,
            entries,
        }
        .serialize(&mut out)
        .unwrap();
        out
    }

    fn entry(account: u8, lamports: u64, tag: u32) -> PayoutEntry {
        PayoutEntry {
            account,
            lamports,
            tag,
        }
    }

    #[test]
    fn an_answer_is_its_borsh_exactly() {
        // 1 + 4 + 13 * n bytes; 16 entries at most: 213 bytes.
        assert_eq!(answer(vec![]).len(), 5);
        let sixteen = answer(vec![entry(1, 2, 3); 16]);
        assert_eq!(sixteen.len(), 213);
        assert_eq!(decode_answer(&sixteen).unwrap().entries.len(), 16);
        let one = answer(vec![entry(7, u64::MAX, u32::MAX)]);
        assert_eq!(one.len(), 18);
        assert_eq!(
            decode_answer(&one).unwrap().entries,
            vec![entry(7, u64::MAX, u32::MAX)]
        );
        // Over 16, a bad version, a byte short or over, garbage: refused.
        assert!(decode_answer(&answer(vec![entry(1, 2, 3); 17])).is_err());
        let mut bad = one.clone();
        bad[0] = 2;
        assert!(decode_answer(&bad).is_err());
        // Version 2 is a wallet answer (its own layout: the payee's key, lamports, tag); 3 is none.
        let mut w = Vec::new();
        WalletPayoutAnswer {
            version: PAYOUT_ANSWER_WALLETS_VERSION,
            entries: vec![
                WalletPayoutEntry {
                    to: Pubkey::new_unique(),
                    lamports: 5,
                    tag: 1
                };
                16
            ],
        }
        .serialize(&mut w)
        .unwrap();
        assert_eq!(w.len(), 1 + 4 + 44 * 16);
        assert!(
            matches!(decode_script_answer(&w), Ok(ScriptAnswer::Wallets(a)) if a.entries.len() == 16)
        );
        assert!(matches!(
            decode_script_answer(&one),
            Ok(ScriptAnswer::Holdings(_))
        ));
        let mut over = w.clone();
        over[1] = 17;
        assert!(decode_script_answer(&over).is_err());
        bad[0] = 3;
        assert!(decode_script_answer(&bad).is_err());
        assert!(decode_answer(&one[..one.len() - 1]).is_err());
        let mut longer = one.clone();
        longer.push(0);
        assert!(decode_answer(&longer).is_err());
        assert!(decode_answer(&[9; 3]).is_err());
        assert!(decode_answer(&[]).is_err());
    }

    #[test]
    fn the_bounty_follows_the_entries() {
        assert_eq!(bounty_for(None), SCRIPT_BOUNTY);
        assert_eq!(bounty_for(Some(0)), 0);
        assert_eq!(bounty_for(Some(1)), SCRIPT_BOUNTY / 8);
        assert_eq!(bounty_for(Some(4)), SCRIPT_BOUNTY / 2);
        assert_eq!(bounty_for(Some(8)), SCRIPT_BOUNTY);
        assert_eq!(bounty_for(Some(16)), SCRIPT_BOUNTY);
        const { assert!(SCRIPT_BOUNTY.is_multiple_of(PAYOUT_BOUNTY_FULL)) };
    }

    #[test]
    fn the_budget_is_the_least_of_three_bounds() {
        // Every cap at 100%: the pot after the script, less the bounty, and never more than the
        // window held at its start.
        assert_eq!(step_budget(1_000, 1_000, 20, 1_000, 0, 10_000, 10_000), 980);
        // The pot at the window's start is a floor: a pot that fell below it (bounties, the
        // script's spending) keeps the frozen cap, never less than before.
        assert_eq!(step_budget(1_000, 1_000, 0, 4_000, 100, 10_000, 2_500), 900);
        // The step's share of the pot before.
        assert_eq!(step_budget(1_000, 5_000, 0, 5_000, 0, 1_000, 10_000), 100);
        // The window's share of the pot as if it had paid nothing, less what it paid: 25% of
        // (750 + 250) - 250 = 0; then fees of 1,000 arrive: 25% of 2,000 - 250 = 250.
        assert_eq!(step_budget(750, 750, 0, 1_000, 250, 10_000, 2_500), 0);
        assert_eq!(step_budget(1_750, 1_750, 0, 1_000, 250, 10_000, 2_500), 250);
        // Never more than the pot less the bounty, whatever the window paid.
        assert_eq!(
            step_budget(1_000, 1_000, 0, 1_000, 2_000, 10_000, 10_000),
            1_000
        );
        assert_eq!(step_budget(1_000, 1_000, 20, 600, 100, 10_000, 10_000), 980);
        // A pot below the bounty: nothing.
        assert_eq!(step_budget(10, 10, 20, 10, 0, 10_000, 10_000), 0);
        let terms = PayoutTermsArgs {
            step_bps: 10_000,
            holding_bps: 10_000,
            window_bps: 10_000,
            window_secs: 300,
        };
        assert!(terms.valid());
        for bad in [
            PayoutTermsArgs {
                step_bps: 0,
                ..terms
            },
            PayoutTermsArgs {
                holding_bps: 10_001,
                ..terms
            },
            PayoutTermsArgs {
                window_bps: 0,
                ..terms
            },
            PayoutTermsArgs {
                window_secs: 9,
                ..terms
            },
            PayoutTermsArgs {
                window_secs: 86_401,
                ..terms
            },
        ] {
            assert!(!bad.valid(), "{bad:?}");
        }
    }

    /// A window's payouts, step after step, against the pot as it stands: paying never shrinks
    /// the window's cap, fees grow it, the window never pays more than its share of the pot as if
    /// it had paid nothing, and no step more than the pot.
    #[test]
    fn the_window_is_measured_against_the_live_pot() {
        // Tenure's terms: 10% a step, 25% a day; a 25 SOL pot, 0.5 SOL of fees a round.
        let (step_bps, window_bps) = (1_000u16, 2_500u16);
        let sol = 1_000_000_000u64;
        let mut pot = 25 * sol;
        let window_pot = pot;
        let mut paid = 0u64;
        let mut fees = 0u64;
        for _ in 0..288 {
            pot += sol / 2;
            fees += sol / 2;
            let budget = step_budget(pot, pot, 0, window_pot, paid, step_bps, window_bps);
            assert!(budget <= pot);
            pot -= budget;
            paid += budget;
            // Within 25% of the window's pot plus its fees (no bounty here).
            assert!(paid <= bps_of(window_pot + fees, u64::from(window_bps)));
        }
        // The window kept paying its fees as they came: a quarter of all it held.
        assert_eq!(paid, bps_of(window_pot + fees, u64::from(window_bps)));
        // The frozen cap would have stopped at a quarter of the first pot.
        assert!(paid > bps_of(window_pot, u64::from(window_bps)) * 5);
        // window_base: the pot plus what was paid, the start's pot a floor.
        assert_eq!(window_base(10, 100, 5), 100);
        assert_eq!(window_base(100, 10, 5), 105);
        assert_eq!(window_base(u64::MAX, 0, 5), u64::MAX);
    }

    #[test]
    fn the_terms_keep_their_layout() {
        let t = PayoutTerms {
            version: PAYOUT_TERMS_VERSION,
            bump: 250,
            mint: Pubkey::new_unique(),
            step_bps: 1,
            holding_bps: 2,
            window_bps: 3,
            window_secs: 300,
            window_start: 1_800_000_000,
            window_pot: 5,
            window_paid: 6,
            paid_total: 7,
            payouts: 8,
            last_paid_at: 9,
            reserved: [0; 28],
        };
        let mut data = Vec::new();
        t.try_serialize(&mut data).unwrap();
        assert_eq!(data.len(), PayoutTerms::LEN);
        assert_eq!(PayoutTerms::LEN, 128);
        assert_eq!(&data[10..42], t.mint.as_ref());
        assert_eq!(&data[42..44], &1u16.to_le_bytes());
        assert_eq!(&data[44..46], &2u16.to_le_bytes());
        assert_eq!(&data[46..48], &3u16.to_le_bytes());
        assert_eq!(&data[48..52], &300u32.to_le_bytes());
        assert_eq!(&data[52..60], &1_800_000_000i64.to_le_bytes());
        assert_eq!(&data[60..68], &5u64.to_le_bytes());
        assert_eq!(&data[68..76], &6u64.to_le_bytes());
        assert_eq!(&data[76..84], &7u64.to_le_bytes());
        assert_eq!(&data[84..92], &8u64.to_le_bytes());
        assert_eq!(&data[92..100], &9i64.to_le_bytes());
        assert_eq!(&data[100..], &[0u8; 28][..]);
        // The window is the clock's: its first second.
        assert_eq!(t.window_start_at(1_800_000_299), 1_800_000_000);
        assert_eq!(t.window_start_at(1_800_000_300), 1_800_000_300);
        assert_eq!(t.window_start_at(0), 0);
    }

    #[test]
    fn the_record_discriminator_and_the_authority_are_anchors_and_ours() {
        let hash = solana_sha256_hasher::hash(b"global:record_payout").to_bytes();
        assert_eq!(hash[..8], RECORD_PAYOUT_DISCRIMINATOR);
        assert_eq!(
            Pubkey::find_program_address(&[PAYOUT_AUTHORITY_SEED], &crate::ID),
            (PAYOUT_AUTHORITY, PAYOUT_AUTHORITY_BUMP)
        );
        assert!(!PAYOUT_AUTHORITY.is_on_curve());
    }
}
