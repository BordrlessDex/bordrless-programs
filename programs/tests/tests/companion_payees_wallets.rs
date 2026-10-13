//! Payees and wallet payouts (2026-10-13).
//!
//! - **Payees:** up to 10 wallets share a companion's beneficiary part, each a fixed share, set once
//!   before the launch with the mint's signature (`set_payees`); every `withdraw` pays them as SOL
//!   and the beneficiary the rest; a payee that can't take its part yet is owed it, and nothing is
//!   lost; a `withdraw` without the payees is refused, so nobody pays the beneficiary around them.
//! - **Wallet payouts:** a script with payout terms answers `PayoutAnswer` version 2, naming the
//!   accounts paid themselves (any wallet), and the companion pays them SOL from the pot within the
//!   same terms, with no hook (a plain coin), and never the protocol's accounts.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::AnchorSerialize;
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateScriptArgs, PayeeArgs, PayoutTermsArgs};
use bordrless_companion::state::{
    Companion, Payees, PayoutTerms, Split, WalletPayoutAnswer, WalletPayoutEntry,
};
use bordrless_core::policy;
use bordrless_launch::client::{self as launch, LaunchKeys};
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_program_tests::program_bytes;
use bordrless_program_tests::timelock::programdata_hash;
use script_payouts as sp;
use solana_keypair::Keypair;
use solana_signer::Signer;

const CREATOR_FEE: u16 = 200;
const FIX: Pubkey = sp::ID;
const BORDRLESS_KEY: Pubkey = bordrless_launch::constants::HOOK_UPGRADE_AUTHORITIES[1];

#[track_caller]
fn refused(tx: &Tx, e: CompanionError) {
    tx.expect_code(u32::from(e));
    let name = format!("{e:?}");
    assert!(
        tx.logs()
            .iter()
            .any(|l| l.contains(&format!("Error Code: {name}"))),
        "expected {name}\n{}",
        tx.logs().join("\n")
    );
}

fn create_args(split: Split) -> CreateArgs {
    CreateArgs {
        split,
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL / 2,
    }
}

fn keys(w: &World, mint: &Pubkey) -> LaunchKeys {
    LaunchKeys::of(&w.launch(mint))
}

fn companion_of(w: &World, mint: &Pubkey) -> Companion {
    w.env.read(&companion::companion_address(mint))
}

/// The launch through `mint`'s companion (made before), inline rules, no hook.
fn launch_tx(w: &mut World, launcher: &Keypair, mint: &Keypair) -> Tx {
    let creator = companion::creator_address(&mint.pubkey());
    let launch_args = World::launch_args("PAY", CREATOR_FEE, VQ, LaunchRules::NONE);
    let inner = launch::create_launch_with(
        creator,
        mint.pubkey(),
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        launch_args.clone(),
        None,
        None,
    );
    let ix = companion::launch(launcher.pubkey(), mint.pubkey(), &inner, launch_args);
    w.env.send_paid_by(&[ix], launcher, &[mint])
}

fn trade(w: &mut World, mint: &Pubkey, wallets: usize, sol: u64) {
    for _ in 0..wallets {
        let t = w.wallet_with_sol(sol + SOL);
        w.buy(&t, mint, sol).ok();
        let held = w.env.holding(mint, &t.pubkey());
        w.sell(&t, mint, held / 4).ok();
    }
}

fn claim(w: &mut World, mint: &Pubkey) {
    let cranker = w.wallet_with_sol(SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees(cranker.pubkey(), *mint, None)],
            &cranker,
            &[],
        )
        .ok();
}

/// The beneficiary's whole part, before any payee.
const ALL_TO_BENEFICIARY: Split = Split {
    buyback_bps: 0,
    holders_bps: 0,
    beneficiary_bps: 10_000,
};

