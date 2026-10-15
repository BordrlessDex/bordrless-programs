//! `set_payout_terms` and the window measured against the live pot (2026-10-15): the launch's
//! beneficiary (its launcher) or the script approver replaces a script's payout terms after the
//! launch, within `create_script_v2`'s bounds, and the window restarts; anyone else, or terms out
//! of bounds, refused. A window's share is of the pot as if the window had paid nothing (the live
//! pot plus what it paid, the pot at its first step a floor), so fees arriving mid-window are
//! payable in it. Tenure's case (10% a step, 25% a day): a script paying every round, starved by
//! the frozen cap, is paid again. Bridged and native companions alike.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::AnchorSerialize;
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateScriptArgs, PayoutTermsArgs};
use bordrless_companion::state::{Companion, PayoutAnswer, PayoutEntry, PayoutTerms, Split};
use bordrless_core::policy;
use bordrless_launch::client::{self as launch, LaunchKeys};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::timelock::{programdata_hash, send_as};
use bordrless_token::client as token;
use loss_cover_hook::{client as hook, Record};
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

/// Tenure's terms: 10% of the pot a step, all of a step to one holding, 25% a day.
fn tenure_terms() -> PayoutTermsArgs {
    PayoutTermsArgs {
        step_bps: 1_000,
        holding_bps: 10_000,
        window_bps: 2_500,
        window_secs: 86_400,
    }
}

/// A coin ready to step under `terms`, migrated native when `native`.
fn coin(
    w: &mut World,
    terms: PayoutTermsArgs,
    native: bool,
) -> (Keypair, Pubkey, Keypair, Keypair, Keypair) {
    let r = ready(w, Some(terms));
    if native {
        w.migrate_companion_native(&r.1).ok();
        assert!(companion_of(w, &r.1).is_native());
    } else {
        assert!(!companion_of(w, &r.1).is_native());
    }
    r
}

fn set_terms(w: &mut World, signer: &Keypair, mint: &Pubkey, terms: PayoutTermsArgs) -> Tx {
    w.env.send_paid_by(
        &[companion::set_payout_terms(signer.pubkey(), *mint, terms)],
        signer,
        &[],
    )
}

/// What the window may still pay: `window_bps` of the live pot plus what it paid (the pot at its
/// first step a floor), less what it paid.
fn window_left(w: &World, mint: &Pubkey) -> u64 {
    let t = terms_of(w, mint);
    let pot = companion_of(w, mint).pending_pot;
    let base = (pot + t.window_paid).max(t.window_pot);
    (base * u64::from(t.window_bps) / 10_000).saturating_sub(t.window_paid)
}

/// More fees into the pot mid-window (trades and a claim), the window unchanged.
fn more_fees(w: &mut World, mint: &Pubkey) -> u64 {
    let before = companion_of(w, mint).pending_pot;
    let start = terms_of(w, mint).window_start;
    fill_pot(w, mint);
    w.env.warp(30);
    assert_eq!(terms_of(w, mint).window_start_at(w.env.now), start);
    companion_of(w, mint).pending_pot - before
}

