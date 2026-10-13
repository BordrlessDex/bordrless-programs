//! The companion's payout primitive (Loss cover, 2026-10-12): a script names holdings in its return
//! data, the companion pays their owners SOL from the pot within the script's terms and has the
//! launch's hook record each one. The script here is `fixtures/scripts/payouts`, whose steps answer
//! whatever bytes their state holds (so a bad answer is as easy as a good one), trade first when
//! told, or try to re-enter the companion; the hook is the Loss cover hook, which records.
//!
//! Every rule of `docs/companions.md` "Payouts": the terms (`create_script_v2`), a step that pays
//! and records, each bound refused (the step's, the window's, the holding's, a non-holding, a
//! holding of another mint, one named twice, an answer over 16 or of another version, an owner
//! left out or read-only, a program's account, the launcher, the pool's vault, an account left
//! below its rent), touches, old scripts unchanged, the bounty by entries, a hook that can't
//! record, a kit token, a trade with payouts in one step, and the re-entry the runtime refuses.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::AnchorSerialize;
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateScriptArgs, PayoutTermsArgs};
use bordrless_companion::state::{
    Companion, PayoutAnswer, PayoutEntry, PayoutTerms, Script, Split,
};
use bordrless_core::policy;
use bordrless_hook::token_flags;
use bordrless_launch::client::{self as launch, LaunchKeys};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::timelock::programdata_hash;
use bordrless_swap::client as swap;
use bordrless_token::client as token;
use loss_cover_hook::{client as hook, LossCoverError, LossCoverState, Record};
use script_payouts as sp;
use solana_keypair::Keypair;
use solana_signer::Signer;

const FIX: Pubkey = sp::ID;
const CREATOR_FEE: u16 = 200;
/// Bordrless's upgrade key: approved scripts are deployed under it.
const BORDRLESS_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[1];
/// The payout window of every test coin: five minutes (`T0` starts one).
const WINDOW: u32 = 300;
/// The remaining accounts a step's candidates start at: after `payout_accounts`'s seven.
const FIRST: u8 = 7;

fn code(e: CompanionError) -> u32 {
    u32::from(e)
}

#[track_caller]
fn refused(tx: &Tx, e: CompanionError) {
    tx.expect_code(code(e));
    let name = format!("{e:?}");
    assert!(
        tx.logs()
            .iter()
            .any(|l| l.contains(&format!("Error Code: {name}"))),
        "expected {name}\n{}",
        tx.logs().join("\n")
    );
}

/// A world with the fixture script loaded, deployed by Bordrless and approved as a script (no
/// author).
fn world() -> World {
    let mut w = World::new();
    w.env
        .svm
        .add_program(FIX, &program_bytes("script_payouts"))
        .expect("load the script");
    w.env.set_upgrade_authority(FIX, Some(BORDRLESS_KEY));
    let deployer = w.env.deployer.insecure_clone();
    let hash = programdata_hash(&w.env, &FIX);
    w.env
        .send(
            &[companion::approve(
                deployer.pubkey(),
                FIX,
                APPROVAL_KIND_SCRIPT,
                hash,
                Pubkey::default(),
            )],
            &[&deployer],
        )
        .ok();
    w
}

fn create_args() -> CreateArgs {
    CreateArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 10_000,
        },
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL / 2,
    }
}

/// 70% of every fee claim to the pot, 30% to the launcher; a step a minute at most; the fixture's
/// state, doing nothing until a test sets it.
fn script_args() -> CreateScriptArgs {
    CreateScriptArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 3_000,
        },
        pot_bps: 7_000,
        program: FIX,
        interval: 60,
        state_len: sp::STATE_LEN,
        init_data: sp::state_bytes(sp::MODE_NOTHING, 0, &[]),
    }
}

/// Every cap at 100%, five-minute windows.
fn full_terms() -> PayoutTermsArgs {
    PayoutTermsArgs {
        step_bps: 10_000,
        holding_bps: 10_000,
        window_bps: 10_000,
        window_secs: WINDOW,
    }
}

/// A launch config naming `hook` with `flags` (no kit rules).
fn hook_config(w: &mut World, creator: &Keypair, hook: Pubkey, flags: u16) -> Pubkey {
    let (config, tx) = w.create_config(
        creator,
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: CREATOR_FEE,
            custom_hook: Some(hook),
            custom_hook_flags: flags,
            label: "Cover".to_string(),
        },
    );
    tx.ok();
    config
}

/// The companion's launch of `mint` from `config` (a custom hook's accounts resolved from its
/// registry), or with inline `rules` without one.
fn launch_tx(
    w: &mut World,
    launcher: &Keypair,
    mint: &Keypair,
    config: Option<&Pubkey>,
    rules: LaunchRules,
) -> Tx {
    let (args, custom, config) = match config {
        Some(config) => {
            let c = w.launch_config(config);
            let custom = c
                .custom_hook
                .map(|h| w.custom_hook_accounts(&h, &mint.pubkey()));
            (
                World::launch_args("LCOV", c.creator_fee_bps, VQ, c.rules),
                custom,
                Some(*config),
            )
        }
        None => (
            World::launch_args("LCOV", CREATOR_FEE, VQ, rules),
            None,
            None,
        ),
    };
    let inner = launch::create_launch_with(
        companion::creator_address(&mint.pubkey()),
        mint.pubkey(),
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        args.clone(),
        config,
        custom.as_ref(),
    );
    let ix = companion::launch(launcher.pubkey(), mint.pubkey(), &inner, args);
    w.env.send_paid_by(&[ix], launcher, &[mint])
}

/// A Loss cover coin whose script is the fixture, made with the payout terms `payout` (or with
/// `create_script`, without terms), the hook prepared, launched by `launcher`. Answers the mint.
fn launch_coin(w: &mut World, launcher: &Keypair, payout: Option<PayoutTermsArgs>) -> Pubkey {
    let config = hook_config(w, launcher, loss_cover_hook::ID, loss_cover_hook::FLAGS);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let script = match payout {
        Some(p) => companion::create_script_v2(launcher.pubkey(), m, script_args(), p, vec![]),
        None => companion::create_script(launcher.pubkey(), m, script_args(), vec![]),
    };
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), m, create_args()),
        hook::prepare(launcher.pubkey(), m),
        script,
    ];
    w.env.send_paid_by(&ixs, launcher, &[&mint]).ok();
    launch_tx(w, launcher, &mint, Some(&config), LaunchRules::NONE).ok();
    assert_eq!(w.launch(&m).custom_hook, Some(loss_cover_hook::ID));
    m
}