/// A classic companion whose beneficiary part is shared with `payees`, launched. Answers the
/// launcher (the beneficiary) and the mint.
fn coin_with_payees(w: &mut World, payees: &[PayeeArgs]) -> (Keypair, Pubkey) {
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            create_args(ALL_TO_BENEFICIARY),
        ),
        companion::set_payees(launcher.pubkey(), m, payees.to_vec()),
    ];
    let tx = w.env.send_paid_by(&ixs, &launcher, &[&mint]);
    tx.ok();
    let ev = tx.event::<PayeesSet>();
    assert_eq!(ev.payees.len(), payees.len());
    assert_eq!(companion_of(w, &m).version, VERSION_WITH_PAYEES);
    launch_tx(w, &launcher, &mint).ok();
    (launcher, m)
}

#[test]
fn payees_share_the_beneficiary_part_and_the_beneficiary_keeps_the_rest() {
    let mut w = World::new();
    let (team, treasury) = (Keypair::new(), Keypair::new());
    for k in [&team, &treasury] {
        w.env.fund(k.pubkey(), SOL);
    }
    let payees = [
        PayeeArgs {
            wallet: team.pubkey(),
            bps: 3_000,
        },
        PayeeArgs {
            wallet: treasury.pubkey(),
            bps: 2_000,
        },
    ];
    let (launcher, mint) = coin_with_payees(&mut w, &payees);
    w.env.warp(31);
    trade(&mut w, &mint, 4, 2 * SOL);
    claim(&mut w, &mint);
    let pending = companion_of(&w, &mint).pending_beneficiary;
    assert!(pending > 0);

    // A withdraw without the payees: refused, nothing moves.
    let sender = w.wallet_with_sol(SOL);
    let ix = companion::withdraw(sender.pubkey(), mint, launcher.pubkey());
    refused(
        &w.env.send_paid_by(&[ix], &sender, &[]),
        CompanionError::PayeesMissing,
    );

    // With them: 30% and 20% of the part, the beneficiary the rest and the launch's spare SOL.
    let before = [team.pubkey(), treasury.pubkey(), launcher.pubkey()].map(|k| w.env.lamports(&k));
    let ix = companion::withdraw_with_payees(
        sender.pubkey(),
        mint,
        launcher.pubkey(),
        &[team.pubkey(), treasury.pubkey()],
    );
    let tx = w.env.send_paid_by(&[ix], &sender, &[]);
    tx.ok();
    let (a, b) = (pending * 3_000 / 10_000, pending * 2_000 / 10_000);
    assert_eq!(w.env.lamports(&team.pubkey()) - before[0], a);
    assert_eq!(w.env.lamports(&treasury.pubkey()) - before[1], b);
    let paid: BeneficiaryPaid = tx.event();
    assert_eq!(
        w.env.lamports(&launcher.pubkey()) - before[2],
        paid.lamports
    );
    assert!(paid.lamports >= pending - a - b);
    let c = companion_of(&w, &mint);
    assert_eq!(c.pending_beneficiary, 0);
    let p: Payees = w.env.read(&companion::payees_address(&mint));
    assert_eq!((p.payees[0].paid, p.payees[1].paid, p.owed()), (a, b, 0));
    println!(
        "withdraw with two payees: CU {} size {} height {}",
        tx.cu(),
        tx.size,
        tx.max_height()
    );
}