#[test]
fn the_beneficiary_sets_the_terms_and_the_window_restarts() {
    for native in [false, true] {
        let mut w = world();
        let (launcher, mint, cranker, a, _b) = coin(&mut w, tenure_terms(), native);
        let c = companion_of(&w, &mint);
        // The beneficiary is the launcher's wallet, not the companion's creator address.
        assert_eq!(c.beneficiary, launcher.pubkey());
        assert_ne!(c.beneficiary, companion::creator_address(&mint));
        // A payout first, so the counters have something to keep.
        let (extra, idx) = candidates(&mint, &[(a.pubkey(), true)]);
        pay(
            &mut w,
            &cranker,
            &mint,
            vec![entry(idx[0], SOL / 100, 1)],
            extra,
        )
        .ok();
        let before = terms_of(&w, &mint);
        assert_eq!(before.window_paid, SOL / 100);
        assert!(before.window_start > 0 && before.window_pot > 0);

        let new = PayoutTermsArgs {
            step_bps: 2_000,
            holding_bps: 5_000,
            window_bps: 10_000,
            window_secs: 300,
        };
        let tx = set_terms(&mut w, &launcher, &mint, new);
        tx.ok();
        let e: PayoutTermsSet = tx.event();
        assert_eq!(e.companion, companion::companion_address(&mint));
        assert_eq!(e.mint, mint);
        assert_eq!(e.terms, companion::payout_terms_address(&mint));
        assert_eq!(
            (e.step_bps, e.holding_bps, e.window_bps, e.window_secs),
            (2_000, 5_000, 10_000, 300)
        );
        let t = terms_of(&w, &mint);
        assert_eq!(
            (t.step_bps, t.holding_bps, t.window_bps, t.window_secs),
            (2_000, 5_000, 10_000, 300)
        );
        // The window restarts; the record of what was paid, the mint, the bump, the flags stay.
        assert_eq!((t.window_start, t.window_pot, t.window_paid), (0, 0, 0));
        assert_eq!(
            (t.paid_total, t.payouts, t.last_paid_at, t.reserved),
            (
                before.paid_total,
                before.payouts,
                before.last_paid_at,
                before.reserved
            )
        );
        assert_eq!(
            (t.mint, t.bump, t.version),
            (before.mint, before.bump, before.version)
        );
        // The data is the same size: nothing moved.
        assert_eq!(
            w.env
                .account(&companion::payout_terms_address(&mint))
                .unwrap()
                .data
                .len(),
            PayoutTerms::LEN
        );
        // The next step opens a window under the new terms: 20% of the pot, half to a holding.
        w.env.warp(61);
        let pot = companion_of(&w, &mint).pending_pot;
        let budget = pot * 2_000 / 10_000;
        let cap = budget * 5_000 / 10_000;
        let (extra, idx) = candidates(&mint, &[(a.pubkey(), true)]);
        refused(
            &pay(
                &mut w,
                &cranker,
                &mint,
                vec![entry(idx[0], cap + 1, 2)],
                extra.clone(),
            ),
            CompanionError::PayoutOverBound,
        );
        pay(&mut w, &cranker, &mint, vec![entry(idx[0], cap, 2)], extra).ok();
        let t = terms_of(&w, &mint);
        assert_eq!(t.window_paid, cap);
        assert_eq!(t.window_start, w.env.now - w.env.now % 300);
        assert_eq!(t.paid_total, before.paid_total + cap);
    }
}

#[test]
fn the_script_approver_sets_the_terms() {
    let mut w = world();
    let (_l, mint, _c, _a, _b) = coin(&mut w, tenure_terms(), false);
    w.env.without_sigverify();
    w.env.fund(SCRIPT_APPROVER, SOL);
    let terms = PayoutTermsArgs {
        window_bps: 10_000,
        ..tenure_terms()
    };
    let tx = send_as(
        &mut w.env,
        &[companion::set_payout_terms(SCRIPT_APPROVER, mint, terms)],
        &[],
        &[SCRIPT_APPROVER],
    );
    tx.ok();
    let e: PayoutTermsSet = tx.event();
    assert_eq!(e.window_bps, 10_000);
    assert_eq!(terms_of(&w, &mint).window_bps, 10_000);
}