fn keys(w: &World, mint: &Pubkey) -> LaunchKeys {
    LaunchKeys::of(&w.launch(mint))
}

fn companion_of(w: &World, mint: &Pubkey) -> Companion {
    w.env.read(&companion::companion_address(mint))
}

fn terms_of(w: &World, mint: &Pubkey) -> PayoutTerms {
    w.env.read(&companion::payout_terms_address(mint))
}

fn record_of(w: &World, mint: &Pubkey, owner: &Pubkey) -> Record {
    Record::decode(&w.env.hook_data(mint, owner))
}

/// Trades enough to accrue creator fees: buys and sells by a few wallets.
fn trade(w: &mut World, mint: &Pubkey, wallets: usize, sol: u64) {
    for _ in 0..wallets {
        let t = w.wallet_with_sol(sol + SOL);
        w.buy(&t, mint, sol).ok();
        let held = w.env.holding(mint, &t.pubkey());
        w.sell(&t, mint, held / 4).ok();
    }
}

/// Fees into the pot: trades, then a claim. Answers a cranker.
fn fill_pot(w: &mut World, mint: &Pubkey) -> Keypair {
    w.env.warp(31);
    trade(w, mint, 4, 2 * SOL);
    let cranker = w.wallet_with_sol(SOL);
    let tx = w.env.send_paid_by(
        &[companion::claim_fees_script(cranker.pubkey(), *mint, None)],
        &cranker,
        &[],
    );
    tx.ok();
    let funded: PotFunded = tx.event();
    assert!(funded.to_pot > 10 * SCRIPT_BOUNTY);
    cranker
}

/// A holder: a wallet that bought `sol` of the token (its record set by the hook).
fn holder(w: &mut World, mint: &Pubkey, sol: u64) -> Keypair {
    let t = w.wallet_with_sol(sol + SOL);
    w.buy(&t, mint, sol).ok();
    assert!(record_of(w, mint, &t.pubkey()).is_set());
    t
}

/// The fixture's state for `mint` rewritten: the mode, its parameter, the answer's bytes.
fn set_state(w: &mut World, mint: &Pubkey, mode: u8, param: u64, answer: &[u8]) {
    let key = sp::state_address(mint);
    let mut a = w.env.account(&key).expect("the script's state");
    let bytes = sp::state_bytes(mode, param, answer);
    a.data[..bytes.len()].copy_from_slice(&bytes);
    w.env.put(key, a);
}

fn entry(account: u8, lamports: u64, tag: u32) -> PayoutEntry {
    PayoutEntry {
        account,
        lamports,
        tag,
    }
}

/// An answer's bytes, version 1.
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

/// The candidates' accounts: each holding (writable: the hook records into it) and, when paid, its
/// owner (writable); answers them and each holding's index among the step's remaining accounts.
fn candidates(mint: &Pubkey, list: &[(Pubkey, bool)]) -> (Vec<AccountMeta>, Vec<u8>) {
    let mut metas = Vec::new();
    let mut idx = Vec::new();
    for (owner, paid) in list {
        idx.push(FIRST + metas.len() as u8);
        metas.push(AccountMeta::new(token::holding_address(mint, owner), false));
        if *paid {
            metas.push(AccountMeta::new(*owner, false));
        }
    }
    (metas, idx)
}

/// What the fixture's buy needs among the step's accounts: the swap's, with the hook's slice for
/// the pool vault's transfer to the creator address, and the hook's registry.
fn buy_accounts(w: &World, mint: &Pubkey) -> Vec<AccountMeta> {
    let creator = companion::creator_address(mint);
    let ix = w.launch_swap_to_ix(&creator, &creator, mint, 1, 0);
    let mut metas: Vec<AccountMeta> = ix
        .accounts
        .iter()
        .map(|a| AccountMeta {
            pubkey: a.pubkey,
            is_signer: false,
            is_writable: a.is_writable,
        })
        .collect();
    metas.push(AccountMeta::new_readonly(ix.program_id, false));
    metas.push(AccountMeta::new_readonly(
        bordrless_hook::hook_accounts_address(&loss_cover_hook::ID, mint).0,
        false,
    ));
    metas
}

/// A step of `mint`'s fixture script: the payout accounts, then `extra`, as the sender passes them.
fn step_ix(w: &World, cranker: Pubkey, mint: &Pubkey, extra: Vec<AccountMeta>) -> Instruction {
    let mut accounts = companion::payout_accounts(mint, &loss_cover_hook::ID);
    assert_eq!(accounts.len(), usize::from(FIRST));
    accounts.extend(extra);
    companion::script_step(cranker, &keys(w, mint), FIX, accounts)
}

/// Sets the fixture's state and sends a step with `extra` after the payout accounts.
fn step(
    w: &mut World,
    cranker: &Keypair,
    mint: &Pubkey,
    mode: u8,
    param: u64,
    answer: &[u8],
    extra: Vec<AccountMeta>,
) -> Tx {
    set_state(w, mint, mode, param, answer);
    let ix = step_ix(w, cranker.pubkey(), mint, extra);
    w.env.send_paid_by(&[ix], cranker, &[])
}

/// A paying step: `entries` answered with `extra` after the payout accounts.
fn pay(
    w: &mut World,
    cranker: &Keypair,
    mint: &Pubkey,
    entries: Vec<PayoutEntry>,
    extra: Vec<AccountMeta>,
) -> Tx {
    step(
        w,
        cranker,
        mint,
        sp::MODE_ANSWER,
        0,
        &answer(entries),
        extra,
    )
}

/// A coin with its pot filled and two holders, ready to step: the cranker, the holders.
fn ready(
    w: &mut World,
    payout: Option<PayoutTermsArgs>,
) -> (Keypair, Pubkey, Keypair, Keypair, Keypair) {
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = launch_coin(w, &launcher, payout);
    let cranker = fill_pot(w, &mint);
    let a = holder(w, &mint, SOL);
    let b = holder(w, &mint, SOL / 2);
    w.env.warp(30);
    (launcher, mint, cranker, a, b)
}