#[test]
fn a_payee_that_cant_take_its_part_is_owed_it_and_paid_later() {
    let mut w = World::new();
    // A fresh wallet (no lamports): a small part would leave it below its rent, so it is owed.
    let (fresh, steady) = (Keypair::new(), Keypair::new());
    w.env.fund(steady.pubkey(), SOL);
    let payees = [
        PayeeArgs {
            wallet: fresh.pubkey(),
            bps: 100,
        },
        PayeeArgs {
            wallet: steady.pubkey(),
            bps: 5_000,
        },
    ];
    let (launcher, mint) = coin_with_payees(&mut w, &payees);
    w.env.warp(31);
    trade(&mut w, &mint, 2, SOL / 2);
    claim(&mut w, &mint);
    let pending = companion_of(&w, &mint).pending_beneficiary;
    let fresh_part = pending / 100;
    assert!(fresh_part < w.env.rent(0), "the part is under the rent");
    let sender = w.wallet_with_sol(SOL);
    let ix = companion::withdraw_with_payees(
        sender.pubkey(),
        mint,
        launcher.pubkey(),
        &[fresh.pubkey(), steady.pubkey()],
    );
    let tx = w.env.send_paid_by(&[ix], &sender, &[]);
    tx.ok();
    assert_eq!(w.env.lamports(&fresh.pubkey()), 0);
    let c = companion_of(&w, &mint);
    assert_eq!(
        c.pending_beneficiary, fresh_part,
        "kept for the fresh payee"
    );
    let p: Payees = w.env.read(&companion::payees_address(&mint));
    assert_eq!(p.payees[0].owed, fresh_part);
    // The steady payee was paid all the same; the beneficiary its rest (not the owed part).
    assert_eq!(p.payees[1].paid, pending * 5_000 / 10_000);

    // Leaving a payee out: it is owed too, never paid to anyone else.
    trade(&mut w, &mint, 2, SOL / 2);
    claim(&mut w, &mint);
    let pending2 = companion_of(&w, &mint).pending_beneficiary;
    let new = pending2 - fresh_part;
    let ix = companion::withdraw_with_payees(
        sender.pubkey(),
        mint,
        launcher.pubkey(),
        &[steady.pubkey()],
    );
    w.env.send_paid_by(&[ix], &sender, &[]).ok();
    let p: Payees = w.env.read(&companion::payees_address(&mint));
    assert_eq!(p.payees[0].owed, fresh_part + new / 100);
    assert_eq!(
        companion_of(&w, &mint).pending_beneficiary,
        p.payees[0].owed
    );

    // Once its account can take it, everything owed is paid at once.
    w.env.fund(fresh.pubkey(), SOL);
    let owed = p.payees[0].owed;
    w.env.warp(1);
    trade(&mut w, &mint, 1, SOL / 4);
    claim(&mut w, &mint);
    let pending3 = companion_of(&w, &mint).pending_beneficiary;
    let ix = companion::withdraw_with_payees(
        sender.pubkey(),
        mint,
        launcher.pubkey(),
        &[fresh.pubkey(), steady.pubkey()],
    );
    w.env.send_paid_by(&[ix], &sender, &[]).ok();
    assert_eq!(
        w.env.lamports(&fresh.pubkey()),
        SOL + owed + (pending3 - owed) / 100
    );
    assert_eq!(companion_of(&w, &mint).pending_beneficiary, 0);
}