#[test]
fn anyone_else_is_refused() {
    let mut w = world();
    let (launcher, mint, cranker, a, _b) = coin(&mut w, tenure_terms(), false);
    let before = terms_of(&w, &mint);
    let loose = PayoutTermsArgs {
        window_bps: 10_000,
        ..tenure_terms()
    };
    // A stranger, a holder, a cranker.
    let stranger = w.wallet_with_sol(SOL);
    for k in [&stranger, &a, &cranker] {
        refused(
            &set_terms(&mut w, k, &mint, loose),
            CompanionError::NotTermsAuthority,
        );
    }
    // The approver's key unsigned, the beneficiary's unsigned: no signature, no change.
    let mut ix = companion::set_payout_terms(launcher.pubkey(), mint, loose);
    ix.accounts[0].is_signer = false;
    assert!(w.env.send_paid_by(&[ix], &stranger, &[]).result.is_err());
    // The beneficiary of another coin is nobody here.
    let other = w.wallet_with_sol(10 * SOL);
    let _other_mint = launch_coin(&mut w, &other, Some(tenure_terms()));
    refused(
        &set_terms(&mut w, &other, &mint, loose),
        CompanionError::NotTermsAuthority,
    );
    // A companion without terms (create_script): nothing to set.
    let plain = w.wallet_with_sol(10 * SOL);
    let plain_mint = launch_coin(&mut w, &plain, None);
    assert!(set_terms(&mut w, &plain, &plain_mint, loose)
        .result
        .is_err());
    let t = terms_of(&w, &mint);
    assert_eq!(
        (t.step_bps, t.holding_bps, t.window_bps, t.window_secs),
        (
            before.step_bps,
            before.holding_bps,
            before.window_bps,
            before.window_secs
        )
    );
}

#[test]
fn terms_out_of_bounds_are_refused() {
    let mut w = world();
    let (launcher, mint, _c, _a, _b) = coin(&mut w, tenure_terms(), false);
    let ok = tenure_terms();
    for bad in [
        PayoutTermsArgs { step_bps: 0, ..ok },
        PayoutTermsArgs {
            step_bps: 10_001,
            ..ok
        },
        PayoutTermsArgs {
            holding_bps: 0,
            ..ok
        },
        PayoutTermsArgs {
            holding_bps: 10_001,
            ..ok
        },
        PayoutTermsArgs {
            window_bps: 0,
            ..ok
        },
        PayoutTermsArgs {
            window_bps: 10_001,
            ..ok
        },
        PayoutTermsArgs {
            window_secs: 9,
            ..ok
        },
        PayoutTermsArgs {
            window_secs: 86_401,
            ..ok
        },
    ] {
        refused(
            &set_terms(&mut w, &launcher, &mint, bad),
            CompanionError::BadPayoutTerms,
        );
    }
    // The bounds themselves pass.
    for good in [
        PayoutTermsArgs {
            step_bps: 1,
            holding_bps: 1,
            window_bps: 1,
            window_secs: 10,
        },
        PayoutTermsArgs {
            step_bps: 10_000,
            holding_bps: 10_000,
            window_bps: 10_000,
            window_secs: 86_400,
        },
    ] {
        set_terms(&mut w, &launcher, &mint, good).ok();
        w.env.warp(1);
    }
}