#[test]
fn create_script_v2_writes_the_terms_and_create_script_none() {
    let mut w = world();
    let launcher = w.wallet_with_sol(10 * SOL);
    let config = hook_config(
        &mut w,
        &launcher,
        loss_cover_hook::ID,
        loss_cover_hook::FLAGS,
    );
    // Out of bounds: refused, nothing made.
    for bad in [
        PayoutTermsArgs {
            step_bps: 0,
            ..full_terms()
        },
        PayoutTermsArgs {
            holding_bps: 10_001,
            ..full_terms()
        },
        PayoutTermsArgs {
            window_bps: 0,
            ..full_terms()
        },
        PayoutTermsArgs {
            window_secs: 9,
            ..full_terms()
        },
        PayoutTermsArgs {
            window_secs: 86_401,
            ..full_terms()
        },
    ] {
        let mint = Keypair::new();
        let ixs = [
            companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                mint.pubkey(),
                create_args(),
            ),
            companion::create_script_v2(
                launcher.pubkey(),
                mint.pubkey(),
                script_args(),
                bad,
                vec![],
            ),
        ];
        refused(
            &w.env.send_paid_by(&ixs, &launcher, &[&mint]),
            CompanionError::BadPayoutTerms,
        );
        assert!(w
            .env
            .account(&companion::payout_terms_address(&mint.pubkey()))
            .is_none());
    }
    // Within them: the script as `create_script` makes it, and the terms.
    let mint = Keypair::new();
    let m = mint.pubkey();
    let terms = PayoutTermsArgs {
        step_bps: 1_000,
        holding_bps: 2_500,
        window_bps: 5_000,
        window_secs: 600,
    };
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), m, create_args()),
        hook::prepare(launcher.pubkey(), m),
        companion::create_script_v2(launcher.pubkey(), m, script_args(), terms, vec![]),
    ];
    let tx = w.env.send_paid_by(&ixs, &launcher, &[&mint]);
    tx.ok();
    let created: ScriptCreated = tx.event();
    assert_eq!((created.program, created.pot_bps), (FIX, 7_000));
    let set: PayoutTermsSet = tx.event();
    assert_eq!(
        (set.companion, set.mint, set.terms),
        (
            companion::companion_address(&m),
            m,
            companion::payout_terms_address(&m)
        )
    );
    assert_eq!(
        (
            set.step_bps,
            set.holding_bps,
            set.window_bps,
            set.window_secs
        ),
        (1_000, 2_500, 5_000, 600)
    );
    let t = terms_of(&w, &m);
    assert_eq!(
        (
            t.version,
            t.mint,
            t.step_bps,
            t.holding_bps,
            t.window_bps,
            t.window_secs
        ),
        (PAYOUT_TERMS_VERSION, m, 1_000, 2_500, 5_000, 600)
    );
    assert_eq!(
        (
            t.window_start,
            t.window_pot,
            t.window_paid,
            t.paid_total,
            t.payouts,
            t.last_paid_at
        ),
        (0, 0, 0, 0, 0, 0)
    );
    let account = w.env.account(&companion::payout_terms_address(&m)).unwrap();
    assert_eq!(
        (account.owner, account.data.len()),
        (bordrless_companion::ID, 128)
    );
    let s: Script = w.env.read(&companion::script_address(&m));
    assert_eq!((s.program, s.state_len), (FIX, sp::STATE_LEN));
    assert!(companion_of(&w, &m).is_script());
    launch_tx(&mut w, &launcher, &mint, Some(&config), LaunchRules::NONE).ok();
    // The old form makes no terms.
    let old = Keypair::new();
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            old.pubkey(),
            create_args(),
        ),
        companion::create_script(launcher.pubkey(), old.pubkey(), script_args(), vec![]),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&old]).ok();
    assert!(w
        .env
        .account(&companion::payout_terms_address(&old.pubkey()))
        .is_none());
}

/// A script that answers within its terms: the owner named with lamports is paid SOL, the one
/// named with 0 is touched (recorded, not paid, no owner account needed); the hook records each
/// (paid, round, when), the terms and the pot count them, one `ScriptPayout` each, the bounty by
/// the entries.
#[test]
fn a_script_that_answers_within_terms_pays_and_records() {
    let mut w = world();
    let (_launcher, mint, cranker, a, b) = ready(&mut w, Some(full_terms()));
    let creator = companion::creator_address(&mint);
    let c = companion_of(&w, &mint);
    let pot = c.pending_pot;
    let x = pot / 10;
    let (extra, idx) = candidates(&mint, &[(a.pubkey(), true), (b.pubkey(), false)]);
    let a_before = w.env.lamports(&a.pubkey());
    let b_before = w.env.lamports(&b.pubkey());
    let cranker_before = w.env.lamports(&cranker.pubkey());
    let bounties_before = c.bounties_total;
    let rec_a = record_of(&w, &mint, &a.pubkey());
    let rec_b = record_of(&w, &mint, &b.pubkey());
    let now = w.env.now;
    let tx = pay(
        &mut w,
        &cranker,
        &mint,
        vec![entry(idx[0], x, 42), entry(idx[1], 0, 42)],
        extra,
    );
    tx.ok();
    println!(
        "payout step, one paid and one touched: CU {} size {} height {} trace {}",
        tx.cu(),
        tx.size,
        tx.max_height(),
        tx.trace_len()
    );
    assert!(tx.max_height() <= 5);
    // Paid and touched.
    assert_eq!(w.env.lamports(&a.pubkey()), a_before + x);
    assert_eq!(w.env.lamports(&b.pubkey()), b_before);
    let bounty = SCRIPT_BOUNTY * 2 / 8;
    // The sender: the bounty, less its fee and the rent of the creator's token holding it made.
    let rent = w.env.rent(bordrless_token::state::Holding::LEN);
    assert_eq!(
        w.env.lamports(&cranker.pubkey()),
        cranker_before - 5_000 - rent + bounty
    );
    // The records: the hook wrote paid, round and when; nothing else moved.
    let after_a = record_of(&w, &mint, &a.pubkey());
    assert_eq!((after_a.paid, after_a.round, after_a.paid_at), (x, 42, now));
    assert_eq!(
        (
            after_a.entry,
            after_a.amount,
            after_a.basis_at,
            after_a.flags
        ),
        (rec_a.entry, rec_a.amount, rec_a.basis_at, rec_a.flags)
    );
    assert_eq!(after_a.updated_slot, w.env.slot);
    let after_b = record_of(&w, &mint, &b.pubkey());
    assert_eq!((after_b.paid, after_b.round, after_b.paid_at), (0, 42, 0));
    assert_eq!((after_b.entry, after_b.amount), (rec_b.entry, rec_b.amount));
    let state: LossCoverState = w.env.read(&hook::state_address(&mint));
    assert_eq!((state.paid_total, state.records), (x, 2));
    // The terms' counters, the window's first step.
    let t = terms_of(&w, &mint);
    assert_eq!(t.window_start, now - now % i64::from(WINDOW));
    assert_eq!((t.window_pot, t.window_paid), (pot, x));
    assert_eq!((t.paid_total, t.payouts, t.last_paid_at), (x, 2, now));
    // The pot: the payout and the bounty; the holding holds exactly what is set aside.
    let c = companion_of(&w, &mint);
    assert_eq!(c.pending_pot, pot - x - bounty);
    assert_eq!(w.env.holding(&w.sol, &creator), c.set_aside().unwrap());
    assert_eq!(c.bounties_total, bounties_before + bounty);
    // The events: one per entry, then the step.
    let events = tx.events::<ScriptPayout>();
    assert_eq!(events.len(), 2);
    assert_eq!(
        (
            events[0].holding,
            events[0].owner,
            events[0].lamports,
            events[0].tag
        ),
        (
            token::holding_address(&mint, &a.pubkey()),
            a.pubkey(),
            x,
            42
        )
    );
    assert_eq!(
        (
            events[0].window_paid,
            events[0].paid_total,
            events[0].pot_after
        ),
        (x, x, c.pending_pot)
    );
    assert_eq!(
        (
            events[1].owner,
            events[1].lamports,
            events[1].window_paid,
            events[1].pot_after
        ),
        (b.pubkey(), 0, x, c.pending_pot)
    );
    assert_eq!(
        (events[0].companion, events[0].program),
        (companion::companion_address(&mint), FIX)
    );
    let stepped: ScriptStepped = tx.event();
    assert_eq!((stepped.bounty, stepped.pot_after), (bounty, c.pending_pot));
    assert_eq!(stepped.sol_before, stepped.sol_after);
    // A second payment adds to the record.
    w.env.warp(60);
    let (extra, idx) = candidates(&mint, &[(a.pubkey(), true)]);
    pay(
        &mut w,
        &cranker,
        &mint,
        vec![entry(idx[0], 1_000, 43)],
        extra,
    )
    .ok();
    let after_a = record_of(&w, &mint, &a.pubkey());
    assert_eq!((after_a.paid, after_a.round), (x + 1_000, 43));
    assert_eq!(terms_of(&w, &mint).paid_total, x + 1_000);
}