#[test]
fn payees_are_set_once_before_the_launch_by_the_mint() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    w.env
        .send_paid_by(
            &[companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                m,
                create_args(ALL_TO_BENEFICIARY),
            )],
            &launcher,
            &[&mint],
        )
        .ok();
    let p = |wallet: Pubkey, bps: u16| PayeeArgs { wallet, bps };
    let x = Pubkey::new_unique();
    // Bad lists: refused.
    for bad in [
        vec![],
        vec![p(x, 0)],
        vec![p(x, 6_000), p(Pubkey::new_unique(), 5_000)],
        vec![p(x, 1), p(x, 1)],
        vec![p(launcher.pubkey(), 1)],
        vec![p(companion::creator_address(&m), 1)],
        (0..11).map(|_| p(Pubkey::new_unique(), 1)).collect(),
    ] {
        let tx = w.env.send_paid_by(
            &[companion::set_payees(launcher.pubkey(), m, bad)],
            &launcher,
            &[&mint],
        );
        refused(&tx, CompanionError::BadPayees);
    }
    // Without the mint's signature: no transaction can be signed.
    let mut ix = companion::set_payees(launcher.pubkey(), m, vec![p(x, 1)]);
    ix.accounts[1].is_signer = false;
    w.env.send_paid_by(&[ix], &launcher, &[]).expect_fail();
    // Ten distinct payees summing to all of it: fine, once.
    let ten: Vec<PayeeArgs> = (0..10).map(|_| p(Pubkey::new_unique(), 1_000)).collect();
    w.env
        .send_paid_by(
            &[companion::set_payees(launcher.pubkey(), m, ten.clone())],
            &launcher,
            &[&mint],
        )
        .ok();
    w.env
        .send_paid_by(
            &[companion::set_payees(launcher.pubkey(), m, vec![p(x, 1)])],
            &launcher,
            &[&mint],
        )
        .expect_fail();
    // After the launch: refused.
    launch_tx(&mut w, &launcher, &mint).ok();
    let other = Keypair::new();
    w.env
        .send_paid_by(
            &[companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                other.pubkey(),
                create_args(ALL_TO_BENEFICIARY),
            )],
            &launcher,
            &[&other],
        )
        .ok();
    launch_tx(&mut w, &launcher, &other).ok();
    let tx = w.env.send_paid_by(
        &[companion::set_payees(
            launcher.pubkey(),
            other.pubkey(),
            vec![p(x, 1)],
        )],
        &launcher,
        &[&other],
    );
    refused(&tx, CompanionError::AlreadyLaunched);
    // A companion without payees withdraws as before.
    w.env.warp(31);
    trade(&mut w, &other.pubkey(), 2, SOL);
    claim(&mut w, &other.pubkey());
    let ix = companion::withdraw(launcher.pubkey(), other.pubkey(), launcher.pubkey());
    w.env.send_paid_by(&[ix], &launcher, &[]).ok();
    assert_eq!(companion_of(&w, &other.pubkey()).pending_beneficiary, 0);
}

// ---- wallet payouts ---------------------------------------------------------------------------------

fn script_world() -> World {
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

fn terms(step_bps: u16, holding_bps: u16) -> PayoutTermsArgs {
    PayoutTermsArgs {
        step_bps,
        holding_bps,
        window_bps: 10_000,
        window_secs: 300,
    }
}

/// A plain coin (no custom hook) whose companion runs the fixture script with `t`, wallet payouts
/// allowed before the launch when `wallets`, its pot filled.
fn script_coin_with(w: &mut World, t: PayoutTermsArgs, wallets: bool) -> (Keypair, Pubkey) {
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let mut ixs = vec![
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            create_args(ALL_TO_BENEFICIARY),
        ),
        companion::create_script_v2(launcher.pubkey(), m, script_args(), t, vec![]),
    ];
    if wallets {
        ixs.push(companion::allow_wallet_payouts(m));
    }
    let tx = w.env.send_paid_by(&ixs, &launcher, &[&mint]);
    tx.ok();
    if wallets {
        assert_eq!(tx.events::<WalletPayoutsAllowed>().len(), 1);
    }
    launch_tx(w, &launcher, &mint).ok();
    w.env.warp(31);
    trade(w, &m, 4, 2 * SOL);
    let cranker = w.wallet_with_sol(SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees_script(cranker.pubkey(), m, None)],
            &cranker,
            &[],
        )
        .ok();
    w.env.warp(61);
    (launcher, m)
}

fn script_coin(w: &mut World, t: PayoutTermsArgs) -> (Keypair, Pubkey) {
    script_coin_with(w, t, true)
}

fn wallets_answer(entries: Vec<(Pubkey, u64, u32)>) -> Vec<u8> {
    let mut out = Vec::new();
    WalletPayoutAnswer {
        version: PAYOUT_ANSWER_WALLETS_VERSION,
        entries: entries
            .into_iter()
            .map(|(to, lamports, tag)| WalletPayoutEntry { to, lamports, tag })
            .collect(),
    }
    .serialize(&mut out)
    .unwrap();
    out
}

fn set_state(w: &mut World, mint: &Pubkey, answer: &[u8]) {
    let key = sp::state_address(mint);
    let mut a = w.env.account(&key).expect("the script's state");
    let bytes = sp::state_bytes(sp::MODE_ANSWER, 0, answer);
    a.data[..bytes.len()].copy_from_slice(&bytes);
    w.env.put(key, a);
}

