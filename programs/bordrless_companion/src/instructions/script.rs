//! Script companions (2026-10-12): a builder writes their own program, the script, and it decides
//! what the companion does with its pot. Bordrless approves each script's code (`approval.rs`), or
//! the script is immutable and needs no approval (nobody can change its code: the rule the
//! launchpad applies to hooks); the companion keeps custody of everything and lets the script act
//! with the creator address signing, within invariants it checks after every step.
//!
//! - `create_script(args)`, before the launch, the mint signing (as `create_snowball`): the pot's
//!   part of every fee claim (`pot_bps`, beside the split), the script and how often it may step.
//!   The script must hold a current approval of kind `APPROVAL_KIND_SCRIPT`, unless it is immutable
//!   (`approval::script_approval`; its approval account is then not read). With `state_len` above
//!   0 the companion makes the script's state, `PDA(["script-state", mint])`: `state_len` bytes,
//!   zeroed, owned by the script, the payer paying its rent (the companion signs for it; the payer's
//!   signature stays in the companion's own instruction). Then the script's `companion_init` is
//!   called once: `[sha256("global:companion_init")[..8], mint, init_data (Borsh Vec<u8>)]` with the
//!   accounts `[creator (signer, writable), mint, state (writable; the system program's id, read
//!   only, without a state), system program]` and then the remaining accounts.
//! - `create_script_v2(args, payout)` (Loss cover, 2026-10-12): the same, and the script's payout
//!   terms (`PayoutTerms`, `PDA(["payout-terms", mint])`), the payer paying their rent. Only a
//!   script made with it may pay holders (`payout.rs`).
//! - **No signature reaches a script but the creator address's.** Every remaining account is
//!   forwarded with `is_signer = false`, in both calls; the payer is never passed. A builder's code
//!   never gets a launcher's (or a sender's) signature, so it can never move their SOL or tokens.
//! - `claim_fees` first pays the script's author (copied from the approval into the `Script`; an
//!   immutable script has none)
//!   `AUTHOR_SHARE_BPS` of what the claim took from the launch, as SOL, unless the author is the
//!   launch's beneficiary; then it splits the rest as ever, crediting the pot's part to
//!   `pending_pot` as for a Snowball.
//! - `script_step` (anyone, paid `SCRIPT_BOUNTY` from the pot), from a minute after the launch and
//!   once an `interval`, while the script's approval holds for the code it runs (an immutable
//!   script needs none):
//!   `[sha256("global:companion_step")[..8], mint]` with the accounts `[creator (signer, writable),
//!   mint, launch, companion, pool, state (writable; the system program's id, read only, without a
//!   state)]` and then the remaining accounts, none signing. Then the companion checks what the
//!   script did:
//!   - **The pot, and nothing else set aside.** The creator's bridged SOL fell by at most
//!     `pending_pot` (`ScriptOverspent`); the pot moves by what it changed (it grows when the script
//!     sold tokens).
//!   - **The dev bag stays.** The creator's tokens are at least the dev bag still held
//!     (`dev_tokens - dev_released`; `ScriptTookDevBag`).
//!   - **No lamports.** The creator address holds at least what it held (`ScriptTookLamports`).
//!   - **Custody unchanged** (`ScriptBrokeCustody`): the creator address is still a system account
//!     with no data, its bridged-SOL and token holdings are still there with no delegate, and the
//!     launch's creator fees were not claimed by the script (a claim inside a step would pass the
//!     launcher's and the buyback's shares to the pot).
//!   - **The payouts the script answered** (`payout.rs`): read from its return data after these
//!     checks, each checked against the terms, paid to the holding's owner as SOL and recorded by
//!     the launch's hook. A step with an answer earns its bounty by the entries it carries.
//!
//!   The same custody checks follow the script's `companion_init` (before the launch nothing is set
//!   aside: the creator's bridged SOL may not fall at all).
//!
//! A script never re-enters the companion (the runtime refuses it), and nothing here relies on it.
//! Whatever the script buys stays in the creator's token holding beside the dev bag, which `release`
//! alone pays out.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::system_program;
use bordrless_launch::client as launch_client;
use bordrless_launch::state::Launch;
use bordrless_token::client as token_client;