/// The step's share of the pot, the holding's share of the step and the window's share of the
/// pot: an entry over any is refused, never clamped, and nothing moves.
#[test]
fn an_entry_over_the_step_the_holding_or_the_window_is_refused() {
    // The step: 10% of the pot before the step.
    let mut w = world();
    let terms = PayoutTermsArgs {
        step_bps: 1_000,
        ..full_terms()
    };
    let (_l, mint, cranker, a, _b) = ready(&mut w, Some(terms));
    let pot = companion_of(&w, &mint).pending_pot;
    let cap = pot * 1_000 / 10_000;
    let (extra, idx) = candidates(&mint, &[(a.pubkey(), true)]);
    let before = w.env.lamports(&a.pubkey());
    refused(
        &pay(
            &mut w,
            &cranker,
            &mint,
            vec![entry(idx[0], cap + 1, 1)],
            extra.clone(),
        ),
        CompanionError::PayoutOverBound,
    );
    assert_eq!(w.env.lamports(&a.pubkey()), before);
    assert_eq!(companion_of(&w, &mint).pending_pot, pot);
    assert_eq!(record_of(&w, &mint, &a.pubkey()).round, 0);
    pay(&mut w, &cranker, &mint, vec![entry(idx[0], cap, 1)], extra).ok();
    assert_eq!(w.env.lamports(&a.pubkey()), before + cap);

    // The holding: half of the step's budget, which is the pot less the bounty.
    let mut w = world();
    let terms = PayoutTermsArgs {
        holding_bps: 5_000,
        ..full_terms()
    };
    let (_l, mint, cranker, a, b) = ready(&mut w, Some(terms));
    let pot = companion_of(&w, &mint).pending_pot;
    let (extra, idx) = candidates(&mint, &[(a.pubkey(), true), (b.pubkey(), true)]);
    let budget = pot - SCRIPT_BOUNTY * 2 / 8;
    let cap = budget * 5_000 / 10_000;
    refused(
        &pay(
            &mut w,
            &cranker,
            &mint,
            vec![entry(idx[0], cap + 1, 1), entry(idx[1], 0, 1)],
            extra.clone(),
        ),
        CompanionError::PayoutOverBound,
    );
    // Two at the cap: the whole budget, taken.
    let a_before = w.env.lamports(&a.pubkey());
    pay(
        &mut w,
        &cranker,
        &mint,
        vec![entry(idx[0], cap, 1), entry(idx[1], cap, 1)],
        extra,
    )
    .ok();
    assert_eq!(w.env.lamports(&a.pubkey()), a_before + cap);
    assert_eq!(
        companion_of(&w, &mint).pending_pot,
        pot - 2 * cap - SCRIPT_BOUNTY * 2 / 8
    );

    // The window: 10% of the pot as it stood at the window's first step, over the window.
    let mut w = world();
    let terms = PayoutTermsArgs {
        window_bps: 1_000,
        ..full_terms()
    };
    let (_l, mint, cranker, a, b) = ready(&mut w, Some(terms));
    let pot = companion_of(&w, &mint).pending_pot;
    let window = pot * 1_000 / 10_000;
    let (extra, idx) = candidates(&mint, &[(a.pubkey(), true), (b.pubkey(), false)]);
    refused(
        &pay(
            &mut w,
            &cranker,
            &mint,
            vec![entry(idx[0], window + 1, 1)],
            extra.clone(),
        ),
        CompanionError::PayoutOverBound,
    );
    pay(
        &mut w,
        &cranker,
        &mint,
        vec![entry(idx[0], window, 1)],
        extra.clone(),
    )
    .ok();
    let t = terms_of(&w, &mint);
    assert_eq!((t.window_pot, t.window_paid), (pot, window));
    // The same window: nothing more, a touch still goes through.
    w.env.warp(60);
    refused(
        &pay(
            &mut w,
            &cranker,
            &mint,
            vec![entry(idx[0], 1, 2)],
            extra.clone(),
        ),
        CompanionError::PayoutOverBound,
    );
    pay(
        &mut w,
        &cranker,
        &mint,
        vec![entry(idx[1], 0, 2)],
        extra.clone(),
    )
    .ok();
    // The next window: its own pot, 10% again.
    let now = w.env.now;
    w.env.warp(i64::from(WINDOW) - now % i64::from(WINDOW));
    let pot2 = companion_of(&w, &mint).pending_pot;
    let window2 = pot2 * 1_000 / 10_000;
    refused(
        &pay(
            &mut w,
            &cranker,
            &mint,
            vec![entry(idx[0], window2 + 1, 3)],
            extra.clone(),
        ),
        CompanionError::PayoutOverBound,
    );
    pay(
        &mut w,
        &cranker,
        &mint,
        vec![entry(idx[0], window2, 3)],
        extra,
    )
    .ok();
    let t = terms_of(&w, &mint);
    assert_eq!((t.window_pot, t.window_paid), (pot2, window2));
    assert_eq!(t.window_start, w.env.now - w.env.now % i64::from(WINDOW));
}