/// A step answering `answer`, the remaining accounts the terms then `to`.
fn wallet_step(
    w: &mut World,
    cranker: &Keypair,
    mint: &Pubkey,
    answer: &[u8],
    to: Vec<AccountMeta>,
) -> Tx {
    set_state(w, mint, answer);
    let mut accounts = vec![AccountMeta::new(
        companion::payout_terms_address(mint),
        false,
    )];
    accounts.extend(to);
    let ix = companion::script_step(cranker.pubkey(), &keys(w, mint), FIX, accounts);
    w.env.send_paid_by(&[ix], cranker, &[])
}

#[test]
fn a_script_pays_any_wallet_sol_within_its_terms_with_no_hook() {
    let mut w = script_world();
    let (_launcher, mint) = script_coin(&mut w, terms(5_000, 5_000));
    assert_eq!(w.launch(&mint).custom_hook, None);
    let pot = companion_of(&w, &mint).pending_pot;
    assert!(pot > 10 * SCRIPT_BOUNTY);
    let cranker = w.wallet_with_sol(SOL);
    // A funded wallet and a program-derived address that already holds rent (a multisig vault, say).
    let (alice, vault) = (
        Keypair::new(),
        Pubkey::find_program_address(&[b"vault"], &Pubkey::new_unique()).0,
    );
    w.env.fund(alice.pubkey(), SOL);
    w.env.fund(vault, w.env.rent(0));
    let x = pot / 10;
    let tx = wallet_step(
        &mut w,
        &cranker,
        &mint,
        &wallets_answer(vec![(alice.pubkey(), x, 7), (vault, x / 2, 8)]),
        vec![
            AccountMeta::new(alice.pubkey(), false),
            AccountMeta::new(vault, false),
        ],
    );
    tx.ok();
    assert_eq!(w.env.lamports(&alice.pubkey()), SOL + x);
    assert_eq!(w.env.lamports(&vault), w.env.rent(0) + x / 2);
    let evs = tx.events::<ScriptWalletPayout>();
    assert_eq!(
        evs.iter()
            .map(|e| (e.to, e.lamports, e.tag))
            .collect::<Vec<_>>(),
        vec![(alice.pubkey(), x, 7), (vault, x / 2, 8)]
    );
    let t: PayoutTerms = w.env.read(&companion::payout_terms_address(&mint));
    assert_eq!((t.paid_total, t.payouts), (x + x / 2, 2));
    let c = companion_of(&w, &mint);
    assert_eq!(c.pending_pot, pot - x - x / 2 - SCRIPT_BOUNTY * 2 / 8);
    println!(
        "wallet payout step, two wallets: CU {} size {} height {}",
        tx.cu(),
        tx.size,
        tx.max_height()
    );
}