use crate::constants::*;
use crate::error::CompanionError;
use crate::events::*;
use crate::instructions::approval::{is_protocol_program, script_approval};
use crate::instructions::payout::{
    bounty_for, clear_answer, pay_answer, read_answer, PayoutTermsArgs, StepPayouts,
};
use crate::instructions::steps::{available, balance, ensure_holding, find, pay_sol};
use crate::instructions::CreatorSeeds;
use crate::state::*;

/// Arguments of `create_script`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct CreateScriptArgs {
    /// Every fee claim's split besides the pot: the four parts (with `pot_bps`) sum to 10,000.
    pub split: Split,
    /// The pot's part of every fee claim: what the script spends.
    pub pot_bps: u16,
    /// The script: a program with a current approval of kind `APPROVAL_KIND_SCRIPT`, or an immutable
    /// one, which needs none.
    pub program: Pubkey,
    /// The least time between steps (10 seconds to a day).
    pub interval: i64,
    /// The size of the script's state the companion makes (0: none; at most
    /// `MAX_SCRIPT_STATE_LEN`).
    pub state_len: u32,
    /// Handed to the script's `companion_init` (at most `MAX_SCRIPT_INIT_LEN` bytes).
    pub init_data: Vec<u8>,
}

/// Accounts of `create_script`. Remaining: forwarded to the script's `companion_init`, none
/// signing.
#[event_cpi]
#[derive(Accounts)]
#[instruction(args: CreateScriptArgs)]
pub struct CreateScript<'info> {
    /// Pays the rent of the script account and of the script's state; never passed to the script.
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The launch's mint, not created yet: it signs, so only whoever made the companion makes its
    /// script.
    pub mint: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, mint.key().as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    #[account(init, payer = payer, space = Script::LEN, seeds = [SCRIPT_SEED, mint.key().as_ref()], bump)]
    pub script: Box<Account<'info, Script>>,
    /// CHECK: the script's state, `PDA(["script-state", mint])` (seeds-checked): made here, owned by
    /// the script, when `state_len` is above 0; unused otherwise.
    #[account(mut, seeds = [SCRIPT_STATE_SEED, mint.key().as_ref()], bump)]
    pub script_state: UncheckedAccount<'info>,
    /// CHECK: the creator address (seeds-checked); signs the script's `companion_init`.
    #[account(mut, seeds = [CREATOR_SEED, mint.key().as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: the creator's bridged-SOL holding (address-checked), which the init may not touch.
    #[account(address = token_client::holding_address(&BRIDGED_SOL_MINT, &creator.key()))]
    pub creator_quote: UncheckedAccount<'info>,
    /// CHECK: the script's approval, `PDA(["approval", program])` (checked in the handler; not read
    /// for an immutable script).
    pub approval: UncheckedAccount<'info>,
    /// CHECK: the script (address-checked; an upgradeable-loader program, checked in the handler).
    #[account(address = args.program @ CompanionError::BadScript)]
    pub script_program: UncheckedAccount<'info>,
    /// CHECK: its ProgramData (address, owner and header checked in the handler).
    pub script_programdata: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Accounts of `create_script_v2`: `create_script`'s, and the script's payout terms, made here.
/// Remaining: forwarded to the script's `companion_init`, none signing.
#[event_cpi]
#[derive(Accounts)]
#[instruction(args: CreateScriptArgs)]
pub struct CreateScriptV2<'info> {
    /// Pays the rent of the script account, of the script's state and of the terms; never passed
    /// to the script.
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The launch's mint, not created yet: it signs.
    pub mint: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, mint.key().as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    #[account(init, payer = payer, space = Script::LEN, seeds = [SCRIPT_SEED, mint.key().as_ref()], bump)]
    pub script: Box<Account<'info, Script>>,
    /// CHECK: the script's state (seeds-checked), as for `create_script`.
    #[account(mut, seeds = [SCRIPT_STATE_SEED, mint.key().as_ref()], bump)]
    pub script_state: UncheckedAccount<'info>,
    /// CHECK: the creator address (seeds-checked); signs the script's `companion_init`.
    #[account(mut, seeds = [CREATOR_SEED, mint.key().as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    /// CHECK: the creator's bridged-SOL holding (address-checked), which the init may not touch.
    #[account(address = token_client::holding_address(&BRIDGED_SOL_MINT, &creator.key()))]
    pub creator_quote: UncheckedAccount<'info>,
    /// CHECK: the script's approval (checked in the handler; not read for an immutable script).
    pub approval: UncheckedAccount<'info>,
    /// CHECK: the script (address-checked; an upgradeable-loader program, checked in the handler).
    #[account(address = args.program @ CompanionError::BadScript)]
    pub script_program: UncheckedAccount<'info>,
    /// CHECK: its ProgramData (address, owner and header checked in the handler).
    pub script_programdata: UncheckedAccount<'info>,
    /// The payout terms, `PDA(["payout-terms", mint])`, made here.
    #[account(init, payer = payer, space = PayoutTerms::LEN, seeds = [PAYOUT_TERMS_SEED, mint.key().as_ref()], bump)]
    pub terms: Box<Account<'info, PayoutTerms>>,
    pub system_program: Program<'info, System>,
}