/// What is no answer the terms could pay: a bad version, over 16 entries, bytes short or over, an
/// index past the accounts, an account that is no holding, a holding of another mint, a holding
/// named twice. Each fails the whole step, and nothing moves.
#[test]
fn a_malformed_answer_fails_the_step() {
    let mut w = world();
    let (_l, mint, cranker, a, b) = ready(&mut w, Some(full_terms()));
    let pot = companion_of(&w, &mint).pending_pot;
    // The wallet of A, A's bridged-SOL holding and B's token holding among the candidates.
    let (mut extra, idx) = candidates(&mint, &[(a.pubkey(), true), (b.pubkey(), true)]);
    let wallet_at = FIRST + extra.len() as u8;
    extra.push(AccountMeta::new_readonly(
        token::holding_address(&w.sol, &a.pubkey()),
        false,
    ));
    let sol_at = wallet_at;
    let a_at = idx[0];
    let owner_at = idx[0] + 1;
    let good = vec![entry(a_at, 1_000, 1)];
    // Version 2 is a wallet answer since 2026-10-13 (`companion_payees_wallets.rs`); 3 is none.
    let mut version = answer(good.clone());
    version[0] = 3;
    let mut short = answer(good.clone());
    short.pop();
    let mut long = answer(good.clone());
    long.push(0);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("a bad version", version),
        ("a byte short", short),
        ("a byte over", long),
        ("garbage", vec![9; 7]),
        ("over 16 entries", answer(vec![entry(idx[1], 0, 1); 17])),
        ("an index past the accounts", answer(vec![entry(200, 0, 1)])),
        ("a wallet as a holding", answer(vec![entry(owner_at, 0, 1)])),
        (
            "a holding of another mint",
            answer(vec![entry(sol_at, 0, 1)]),
        ),
        (
            "a holding named twice",
            answer(vec![entry(a_at, 1_000, 1), entry(a_at, 0, 1)]),
        ),
    ];
    for (what, bytes) in cases {
        let tx = step(
            &mut w,
            &cranker,
            &mint,
            sp::MODE_ANSWER,
            0,
            &bytes,
            extra.clone(),
        );
        refused(&tx, CompanionError::BadPayoutAnswer);
        assert_eq!(companion_of(&w, &mint).pending_pot, pot, "{what}");
        assert_eq!(record_of(&w, &mint, &a.pubkey()).round, 0, "{what}");
    }
    // Sixteen entries are an answer: the same holding can't be named sixteen times, but sixteen
    // touches of sixteen holders can (`a_full_answer_earns_the_whole_bounty`).
    let tx = step(
        &mut w,
        &cranker,
        &mint,
        sp::MODE_ANSWER,
        0,
        &answer(good),
        extra,
    );
    tx.ok();
}

/// Who is never paid: the launcher's own holding (`PayoutToBeneficiary`); the creator address's,
/// the pool's vault, a program's (the script's own, the token program's), an owner left out or
/// passed read-only, an account that would stay below its rent-exempt minimum (`PayoutTarget`).
#[test]
fn who_is_never_paid() {
    let mut w = world();
    let (launcher, mint, cranker, a, _b) = ready(&mut w, Some(full_terms()));
    let creator = companion::creator_address(&mint);
    let pool = w.launch(&mint).pool;
    // The launcher holds too.
    w.buy(&launcher, &mint, SOL / 4).ok();
    // Holdings whose owners are programs: the script's own (excluded by name) and the token
    // program's (executable, so no transfer can pay it), each given tokens by A (the hook records
    // them: they are on the curve).
    w.holdings(&a, mint, &[FIX, bordrless_token::ID]);
    w.send_tokens(&a, mint, &FIX, 1_000).ok();
    w.send_tokens(&a, mint, &bordrless_token::ID, 1_000).ok();
    // A fresh wallet with no lamports that holds tokens.
    let poor = Keypair::new();
    w.holdings(&a, mint, &[poor.pubkey()]);
    w.send_tokens(&a, mint, &poor.pubkey(), 1_000).ok();
    assert_eq!(w.env.lamports(&poor.pubkey()), 0);
    let holding = |owner: &Pubkey| AccountMeta::new(token::holding_address(&mint, owner), false);
    let mut extra = vec![
        holding(&launcher.pubkey()),                                // 7
        AccountMeta::new(launcher.pubkey(), false),                 // 8
        holding(&creator),                                          // 9
        AccountMeta::new(swap::vault_address(&pool, &mint), false), // 10
        holding(&FIX),                                              // 11
        AccountMeta::new_readonly(FIX, false),                      // 12
        holding(&bordrless_token::ID),                              // 13
        AccountMeta::new_readonly(bordrless_token::ID, false),      // 14
        holding(&a.pubkey()),                                       // 15: its owner left out
        holding(&poor.pubkey()),                                    // 16
        AccountMeta::new(poor.pubkey(), false),                     // 17
    ];
    let rent_min = w.env.rent(0);
    let pot = companion_of(&w, &mint).pending_pot;
    let cases: Vec<(&str, u8, u64, CompanionError)> = vec![
        (
            "the launcher",
            7,
            1_000,
            CompanionError::PayoutToBeneficiary,
        ),
        (
            "the launcher, a touch",
            7,
            0,
            CompanionError::PayoutToBeneficiary,
        ),
        ("the creator address", 9, 0, CompanionError::PayoutTarget),
        ("the pool's vault", 10, 0, CompanionError::PayoutTarget),
        (
            "the script's own holding",
            11,
            1_000,
            CompanionError::PayoutTarget,
        ),
        (
            "the token program's holding",
            13,
            1_000,
            CompanionError::PayoutTarget,
        ),
        ("an owner left out", 15, 1_000, CompanionError::PayoutTarget),
        (
            "below the rent-exempt minimum",
            16,
            rent_min - 1,
            CompanionError::PayoutTarget,
        ),
    ];
    for (what, at, lamports, e) in cases {
        let tx = pay(
            &mut w,
            &cranker,
            &mint,
            vec![entry(at, lamports, 1)],
            extra.clone(),
        );
        refused(&tx, e);
        assert_eq!(companion_of(&w, &mint).pending_pot, pot, "{what}");
    }
    // The owner passed read-only.
    let (mut ro, idx) = candidates(&mint, &[(a.pubkey(), true)]);
    ro[1].is_writable = false;
    refused(
        &pay(&mut w, &cranker, &mint, vec![entry(idx[0], 1_000, 1)], ro),
        CompanionError::PayoutTarget,
    );
    // The token program's holding, touched: its owner is on the curve and holds a record, and a
    // touch pays nothing, so it is recorded (the hook's rule; the companion pays it nothing).
    // The poor wallet paid its minimum: opened.
    extra.push(AccountMeta::new(a.pubkey(), false));
    let tx = pay(
        &mut w,
        &cranker,
        &mint,
        vec![entry(16, rent_min, 1), entry(15, 1, 1)],
        extra,
    );
    tx.ok();
    assert_eq!(w.env.lamports(&poor.pubkey()), rent_min);
    assert_eq!(record_of(&w, &mint, &poor.pubkey()).paid, rent_min);
}