#[test]
fn a_wallet_payout_keeps_to_its_terms_and_never_pays_the_protocols_accounts() {
    let mut w = script_world();
    let (launcher, mint) = script_coin(&mut w, terms(2_000, 5_000));
    let pot = companion_of(&w, &mint).pending_pot;
    let cranker = w.wallet_with_sol(SOL);
    let alice = Keypair::new();
    w.env.fund(alice.pubkey(), SOL);
    let a = || vec![AccountMeta::new(alice.pubkey(), false)];
    let budget = pot * 2_000 / 10_000;
    // Over the recipient's cap (half the step's budget), or the step's budget: refused.
    let tx = wallet_step(
        &mut w,
        &cranker,
        &mint,
        &wallets_answer(vec![(alice.pubkey(), budget / 2 + 1, 0)]),
        a(),
    );
    refused(&tx, CompanionError::PayoutOverBound);
    // Zero, or twice the same account: malformed.
    for bad in [
        vec![(alice.pubkey(), 0, 0)],
        vec![(alice.pubkey(), 1, 0), (alice.pubkey(), 1, 0)],
    ] {
        let tx = wallet_step(&mut w, &cranker, &mint, &wallets_answer(bad), a());
        refused(&tx, CompanionError::BadPayoutAnswer);
    }
    // The payee named but not passed (a sender putting its own wallet in its place): refused.
    let tx = wallet_step(
        &mut w,
        &cranker,
        &mint,
        &wallets_answer(vec![(alice.pubkey(), 1_000, 0)]),
        vec![AccountMeta::new(cranker.pubkey(), false)],
    );
    refused(&tx, CompanionError::WalletPayoutTarget);
    // Read-only, below its rent, or one of the protocol's: refused.
    let tx = wallet_step(
        &mut w,
        &cranker,
        &mint,
        &wallets_answer(vec![(alice.pubkey(), 1_000, 0)]),
        vec![AccountMeta::new_readonly(alice.pubkey(), false)],
    );
    refused(&tx, CompanionError::WalletPayoutTarget);
    let fresh = Pubkey::new_unique();
    let tx = wallet_step(
        &mut w,
        &cranker,
        &mint,
        &wallets_answer(vec![(fresh, 1_000, 0)]),
        vec![AccountMeta::new(fresh, false)],
    );
    refused(&tx, CompanionError::WalletPayoutTarget);
    let l = w.launch(&mint);
    let creator = companion::creator_address(&mint);
    for key in [
        launch::launch_address(&mint),
        l.pool,
        creator,
        companion::companion_address(&mint),
        sp::state_address(&mint),
        mint,
        // Accounts of the protocol's programs, refused by their owner.
        bordrless_token::client::holding_address(&w.sol, &creator),
        bordrless_token::client::holding_address(&w.sol, &launch::launch_address(&mint)),
        companion::payout_terms_address(&mint),
        companion::event_authority(),
    ] {
        let tx = wallet_step(
            &mut w,
            &cranker,
            &mint,
            &wallets_answer(vec![(key, 1_000, 0)]),
            vec![AccountMeta::new(key, false)],
        );
        refused(&tx, CompanionError::WalletPayoutTarget);
    }
    // The launcher's wallet is any wallet: a script may pay it, as its terms let it (a wallet payout
    // is named, unlike a holding's).
    let ok = wallet_step(
        &mut w,
        &cranker,
        &mint,
        &wallets_answer(vec![(launcher.pubkey(), budget / 4, 0)]),
        vec![AccountMeta::new(launcher.pubkey(), false)],
    );
    ok.ok();
    // A script without terms (create_script) can't pay wallets either.
    let launcher2 = w.wallet_with_sol(10 * SOL);
    let mint2 = Keypair::new();
    let m2 = mint2.pubkey();
    let ixs = [
        companion::create(
            launcher2.pubkey(),
            launcher2.pubkey(),
            m2,
            create_args(ALL_TO_BENEFICIARY),
        ),
        companion::create_script(launcher2.pubkey(), m2, script_args(), vec![]),
    ];
    w.env.send_paid_by(&ixs, &launcher2, &[&mint2]).ok();
    launch_tx(&mut w, &launcher2, &mint2).ok();
    w.env.warp(31);
    trade(&mut w, &m2, 2, SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees_script(cranker.pubkey(), m2, None)],
            &cranker,
            &[],
        )
        .ok();
    w.env.warp(61);
    let tx = wallet_step(
        &mut w,
        &cranker,
        &m2,
        &wallets_answer(vec![(alice.pubkey(), 1_000, 0)]),
        a(),
    );
    refused(&tx, CompanionError::PayoutsOff);
    // A script with terms whose launcher didn't allow wallet payouts: holders only.
    let (_l3, m3) = script_coin_with(&mut w, terms(5_000, 5_000), false);
    let tx = wallet_step(
        &mut w,
        &cranker,
        &m3,
        &wallets_answer(vec![(alice.pubkey(), 1_000, 0)]),
        a(),
    );
    refused(&tx, CompanionError::WalletPayoutsOff);
}