/// Fees that arrive mid-window are payable in it: the window's share is of the live pot plus what
/// it paid, so paying never shrinks it and fees grow it; one lamport over is refused.
#[test]
fn the_window_is_measured_against_the_live_pot() {
    for native in [false, true] {
        let mut w = world();
        let terms = PayoutTermsArgs {
            window_bps: 2_500,
            window_secs: 86_400,
            ..full_terms()
        };
        let (_l, mint, cranker, a, _b) = coin(&mut w, terms, native);
        let pot0 = companion_of(&w, &mint).pending_pot;
        let (extra, idx) = candidates(&mint, &[(a.pubkey(), true)]);
        // The first step: 25% of the pot.
        let first = pot0 * 2_500 / 10_000;
        pay(
            &mut w,
            &cranker,
            &mint,
            vec![entry(idx[0], first, 1)],
            extra.clone(),
        )
        .ok();
        let t = terms_of(&w, &mint);
        assert_eq!((t.window_pot, t.window_paid), (pot0, first));
        // The window is spent: the pot less the bounty, plus what it paid, is under its start.
        w.env.warp(61);
        assert_eq!(window_left(&w, &mint), 0);
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
        // Fees arrive mid-window: a quarter of them is payable now, in the same window.
        let fees = more_fees(&mut w, &mint);
        assert!(fees > 0);
        let left = window_left(&w, &mint);
        assert!(left > 0 && left <= fees / 4 + 1, "{left} of {fees}");
        let pot = companion_of(&w, &mint).pending_pot;
        refused(
            &pay(
                &mut w,
                &cranker,
                &mint,
                vec![entry(idx[0], left + 1, 3)],
                extra.clone(),
            ),
            CompanionError::PayoutOverBound,
        );
        pay(
            &mut w,
            &cranker,
            &mint,
            vec![entry(idx[0], left, 3)],
            extra.clone(),
        )
        .ok();
        let t = terms_of(&w, &mint);
        assert_eq!(t.window_paid, first + left);
        assert!(companion_of(&w, &mint).pending_pot < pot);
        // Never over a quarter of everything the window held.
        assert!(t.window_paid <= (pot0 + fees) * 2_500 / 10_000);
        // Spent again until more fees come.
        w.env.warp(61);
        assert_eq!(window_left(&w, &mint), 0);
    }
}

/// Tenure: 10% a step, 25% a day, a payout every round. Under the frozen cap the day's first
/// payout fixed the window and later fees waited for tomorrow; now each round's fees are payable
/// that round, and the beneficiary may loosen the window at once.
#[test]
fn tenure_is_paid_every_round() {
    for native in [false, true] {
        let mut w = world();
        let (launcher, mint, cranker, a, b) = coin(&mut w, tenure_terms(), native);
        let (extra, idx) = candidates(&mint, &[(a.pubkey(), true), (b.pubkey(), true)]);
        let pot0 = companion_of(&w, &mint).pending_pot;
        let frozen = pot0 * 2_500 / 10_000;
        let mut paid = 0u64;
        // Rounds: each pays what the step and the window allow, then fees arrive.
        for round in 0..6u32 {
            let pot = companion_of(&w, &mint).pending_pot;
            let bounty = SCRIPT_BOUNTY * 2 / 8;
            let step = (pot * 1_000 / 10_000).min(pot - bounty);
            let budget = step.min(window_left(&w, &mint));
            if budget >= 2 {
                let half = budget / 2;
                pay(
                    &mut w,
                    &cranker,
                    &mint,
                    vec![
                        entry(idx[0], half, round),
                        entry(idx[1], budget - half, round),
                    ],
                    extra.clone(),
                )
                .ok();
                paid += budget;
            }
            more_fees(&mut w, &mint);
            w.env.warp(61);
        }
        let t = terms_of(&w, &mint);
        assert_eq!(t.window_paid, paid);
        // More than the frozen cap ever allowed in the day.
        assert!(paid > frozen, "{paid} <= {frozen}");
        // And the beneficiary lifts the day's cap: the window restarts, the step's 10% binds.
        set_terms(
            &mut w,
            &launcher,
            &mint,
            PayoutTermsArgs {
                window_bps: 10_000,
                ..tenure_terms()
            },
        )
        .ok();
        w.env.warp(61);
        let pot = companion_of(&w, &mint).pending_pot;
        let step = pot * 1_000 / 10_000;
        let half = step / 2;
        refused(
            &pay(
                &mut w,
                &cranker,
                &mint,
                vec![entry(idx[0], half, 9), entry(idx[1], step - half + 1, 9)],
                extra.clone(),
            ),
            CompanionError::PayoutOverBound,
        );
        pay(
            &mut w,
            &cranker,
            &mint,
            vec![entry(idx[0], half, 9), entry(idx[1], step - half, 9)],
            extra,
        )
        .ok();
        let t = terms_of(&w, &mint);
        assert_eq!(t.window_paid, step);
        assert_eq!(t.window_pot, pot);
    }
}