/// A script made with `create_script` has no terms: an answer with an entry is refused
/// (`PayoutsOff`), an empty answer passes and earns nothing, no answer keeps the flat bounty, and a
/// trade's return data (another program's) is no answer at all.
#[test]
fn old_scripts_are_unchanged() {
    let mut w = world();
    let (_l, mint, cranker, a, _b) = ready(&mut w, None);
    assert!(w
        .env
        .account(&companion::payout_terms_address(&mint))
        .is_none());
    let pot = companion_of(&w, &mint).pending_pot;
    let (extra, idx) = candidates(&mint, &[(a.pubkey(), true)]);
    refused(
        &pay(
            &mut w,
            &cranker,
            &mint,
            vec![entry(idx[0], 0, 1)],
            extra.clone(),
        ),
        CompanionError::PayoutsOff,
    );
    // An empty answer: the step passes, nothing paid, no bounty.
    let tx = pay(&mut w, &cranker, &mint, vec![], extra.clone());
    tx.ok();
    let ev: ScriptStepped = tx.event();
    assert_eq!((ev.bounty, ev.pot_after), (0, pot));
    assert!(tx.events::<ScriptPayout>().is_empty());
    // No answer: the flat bounty, as ever.
    w.env.warp(60);
    let tx = step(
        &mut w,
        &cranker,
        &mint,
        sp::MODE_NOTHING,
        0,
        &[],
        extra.clone(),
    );
    tx.ok();
    let ev: ScriptStepped = tx.event();
    assert_eq!(
        (ev.bounty, ev.pot_after),
        (SCRIPT_BOUNTY, pot - SCRIPT_BOUNTY)
    );
    // A trade leaves the DEX's hooks' return data behind: no answer, the trade's step as ever.
    w.env.warp(60);
    let pot = companion_of(&w, &mint).pending_pot;
    let buy = SOL / 50;
    let mut with_buy = extra.clone();
    with_buy.extend(buy_accounts(&w, &mint));
    let tx = step(
        &mut w,
        &cranker,
        &mint,
        sp::MODE_BUY_THEN_ANSWER,
        buy,
        &[],
        with_buy,
    );
    tx.ok();
    let ev: ScriptStepped = tx.event();
    assert_eq!(ev.sol_before - ev.sol_after, buy);
    assert!(ev.tokens_after > 0);
    assert_eq!(
        (ev.bounty, ev.pot_after),
        (SCRIPT_BOUNTY, pot - buy - SCRIPT_BOUNTY)
    );
    assert!(tx.events::<ScriptPayout>().is_empty());
    assert_eq!(record_of(&w, &mint, &a.pubkey()).round, 0);
}

/// The bounty follows the entries: a full answer earns the whole `SCRIPT_BOUNTY`, a thin one its
/// part, an empty one nothing; sixteen touches fit a step.
#[test]
fn the_bounty_follows_the_entries() {
    let mut w = world();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = launch_coin(&mut w, &launcher, Some(full_terms()));
    let cranker = fill_pot(&mut w, &mint);
    let holders: Vec<Keypair> = (0..16).map(|_| holder(&mut w, &mint, SOL / 10)).collect();
    w.env.warp(30);
    for (n, want) in [
        (1usize, SCRIPT_BOUNTY / 8),
        (4, SCRIPT_BOUNTY / 2),
        (8, SCRIPT_BOUNTY),
    ] {
        let list: Vec<(Pubkey, bool)> = holders[..n].iter().map(|h| (h.pubkey(), false)).collect();
        let (extra, idx) = candidates(&mint, &list);
        let pot = companion_of(&w, &mint).pending_pot;
        let entries = idx.iter().map(|i| entry(*i, 0, n as u32)).collect();
        let tx = pay(&mut w, &cranker, &mint, entries, extra);
        tx.ok();
        let ev: ScriptStepped = tx.event();
        assert_eq!((ev.bounty, ev.pot_after), (want, pot - want), "{n} entries");
        assert_eq!(tx.events::<ScriptPayout>().len(), n);
        w.env.warp(60);
    }
    // Sixteen touches, the most an answer carries, are over the runtime's instruction trace (64:
    // four inner instructions a touch on a base of eleven); so are fourteen. Thirteen fit.
    for n in [16usize, 14] {
        let list: Vec<(Pubkey, bool)> = holders[..n].iter().map(|h| (h.pubkey(), false)).collect();
        let (extra, idx) = candidates(&mint, &list);
        let entries = idx.iter().map(|i| entry(*i, 0, 99)).collect();
        let tx = pay(&mut w, &cranker, &mint, entries, extra);
        assert!(
            matches!(
                tx.err(),
                solana_transaction::TransactionError::InstructionError(
                    _,
                    solana_transaction::InstructionError::MaxInstructionTraceLengthExceeded
                )
            ),
            "{n} touches: {:?}",
            tx.err()
        );
    }
    let tx = {
        let list: Vec<(Pubkey, bool)> = holders[..13].iter().map(|h| (h.pubkey(), false)).collect();
        let (extra, idx) = candidates(&mint, &list);
        let entries = idx.iter().map(|i| entry(*i, 0, 98)).collect();
        pay(&mut w, &cranker, &mint, entries, extra)
    };
    tx.ok();
    println!(
        "thirteen touches: CU {} size {} trace {}",
        tx.cu(),
        tx.size,
        tx.trace_len()
    );
    assert_eq!(tx.events::<ScriptPayout>().len(), 13);
    assert_eq!(tx.event::<ScriptStepped>().bounty, SCRIPT_BOUNTY);
    assert!(tx.trace_len() <= 64);
}