/// What the creator held when a script was called: its bridged SOL, its tokens (0 without a
/// holding) and its lamports.
#[derive(Clone, Copy)]
struct Held {
    sol: u64,
    tokens: u64,
    lamports: u64,
}

impl Held {
    fn of(all: &[AccountInfo], creator: &AccountInfo, mint: Option<&Pubkey>) -> Result<Self> {
        Ok(Self {
            sol: balance(all, &BRIDGED_SOL_MINT, creator.key)?,
            tokens: match mint {
                Some(mint) => balance(all, mint, creator.key)?,
                None => 0,
            },
            lamports: creator.lamports(),
        })
    }
}

/// Whether a script left the creator's custody as it was: the creator address a system account
/// with no data, and its holdings of `mints` still there with no delegate.
fn custody_intact(all: &[AccountInfo], creator: &AccountInfo, mints: &[Pubkey]) -> Result<bool> {
    if *creator.owner != system_program::ID || !creator.data_is_empty() {
        return Ok(false);
    }
    for mint in mints {
        let info = find(all, &token_client::holding_address(mint, creator.key))?;
        if *info.owner != TOKEN_ID {
            return Ok(false);
        }
        let h = token_client::read_holding(info)?;
        if h.delegate.is_some() || h.delegated_amount != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Calls `program` with `data`, the creator address signing (it is the one signer of `prefix`,
/// each of whose accounts is taken as given), then every account of `forwarded` with its
/// writability as the client passed it and never as a signer: no other signature (the payer's, the
/// mint's, the sender's, anyone's) ever reaches a script. The return data is cleared first, so
/// only an answer the script itself sets is read after it.
fn call_script<'info>(
    program: &AccountInfo<'info>,
    data: Vec<u8>,
    prefix: Vec<(AccountInfo<'info>, bool, bool)>,
    forwarded: &[AccountInfo<'info>],
    seeds: &CreatorSeeds,
) -> Result<()> {
    let mut metas = Vec::with_capacity(prefix.len() + forwarded.len());
    let mut infos = Vec::with_capacity(prefix.len() + forwarded.len() + 1);
    for (info, is_signer, is_writable) in prefix {
        metas.push(AccountMeta {
            pubkey: *info.key,
            is_signer,
            is_writable,
        });
        infos.push(info);
    }
    for a in forwarded {
        metas.push(AccountMeta {
            pubkey: *a.key,
            is_signer: false,
            is_writable: a.is_writable,
        });
        infos.push(a.clone());
    }
    infos.push(program.clone());
    let ix = Instruction {
        program_id: *program.key,
        accounts: metas,
        data,
    };
    clear_answer();
    invoke_signed(&ix, &infos, &[&seeds.seeds()])?;
    Ok(())
}

/// Makes the script's state, `PDA(["script-state", mint])` (`state`, signed for by `seeds`): `len`
/// bytes, zeroed, owned by `owner` (the script), its rent paid by `payer` (who signs the companion's
/// own instruction). Lamports someone sent the address before are kept (topped up to the rent, the
/// account then allocated and assigned), so nobody can block a script by funding its state first.
fn create_state<'info>(
    payer: &AccountInfo<'info>,
    state: &AccountInfo<'info>,
    system: &AccountInfo<'info>,
    owner: &Pubkey,
    len: u64,
    seeds: &[&[u8]],
) -> Result<()> {
    let rent = Rent::get()?.minimum_balance(len as usize);
    let held = state.lamports();
    if held == 0 {
        return system_program::create_account(
            CpiContext::new_with_signer(
                *system.key,
                system_program::CreateAccount {
                    from: payer.clone(),
                    to: state.clone(),
                },
                &[seeds],
            ),
            rent,
            len,
            owner,
        );
    }
    if rent > held {
        system_program::transfer(
            CpiContext::new(
                *system.key,
                system_program::Transfer {
                    from: payer.clone(),
                    to: state.clone(),
                },
            ),
            rent - held,
        )?;
    }
    system_program::allocate(
        CpiContext::new_with_signer(
            *system.key,
            system_program::Allocate {
                account_to_allocate: state.clone(),
            },
            &[seeds],
        ),
        len,
    )?;
    system_program::assign(
        CpiContext::new_with_signer(
            *system.key,
            system_program::Assign {
                account_to_assign: state.clone(),
            },
            &[seeds],
        ),
        owner,
    )
}

/// The accounts a script's setup works on, the same for `create_script` and `create_script_v2`.
struct ScriptSetup<'a, 'info> {
    payer: AccountInfo<'info>,
    mint: AccountInfo<'info>,
    companion: &'a mut Account<'info, Companion>,
    script: &'a mut Account<'info, Script>,
    script_bump: u8,
    script_state: AccountInfo<'info>,
    script_state_bump: u8,
    creator: AccountInfo<'info>,
    approval: AccountInfo<'info>,
    script_program: AccountInfo<'info>,
    script_programdata: AccountInfo<'info>,
    system: AccountInfo<'info>,
}

/// What `create_script` does, for both of its forms: the checks, the script's state made (when it
/// asks for one), its `companion_init` run once with the creator address signing, the custody
/// checks after it, the companion and the script written. Answers the event to emit.
fn setup_script<'info>(
    s: ScriptSetup<'_, 'info>,
    args: CreateScriptArgs,
    named: Vec<AccountInfo<'info>>,
    remaining: &[AccountInfo<'info>],
) -> Result<ScriptCreated> {
    let c = &*s.companion;
    require!(
        !c.launched && !c.is_game() && c.is_classic(),
        CompanionError::BadScript
    );
    require!(
        args.pot_bps > 0 && args.split.valid_with_pot(args.pot_bps),
        CompanionError::BadSplit
    );
    require!(
        (MIN_SCRIPT_INTERVAL..=MAX_SCRIPT_INTERVAL).contains(&args.interval)
            && args.init_data.len() <= MAX_SCRIPT_INIT_LEN
            && args.state_len <= MAX_SCRIPT_STATE_LEN
            && !is_protocol_program(&args.program),
        CompanionError::BadScript
    );
    // A buyback beside the pot needs the limits `create` checks only for its own split.
    if args.split.buyback_bps > 0 {
        require!(
            c.max_buyback >= MIN_MAX_BUYBACK
                && (MIN_BUYBACK_INTERVAL..=MAX_BUYBACK_INTERVAL).contains(&c.buyback_interval),
            CompanionError::BadBuybackLimits
        );
    }
    // The script's author, from its approval; an immutable script needs no approval and has none.
    let author = script_approval(&s.approval, &s.script_program, &s.script_programdata)?
        .map_or(Pubkey::default(), |a| a.author);
    let now = Clock::get()?.unix_timestamp;
    let mint = *s.mint.key;
    let seeds = CreatorSeeds::new(mint, c.creator_bump);
    let all = available(named, remaining);
    // The script's state, made by the companion: the payer's signature never leaves this
    // instruction.
    let state = if args.state_len > 0 {
        create_state(
            &s.payer,
            &s.script_state,
            &s.system,
            &args.program,
            u64::from(args.state_len),
            &[SCRIPT_STATE_SEED, mint.as_ref(), &[s.script_state_bump]],
        )?;
        (s.script_state.clone(), false, true)
    } else {
        (s.system.clone(), false, false)
    };
    let before = Held::of(&all, &s.creator, None)?;

    let mut data = Vec::with_capacity(8 + 32 + 4 + args.init_data.len());
    data.extend_from_slice(&COMPANION_INIT_DISCRIMINATOR);
    data.extend_from_slice(mint.as_ref());
    args.init_data.serialize(&mut data)?;
    call_script(
        &s.script_program,
        data,
        vec![
            (s.creator.clone(), true, true),
            (s.mint.clone(), false, false),
            state,
            (s.system.clone(), false, false),
        ],
        remaining,
        &seeds,
    )?;

    // Nothing is set aside before the launch: the init may spend none of it.
    let after = Held::of(&all, &s.creator, None)?;
    require!(after.sol >= before.sol, CompanionError::ScriptOverspent);
    require!(
        after.lamports >= before.lamports,
        CompanionError::ScriptTookLamports
    );
    require!(
        custody_intact(&all, &s.creator, &[BRIDGED_SOL_MINT])?,
        CompanionError::ScriptBrokeCustody
    );

    let c = s.companion;
    c.split = args.split;
    c.pot_bps = args.pot_bps;
    c.mode = MODE_SCRIPT;
    let sc = s.script;
    sc.version = SCRIPT_VERSION;
    sc.bump = s.script_bump;
    sc.mint = mint;
    sc.program = args.program;
    sc.interval = args.interval;
    sc.last_at = 0;
    sc.steps = 0;
    sc.created_at = now;
    sc.author = author;
    sc.author_paid = 0;
    sc.state_len = args.state_len;
    sc.reserved = [0; 20];
    Ok(ScriptCreated {
        companion: c.key(),
        mint,
        program: args.program,
        split: args.split,
        pot_bps: args.pot_bps,
        interval: args.interval,
        author,
    })
}

/// `create_script`: before the launch, the companion becomes a script companion (`mode`), its split
/// now with the pot's part, its script's state is made (when it asks for one), and the script's
/// `companion_init` runs once.
pub fn process_create_script<'info>(
    ctx: Context<'info, CreateScript<'info>>,
    args: CreateScriptArgs,
) -> Result<()> {
    let named = ctx.accounts.to_account_infos();
    let setup = ScriptSetup {
        payer: ctx.accounts.payer.to_account_info(),
        mint: ctx.accounts.mint.to_account_info(),
        companion: &mut ctx.accounts.companion,
        script: &mut ctx.accounts.script,
        script_bump: ctx.bumps.script,
        script_state: ctx.accounts.script_state.to_account_info(),
        script_state_bump: ctx.bumps.script_state,
        creator: ctx.accounts.creator.to_account_info(),
        approval: ctx.accounts.approval.to_account_info(),
        script_program: ctx.accounts.script_program.to_account_info(),
        script_programdata: ctx.accounts.script_programdata.to_account_info(),
        system: ctx.accounts.system_program.to_account_info(),
    };
    let created = setup_script(setup, args, named, ctx.remaining_accounts)?;
    emit_cpi!(created);
    Ok(())
}

/// `create_script_v2`: `create_script`, and the script's payout terms written (`BadPayoutTerms`
/// out of bounds; `PayoutTermsSet`).
pub fn process_create_script_v2<'info>(
    ctx: Context<'info, CreateScriptV2<'info>>,
    args: CreateScriptArgs,
    payout: PayoutTermsArgs,
) -> Result<()> {
    require!(payout.valid(), CompanionError::BadPayoutTerms);
    let named = ctx.accounts.to_account_infos();
    let setup = ScriptSetup {
        payer: ctx.accounts.payer.to_account_info(),
        mint: ctx.accounts.mint.to_account_info(),
        companion: &mut ctx.accounts.companion,
        script: &mut ctx.accounts.script,
        script_bump: ctx.bumps.script,
        script_state: ctx.accounts.script_state.to_account_info(),
        script_state_bump: ctx.bumps.script_state,
        creator: ctx.accounts.creator.to_account_info(),
        approval: ctx.accounts.approval.to_account_info(),
        script_program: ctx.accounts.script_program.to_account_info(),
        script_programdata: ctx.accounts.script_programdata.to_account_info(),
        system: ctx.accounts.system_program.to_account_info(),
    };
    let created = setup_script(setup, args, named, ctx.remaining_accounts)?;
    let mint = ctx.accounts.mint.key();
    let t = &mut ctx.accounts.terms;
    t.version = PAYOUT_TERMS_VERSION;
    t.bump = ctx.bumps.terms;
    t.mint = mint;
    t.step_bps = payout.step_bps;
    t.holding_bps = payout.holding_bps;
    t.window_bps = payout.window_bps;
    t.window_secs = payout.window_secs;
    t.window_start = 0;
    t.window_pot = 0;
    t.window_paid = 0;
    t.paid_total = 0;
    t.payouts = 0;
    t.last_paid_at = 0;
    t.reserved = [0; 28];
    let terms = t.key();
    emit_cpi!(created);
    emit_cpi!(PayoutTermsSet {
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

/// Accounts of `script_step`. Remaining: forwarded to the script's `companion_step`, none signing;
/// among them the token's `create_holding` accounts for the creator (its holding is created when
/// missing, the sender paying) and the bridge's `unwrap_sol` accounts (the bounty). A script that
/// answers payouts also needs its terms (writable), the launch's hook program, the payout
/// authority, the hook's state (writable) and `["hook-authority"]`, the token program and its event
/// authority, each named holding (writable: the hook records into it) and each paid owner's
/// account (writable).
#[event_cpi]
#[derive(Accounts)]
pub struct ScriptStep<'info> {
    /// Whoever sends it; paid the bounty. Its signature is never forwarded to the script.
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(mut, seeds = [COMPANION_SEED, companion.mint.as_ref()], bump = companion.bump)]
    pub companion: Box<Account<'info, Companion>>,
    /// CHECK: the creator address (seeds-checked); signs the script's step.
    #[account(mut, seeds = [CREATOR_SEED, companion.mint.as_ref()], bump = companion.creator_bump)]
    pub creator: UncheckedAccount<'info>,
    #[account(address = launch_client::launch_address(&companion.mint))]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: the token (address-checked).
    #[account(address = companion.mint)]
    pub mint: UncheckedAccount<'info>,
    /// CHECK: the launch's pool (address-checked).
    #[account(address = launch.pool)]
    pub pool: UncheckedAccount<'info>,
    #[account(mut, seeds = [SCRIPT_SEED, companion.mint.as_ref()], bump = script.bump)]
    pub script: Box<Account<'info, Script>>,
    /// CHECK: the script's state, `PDA(["script-state", mint])` (seeds-checked), handed to the script
    /// when it has one (`Script.state_len` above 0).
    #[account(mut, seeds = [SCRIPT_STATE_SEED, companion.mint.as_ref()], bump)]
    pub script_state: UncheckedAccount<'info>,
    /// CHECK: the script's approval, `PDA(["approval", script.program])` (checked in the handler;
    /// not read for an immutable script).
    pub approval: UncheckedAccount<'info>,
    /// CHECK: the script (address-checked).
    #[account(address = script.program)]
    pub script_program: UncheckedAccount<'info>,
    /// CHECK: its ProgramData (address, owner and header checked in the handler).
    pub script_programdata: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// The launch's creator fees claimed so far, read from its account as it is now.
fn fees_claimed(launch: &AccountInfo) -> Result<u64> {
    let data = launch.try_borrow_data()?;
    Ok(Launch::try_deserialize(&mut &data[..])?.creator_fees_claimed)
}

/// `script_step`: see the module.
pub fn process_script_step<'info>(ctx: Context<'info, ScriptStep<'info>>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let c = &ctx.accounts.companion;
    require!(c.is_script(), CompanionError::NotAScript);
    require!(c.launched, CompanionError::NotLaunched);
    let s = &ctx.accounts.script;
    require!(
        now >= c.launched_at.saturating_add(SCRIPT_AFTER_LAUNCH)
            && now >= s.last_at.saturating_add(s.interval),
        CompanionError::ScriptNotDue
    );
    script_approval(
        &ctx.accounts.approval,
        &ctx.accounts.script_program,
        &ctx.accounts.script_programdata,
    )?;
    let mint = c.mint;
    let seeds = CreatorSeeds::new(mint, c.creator_bump);
    let creator = ctx.accounts.creator.to_account_info();
    let cranker = ctx.accounts.cranker.key();
    let launch = ctx.accounts.launch.to_account_info();
    let all = available(ctx.accounts.to_account_infos(), ctx.remaining_accounts);
    // The creator's token holding: what the script buys lands there.
    ensure_holding(&all, cranker, mint, *creator.key)?;
    let before = Held::of(&all, &creator, Some(&mint))?;
    let claimed = fees_claimed(&launch)?;
    let pot_before = c.pending_pot;

    let state = if s.state_len > 0 {
        (ctx.accounts.script_state.to_account_info(), false, true)
    } else {
        (ctx.accounts.system_program.to_account_info(), false, false)
    };
    let mut data = Vec::with_capacity(8 + 32);
    data.extend_from_slice(&COMPANION_STEP_DISCRIMINATOR);
    data.extend_from_slice(mint.as_ref());
    call_script(
        &ctx.accounts.script_program.to_account_info(),
        data,
        vec![
            (creator.clone(), true, true),
            (ctx.accounts.mint.to_account_info(), false, false),
            (launch.clone(), false, false),
            (ctx.accounts.companion.to_account_info(), false, false),
            (ctx.accounts.pool.to_account_info(), false, false),
            state,
        ],
        ctx.remaining_accounts,
        &seeds,
    )?;
    let answer = read_answer(&s.program)?;

    // What the script did, against what it may do.
    let after = Held::of(&all, &creator, Some(&mint))?;
    let c = &ctx.accounts.companion;
    let spent = before.sol.saturating_sub(after.sol);
    require!(spent <= c.pending_pot, CompanionError::ScriptOverspent);
    let dev_bag = c.dev_tokens.saturating_sub(c.dev_released);
    require!(
        after.tokens >= before.tokens.min(dev_bag),
        CompanionError::ScriptTookDevBag
    );
    require!(
        after.lamports >= before.lamports,
        CompanionError::ScriptTookLamports
    );
    require!(
        custody_intact(&all, &creator, &[BRIDGED_SOL_MINT, mint])?
            && fees_claimed(&launch)? == claimed,
        CompanionError::ScriptBrokeCustody
    );
    // The pot moves by what the script changed: down by what it spent, up by what it sold.
    let pot = (c.pending_pot - spent)
        .checked_add(after.sol.saturating_sub(before.sol))
        .ok_or(CompanionError::MathOverflow)?;
    let wanted = bounty_for(answer.as_ref().map(|a| a.len()));
    let bounty = if pot >= wanted { wanted } else { 0 };
    // The payouts the script answered, paid and recorded (the bounty with them); without an
    // answer, or with an empty one, only the bounty.
    let cranker_info = ctx.accounts.cranker.to_account_info();
    let event_authority = ctx.accounts.event_authority.to_account_info();
    let paid = match &answer {
        Some(a) if !a.is_empty() => {
            let launch_account = &ctx.accounts.launch;
            let step = StepPayouts {
                all: &all,
                remaining: ctx.remaining_accounts,
                companion: c,
                companion_key: c.key(),
                launch: launch_account,
                launch_key: launch_account.key(),
                script: s,
                script_key: s.key(),
                script_state_key: ctx.accounts.script_state.key(),
                creator: *creator.key,
                seeds: &seeds,
                cranker: &cranker_info,
                event_authority: &event_authority,
                pot_before,
                pot_after_script: pot,
                bounty,
                now,
            };
            pay_answer(&step, a)?
        }
        _ => {
            pay_sol(&all, &seeds, *creator.key, &cranker_info, bounty)?;
            0
        }
    };
    let c = &mut ctx.accounts.companion;
    c.pending_pot = pot - paid - bounty;
    c.bounties_total = c.bounties_total.saturating_add(bounty);
    let pot_after = c.pending_pot;
    let s = &mut ctx.accounts.script;
    s.last_at = now;
    s.steps = s.steps.saturating_add(1);
    emit_cpi!(ScriptStepped {
        companion: c.key(),
        program: s.program,
        sol_before: before.sol,
        sol_after: after.sol,
        tokens_before: before.tokens,
        tokens_after: after.tokens,
        bounty,
        pot_after,
        cranker,
    });
    Ok(())
}