#[test]
fn wallet_payouts_are_allowed_only_before_the_launch_by_the_mint() {
    let mut w = script_world();
    let (_launcher, mint) = script_coin_with(&mut w, terms(5_000, 5_000), false);
    // After the launch: refused.
    let ix = companion::allow_wallet_payouts(mint);
    let payer = w.wallet_with_sol(SOL);
    let mut ix2 = ix.clone();
    ix2.accounts[0].is_signer = false;
    w.env.send_paid_by(&[ix2], &payer, &[]).expect_fail();
    let t: PayoutTerms = w.env.read(&companion::payout_terms_address(&mint));
    assert!(!t.wallets_allowed());
}

#[test]
fn a_beneficiary_that_cant_take_its_part_never_blocks_the_payees() {
    let mut w = World::new();
    let p: Vec<Keypair> = (0..2).map(|_| w.env.funded(SOL)).collect();
    let payees = [
        PayeeArgs {
            wallet: p[0].pubkey(),
            bps: 5_000,
        },
        PayeeArgs {
            wallet: p[1].pubkey(),
            bps: 4_000,
        },
    ];
    // A beneficiary no transfer can pay: a program.
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let beneficiary = bordrless_bridge::ID;
    let ixs = [
        companion::create(
            launcher.pubkey(),
            beneficiary,
            m,
            create_args(ALL_TO_BENEFICIARY),
        ),
        companion::set_payees(launcher.pubkey(), m, payees.to_vec()),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint]).ok();
    launch_tx(&mut w, &launcher, &mint).ok();
    w.env.warp(31);
    trade(&mut w, &m, 2, SOL);
    claim(&mut w, &m);
    let pending = companion_of(&w, &m).pending_beneficiary;
    let sender = w.wallet_with_sol(SOL);
    let wallets = [p[0].pubkey(), p[1].pubkey()];
    let before: Vec<u64> = wallets.iter().map(|k| w.env.lamports(k)).collect();
    let ix = companion::withdraw_with_payees(sender.pubkey(), m, beneficiary, &wallets);
    let tx = w.env.send_paid_by(&[ix], &sender, &[]);
    tx.ok();
    assert_eq!(
        w.env.lamports(&wallets[0]) - before[0],
        pending * 5_000 / 10_000
    );
    assert_eq!(
        w.env.lamports(&wallets[1]) - before[1],
        pending * 4_000 / 10_000
    );
    assert_eq!(tx.events::<PayeePaid>().len(), 2);
    let rest = pending - pending * 5_000 / 10_000 - pending * 4_000 / 10_000;
    let pay: Payees = w.env.read(&companion::payees_address(&m));
    assert_eq!(pay.beneficiary_owed, rest);
    assert_eq!(
        companion_of(&w, &m).pending_beneficiary,
        rest,
        "kept for the beneficiary"
    );
    // Next time, the payees get only their new parts; the beneficiary's stays owed.
    trade(&mut w, &m, 1, SOL);
    claim(&mut w, &m);
    let pending2 = companion_of(&w, &m).pending_beneficiary;
    let ix = companion::withdraw_with_payees(sender.pubkey(), m, beneficiary, &wallets);
    w.env.send_paid_by(&[ix], &sender, &[]).ok();
    let new = pending2 - rest;
    assert_eq!(
        w.env.lamports(&wallets[0]) - before[0],
        pending * 5_000 / 10_000 + new * 5_000 / 10_000
    );
}

#[test]
fn a_refund_before_the_launch_needs_no_payees_account() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            m,
            create_args(ALL_TO_BENEFICIARY),
        ),
        companion::set_payees(
            launcher.pubkey(),
            m,
            vec![PayeeArgs {
                wallet: Pubkey::new_unique(),
                bps: 1_000,
            }],
        ),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint]).ok();
    let ix = companion::refund(launcher.pubkey(), m, launcher.pubkey());
    w.env.send_paid_by(&[ix], &launcher, &[&mint]).ok();
}