/// Payouts need a custom hook that writes hook data: a token with no hook at all, and one with a
/// hook that keeps no hook data (`tax_hook`), are refused (`PayoutsNeedHook`); a hook that writes
/// hook data but has no `record_payout` (the lottery hook) fails the step at the record; the Loss
/// cover hook refuses a holding with no record (`NoRecord`), which fails the step too.
#[test]
fn payouts_need_a_hook_that_records() {
    // No hook at all.
    let mut w = world();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), m, create_args()),
        companion::create_script_v2(launcher.pubkey(), m, script_args(), full_terms(), vec![]),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint]).ok();
    launch_tx(&mut w, &launcher, &mint, None, LaunchRules::NONE).ok();
    assert_eq!(w.launch(&m).custom_hook, None);
    let cranker = fill_pot(&mut w, &m);
    let a = w.wallet_with_sol(2 * SOL);
    w.buy(&a, &m, SOL).ok();
    w.env.warp(30);
    let (extra, idx) = candidates(&m, &[(a.pubkey(), true)]);
    let mut accounts = vec![
        AccountMeta::new(companion::payout_terms_address(&m), false),
        AccountMeta::new_readonly(PAYOUT_AUTHORITY, false),
    ];
    accounts.extend(extra);
    // The candidates start after the two accounts above, not after seven.
    let at = idx[0] - FIRST + 2;
    set_state(
        &mut w,
        &m,
        sp::MODE_ANSWER,
        0,
        &answer(vec![entry(at, 0, 1)]),
    );
    let ix = companion::script_step(cranker.pubkey(), &keys(&w, &m), FIX, accounts);
    refused(
        &w.env.send_paid_by(&[ix], &cranker, &[]),
        CompanionError::PayoutsNeedHook,
    );

    // tax_hook: a custom hook without `WRITES_HOOK_DATA`.
    assert_eq!(tax_hook::FLAGS & token_flags::WRITES_HOOK_DATA, 0);
    let mut w = world();
    let launcher = w.wallet_with_sol(10 * SOL);
    let config = hook_config(&mut w, &launcher, tax_hook::ID, tax_hook::FLAGS);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), m, create_args()),
        companion::create_script_v2(launcher.pubkey(), m, script_args(), full_terms(), vec![]),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint]).ok();
    let collector = Keypair::new().pubkey();
    let tax = Pubkey::find_program_address(&[tax_hook::TAX_SEED, m.as_ref()], &tax_hook::ID).0;
    let prepare = Instruction {
        program_id: tax_hook::ID,
        accounts: vec![
            AccountMeta::new(launcher.pubkey(), true),
            AccountMeta::new_readonly(m, false),
            AccountMeta::new_readonly(collector, false),
            AccountMeta::new(tax, false),
            AccountMeta::new(
                bordrless_hook::hook_accounts_address(&tax_hook::ID, &m).0,
                false,
            ),
            AccountMeta::new_readonly(bordrless_program_tests::env::SYSTEM_PROGRAM_ID, false),
        ],
        data: anchor_lang::InstructionData::data(&tax_hook::instruction::Prepare {
            fee_bps: 100,
            max_wallet_bps: 0,
        }),
    };
    w.env.send_paid_by(&[prepare], &launcher, &[]).ok();
    launch_tx(&mut w, &launcher, &mint, Some(&config), LaunchRules::NONE).ok();
    let payer = w.env.payer.pubkey();
    w.env
        .send(&[token::create_holding(payer, m, collector)], &[])
        .ok();
    let cranker = fill_pot(&mut w, &m);
    let a = w.wallet_with_sol(2 * SOL);
    w.buy(&a, &m, SOL).ok();
    w.env.warp(30);
    let (extra, idx) = candidates(&m, &[(a.pubkey(), true)]);
    let mut accounts = companion::payout_accounts(&m, &tax_hook::ID);
    accounts.extend(extra);
    set_state(
        &mut w,
        &m,
        sp::MODE_ANSWER,
        0,
        &answer(vec![entry(idx[0], 0, 1)]),
    );
    let ix = companion::script_step(cranker.pubkey(), &keys(&w, &m), FIX, accounts);
    refused(
        &w.env.send_paid_by(&[ix], &cranker, &[]),
        CompanionError::PayoutsNeedHook,
    );

    // The lottery hook writes hook data but knows no `record_payout`: the record fails the step.
    let mut w = world();
    let launcher = w.wallet_with_sol(10 * SOL);
    let config = hook_config(&mut w, &launcher, lottery_hook::ID, lottery_hook::FLAGS);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let ixs = [
        companion::create(launcher.pubkey(), launcher.pubkey(), m, create_args()),
        lottery_hook::client::prepare(launcher.pubkey(), m, 3_600),
        companion::create_script_v2(launcher.pubkey(), m, script_args(), full_terms(), vec![]),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint]).ok();
    launch_tx(&mut w, &launcher, &mint, Some(&config), LaunchRules::NONE).ok();
    let cranker = fill_pot(&mut w, &m);
    let a = w.wallet_with_sol(2 * SOL);
    w.buy(&a, &m, SOL).ok();
    w.env.warp(30);
    let (extra, idx) = candidates(&m, &[(a.pubkey(), true)]);
    let mut accounts = companion::payout_accounts(&m, &lottery_hook::ID);
    accounts.extend(extra);
    let pot = companion_of(&w, &m).pending_pot;
    let before = w.env.lamports(&a.pubkey());
    set_state(
        &mut w,
        &m,
        sp::MODE_ANSWER,
        0,
        &answer(vec![entry(idx[0], 1_000, 1)]),
    );
    let ix = companion::script_step(cranker.pubkey(), &keys(&w, &m), FIX, accounts);
    let tx = w.env.send_paid_by(&[ix], &cranker, &[]);
    tx.expect_fail();
    assert_eq!(companion_of(&w, &m).pending_pot, pot);
    assert_eq!(w.env.lamports(&a.pubkey()), before);

    // The Loss cover hook refuses a holding with no record: one that never bought.
    let mut w = world();
    let (_l, mint, cranker, a, _b) = ready(&mut w, Some(full_terms()));
    let empty = w.wallet_with_sol(SOL);
    w.holdings(&empty, mint, &[empty.pubkey()]);
    assert!(!record_of(&w, &mint, &empty.pubkey()).is_set());
    let (extra, idx) = candidates(&mint, &[(a.pubkey(), true), (empty.pubkey(), true)]);
    let tx = pay(
        &mut w,
        &cranker,
        &mint,
        vec![entry(idx[0], 1_000, 1), entry(idx[1], 0, 1)],
        extra,
    );
    tx.expect_code(u32::from(LossCoverError::NoRecord));
    assert_eq!(
        record_of(&w, &mint, &a.pubkey()).round,
        0,
        "nothing recorded"
    );
}

/// A trade and payouts in one step: the pot moves by what the trade spent, the payouts and the
/// bounty, and the tokens stay the creator's.
#[test]
fn a_trade_and_payouts_in_one_step() {
    let mut w = world();
    let (_l, mint, cranker, a, _b) = ready(&mut w, Some(full_terms()));
    let creator = companion::creator_address(&mint);
    let pot = companion_of(&w, &mint).pending_pot;
    let buy = SOL / 50;
    let x = pot / 20;
    let (mut extra, idx) = candidates(&mint, &[(a.pubkey(), true)]);
    extra.extend(buy_accounts(&w, &mint));
    let before = w.env.lamports(&a.pubkey());
    let tx = step(
        &mut w,
        &cranker,
        &mint,
        sp::MODE_BUY_THEN_ANSWER,
        buy,
        &answer(vec![entry(idx[0], x, 5)]),
        extra,
    );
    tx.ok();
    println!(
        "a buy and a payout in one step: CU {} height {} trace {}",
        tx.cu(),
        tx.max_height(),
        tx.trace_len()
    );
    let bounty = SCRIPT_BOUNTY / 8;
    let ev: ScriptStepped = tx.event();
    assert_eq!(ev.sol_before - ev.sol_after, buy);
    assert!(ev.tokens_after > 0);
    assert_eq!((ev.bounty, ev.pot_after), (bounty, pot - buy - x - bounty));
    assert_eq!(w.env.lamports(&a.pubkey()), before + x);
    let c = companion_of(&w, &mint);
    assert_eq!(c.pending_pot, ev.pot_after);
    assert_eq!(w.env.holding(&w.sol, &creator), c.set_aside().unwrap());
    assert_eq!(w.env.holding(&mint, &creator), ev.tokens_after);
    assert_eq!(record_of(&w, &mint, &a.pubkey()).paid, x);
    // The whole pot but the bounty, in one entry: taken; one more lamport: refused.
    w.env.warp(60);
    let pot = c.pending_pot;
    let (extra, idx) = candidates(&mint, &[(a.pubkey(), true)]);
    refused(
        &pay(
            &mut w,
            &cranker,
            &mint,
            vec![entry(idx[0], pot - bounty + 1, 6)],
            extra.clone(),
        ),
        CompanionError::PayoutOverBound,
    );
    pay(
        &mut w,
        &cranker,
        &mint,
        vec![entry(idx[0], pot - bounty, 6)],
        extra,
    )
    .ok();
    let c = companion_of(&w, &mint);
    assert_eq!(c.pending_pot, 0);
    assert_eq!(w.env.holding(&w.sol, &creator), c.set_aside().unwrap());
}

/// A script that calls back into the companion from its step is refused by the runtime: the step
/// fails and nothing moves.
#[test]
fn re_entry_is_impossible() {
    let mut w = world();
    let (_l, mint, cranker, a, _b) = ready(&mut w, Some(full_terms()));
    let pot = companion_of(&w, &mint).pending_pot;
    let last_at = w
        .env
        .read::<Script>(&companion::script_address(&mint))
        .last_at;
    let (mut extra, _) = candidates(&mint, &[(a.pubkey(), true)]);
    extra.push(AccountMeta::new_readonly(bordrless_companion::ID, false));
    let data = companion::script_step(cranker.pubkey(), &keys(&w, &mint), FIX, vec![]).data;
    let tx = step(&mut w, &cranker, &mint, sp::MODE_REENTER, 0, &data, extra);
    tx.expect_fail();
    assert!(
        tx.logs()
            .iter()
            .any(|l| l.contains("reentrancy") || l.contains("Reentrancy")),
        "{}",
        tx.logs().join("\n")
    );
    assert_eq!(companion_of(&w, &mint).pending_pot, pot);
    assert_eq!(
        w.env
            .read::<Script>(&companion::script_address(&mint))
            .last_at,
        last_at
    );
}

/// The payout authority and the record's accounts are the ones the hook takes: a step without the
/// hook's accounts can't record (`MissingAccount`), and the terms must be passed writable.
#[test]
fn a_step_needs_the_records_accounts() {
    let mut w = world();
    let (_l, mint, cranker, a, _b) = ready(&mut w, Some(full_terms()));
    let (extra, idx) = candidates(&mint, &[(a.pubkey(), true)]);
    let entries = answer(vec![entry(idx[0], 0, 1)]);
    // Each of the hook's accounts left out: the record can't be built (the token program and its
    // event authority travel with the creator holding's accounts too, so those two are not tried).
    for left_out in 1..5 {
        set_state(&mut w, &mint, sp::MODE_ANSWER, 0, &entries);
        let mut accounts = companion::payout_accounts(&mint, &loss_cover_hook::ID);
        let missing = accounts.remove(left_out);
        // Keep the indices: an unused address in its place.
        accounts.insert(
            left_out,
            AccountMeta::new_readonly(Pubkey::new_unique(), false),
        );
        accounts.extend(extra.clone());
        let ix = companion::script_step(cranker.pubkey(), &keys(&w, &mint), FIX, accounts);
        let tx = w.env.send_paid_by(&[ix], &cranker, &[]);
        refused(&tx, CompanionError::MissingAccount);
        println!("without {}: refused", missing.pubkey);
    }
    // The terms read-only: refused.
    set_state(&mut w, &mint, sp::MODE_ANSWER, 0, &entries);
    let mut accounts = companion::payout_accounts(&mint, &loss_cover_hook::ID);
    accounts[0].is_writable = false;
    accounts.extend(extra.clone());
    let ix = companion::script_step(cranker.pubkey(), &keys(&w, &mint), FIX, accounts);
    refused(
        &w.env.send_paid_by(&[ix], &cranker, &[]),
        CompanionError::MissingAccount,
    );
    // The terms left out: `PayoutsOff`.
    set_state(&mut w, &mint, sp::MODE_ANSWER, 0, &entries);
    let mut accounts = companion::payout_accounts(&mint, &loss_cover_hook::ID);
    accounts[0] = AccountMeta::new(Pubkey::new_unique(), false);
    accounts.extend(extra);
    let ix = companion::script_step(cranker.pubkey(), &keys(&w, &mint), FIX, accounts);
    refused(
        &w.env.send_paid_by(&[ix], &cranker, &[]),
        CompanionError::PayoutsOff,
    );
}
