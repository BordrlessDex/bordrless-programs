#![allow(clippy::type_complexity, clippy::needless_range_loop)]
//! Adversarial audit of payees and wallet payouts (2026-10-13).
//!
//! `finding_*` tests assert the behaviour the design promises and FAIL against the current code:
//! each demonstrates a finding. Every other test pins a property the audit verified.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::AnchorSerialize;
use bordrless_companion::client as companion;
use bordrless_companion::constants::*;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::*;
use bordrless_companion::instructions::{CreateArgs, CreateScriptArgs, PayeeArgs, PayoutTermsArgs};
use bordrless_companion::state::{Companion, Payees, Split, WalletPayoutAnswer, WalletPayoutEntry};
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
const CLOCK: Pubkey = Pubkey::from_str_const("SysvarC1ock11111111111111111111111111111111");

#[track_caller]
fn refused(tx: &Tx, e: CompanionError) {
    tx.expect_code(u32::from(e));
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

fn payees_of(w: &World, mint: &Pubkey) -> Payees {
    w.env.read(&companion::payees_address(mint))
}

fn launch_tx(w: &mut World, launcher: &Keypair, mint: &Keypair) -> Tx {
    let creator = companion::creator_address(&mint.pubkey());
    let launch_args = World::launch_args("AUD", CREATOR_FEE, VQ, LaunchRules::NONE);
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

const ALL_TO_BENEFICIARY: Split = Split {
    buyback_bps: 0,
    holders_bps: 0,
    beneficiary_bps: 10_000,
};

/// A launched classic coin whose beneficiary is `beneficiary` and whose part is shared with
/// `payees`. Answers the payer (launcher) and the mint.
fn coin(w: &mut World, beneficiary: Pubkey, payees: &[PayeeArgs]) -> (Keypair, Keypair) {
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
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
    launch_tx(w, &launcher, &mint).ok();
    (launcher, mint)
}

fn pe(wallet: Pubkey, bps: u16) -> PayeeArgs {
    PayeeArgs { wallet, bps }
}

/// The creator address's SOL above its rent (what `withdraw` sends the beneficiary with its part).
fn spare(w: &World, mint: &Pubkey) -> u64 {
    w.env
        .lamports(&companion::creator_address(mint))
        .saturating_sub(w.env.rent(0))
}

// ---- Payees -------------------------------------------------------------------------------------

/// Fixed after this audit (the beneficiary's part is owed like a payee's). Was: payees are held hostage by the beneficiary's own account. `withdraw` pays the
/// beneficiary's rest in the same instruction with no fallback, so when the beneficiary's account
/// can't take it (here: the launcher emptied its wallet, and the rest is the rounding dust of a
/// 100% payee split, always far below the rent-exempt minimum), every `withdraw` fails and no payee
/// is ever paid, although each payee's own account is funded and passed. Anyone can unblock it by
/// sending the beneficiary its rent, but nothing tells them to, and with an executable beneficiary
/// (next test) nothing can.
#[test]
fn holds_an_unfunded_beneficiary_never_blocks_the_payees() {
    let mut w = World::new();
    let p: Vec<Keypair> = (0..3).map(|_| w.env.funded(SOL)).collect();
    let payees = [
        pe(p[0].pubkey(), 3_333),
        pe(p[1].pubkey(), 3_333),
        pe(p[2].pubkey(), 3_334),
    ];
    let launcher_wallet = w.wallet_with_sol(SOL);
    let (_, mint) = coin(&mut w, launcher_wallet.pubkey(), &payees);
    let m = mint.pubkey();
    let sender = w.wallet_with_sol(SOL);
    let wallets: Vec<Pubkey> = p.iter().map(|k| k.pubkey()).collect();
    // A first withdraw (the launch's spare SOL goes to the beneficiary).
    w.env.warp(31);
    trade(&mut w, &m, 2, SOL);
    claim(&mut w, &m);
    let ix =
        companion::withdraw_with_payees(sender.pubkey(), m, launcher_wallet.pubkey(), &wallets);
    w.env.send_paid_by(&[ix], &sender, &[]).ok();
    assert_eq!(spare(&w, &m), 0);
    // The launcher moves all its SOL out ("send max"): its account is gone.
    w.env.fund(launcher_wallet.pubkey(), 0);
    // More fees. The beneficiary's rest of a 100% split is a few lamports at most.
    w.env.warp(31);
    trade(&mut w, &m, 2, SOL);
    claim(&mut w, &m);
    let pending = companion_of(&w, &m).pending_beneficiary;
    let rest = pending
        - payees
            .iter()
            .map(|e| pending * u64::from(e.bps) / 10_000)
            .sum::<u64>();
    println!("pending {pending}, beneficiary rest {rest}");
    assert!(rest > 0 && rest < w.env.rent(0));
    let before: Vec<u64> = wallets.iter().map(|k| w.env.lamports(k)).collect();
    let ix =
        companion::withdraw_with_payees(sender.pubkey(), m, launcher_wallet.pubkey(), &wallets);
    let tx = w.env.send_paid_by(&[ix], &sender, &[]);
    if let Err(e) = &tx.result {
        println!(
            "error: {:?}\n{}",
            e.err,
            tx.logs()
                .iter()
                .rev()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    // What the design promises: each payee is paid its part.
    assert!(
        tx.result.is_ok(),
        "withdraw failed: every payee is blocked by the beneficiary's account"
    );
    for (i, k) in wallets.iter().enumerate() {
        assert!(w.env.lamports(k) > before[i]);
    }
}

/// Fixed after this audit. Was (same root cause): a beneficiary no transfer can pay (an executable account, here a
/// program) freezes every payee's part for ever: no `withdraw` can ever succeed.
#[test]
fn holds_an_unpayable_beneficiary_never_freezes_the_payees() {
    let mut w = World::new();
    let p: Vec<Keypair> = (0..2).map(|_| w.env.funded(SOL)).collect();
    let payees = [pe(p[0].pubkey(), 5_000), pe(p[1].pubkey(), 5_000)];
    // The launcher names a program as its beneficiary (by mistake, or on purpose: 100% to payees).
    let (_, mint) = coin(&mut w, bordrless_bridge::ID, &payees);
    let m = mint.pubkey();
    w.env.warp(31);
    trade(&mut w, &m, 2, SOL);
    claim(&mut w, &m);
    let sender = w.wallet_with_sol(SOL);
    let wallets: Vec<Pubkey> = p.iter().map(|k| k.pubkey()).collect();
    let ix = companion::withdraw_with_payees(sender.pubkey(), m, bordrless_bridge::ID, &wallets);
    let tx = w.env.send_paid_by(&[ix], &sender, &[]);
    if let Err(e) = &tx.result {
        println!(
            "error: {:?}\n{}",
            e.err,
            tx.logs()
                .iter()
                .rev()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    assert!(
        tx.result.is_ok(),
        "withdraw failed: the payees' parts are frozen for ever"
    );
}

/// Fixed after this audit (nothing pending, nothing to split). Was: a companion with payees can't be refunded before the launch without its payees
/// account, though there is nothing to split (`pending_beneficiary` is 0 before any claim): every
/// existing refund client (the SDK's `refund`, the server's "Get your SOL back") now fails with
/// `PayeesMissing` for such a companion.
#[test]
fn holds_a_prelaunch_refund_needs_no_payees_account() {
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
        companion::set_payees(launcher.pubkey(), m, vec![pe(Pubkey::new_unique(), 1_000)]),
    ];
    w.env.send_paid_by(&ixs, &launcher, &[&mint]).ok();
    assert_eq!(companion_of(&w, &m).pending_beneficiary, 0);
    let ix = companion::refund(launcher.pubkey(), m, launcher.pubkey());
    let tx = w.env.send_paid_by(&[ix], &launcher, &[&mint]);
    assert!(
        tx.result.is_ok(),
        "the refund was refused: {:?}",
        tx.result.as_ref().err().map(|e| &e.err)
    );
}

/// Verified: no payee account can fail `withdraw` (an executable, a sysvar, the payees account
/// itself, the bridge's SOL vault, the sender, an empty account): each is paid or owed, and the
/// owed parts stay in `pending_beneficiary` exactly.
#[test]
fn hostile_payee_accounts_never_fail_withdraw() {
    let mut w = World::new();
    let launcher_kp = w.wallet_with_sol(10 * SOL);
    let mint = Keypair::new();
    let m = mint.pubkey();
    let sender = w.wallet_with_sol(SOL);
    let payees_pda = companion::payees_address(&m);
    let vault = bordrless_bridge::client::sol_vault_address();
    let empty = Pubkey::new_unique();
    let list = vec![
        pe(bordrless_bridge::ID, 1_000), // executable
        pe(CLOCK, 1_000),                // sysvar (demoted to read-only)
        pe(payees_pda, 1_000),           // the payees account itself
        pe(vault, 1_000),                // the bridge's SOL vault (also unwrap's source)
        pe(sender.pubkey(), 1_000),      // the sender, a signer
        pe(empty, 10),                   // no account: a tiny part stays below its rent
    ];
    let ixs = [
        companion::create(
            launcher_kp.pubkey(),
            launcher_kp.pubkey(),
            m,
            create_args(ALL_TO_BENEFICIARY),
        ),
        companion::set_payees(launcher_kp.pubkey(), m, list.clone()),
    ];
    w.env.send_paid_by(&ixs, &launcher_kp, &[&mint]).ok();
    launch_tx(&mut w, &launcher_kp, &mint).ok();
    w.env.warp(31);
    trade(&mut w, &m, 3, SOL);
    claim(&mut w, &m);
    let pending = companion_of(&w, &m).pending_beneficiary;
    let wallets: Vec<Pubkey> = list.iter().map(|e| e.wallet).collect();
    let ix = companion::withdraw_with_payees(sender.pubkey(), m, launcher_kp.pubkey(), &wallets);
    let tx = w.env.send_paid_by(&[ix], &sender, &[]);
    tx.ok();
    let p = payees_of(&w, &m);
    let share = |bps: u64| pending * bps / 10_000;
    // The executable and the sysvar are owed; the others paid; the empty one owed (below rent).
    assert_eq!(p.payees[0].owed, share(1_000));
    assert_eq!(p.payees[1].owed, share(1_000));
    assert_eq!(
        (p.payees[2].paid, p.payees[3].paid, p.payees[4].paid),
        (share(1_000), share(1_000), share(1_000))
    );
    assert_eq!(p.payees[5].owed, share(10));
    assert_eq!(companion_of(&w, &m).pending_beneficiary, p.owed());
    // Owed + paid + the beneficiary's rest is exactly what was pending.
    let given: u64 = p.payees[..6].iter().map(|x| x.owed + x.paid).sum();
    assert_eq!(
        given,
        (0..6).map(|i| share(u64::from(list[i].bps))).sum::<u64>()
    );
}

/// Verified: ten payees, several withdraws with different payees left out, partial owed parts: the
/// companion's `pending_beneficiary` always equals what the payees are owed, each payee's
/// `paid + owed` equals the sum of its rounded-down shares of each withdraw's new part, and the
/// beneficiary receives exactly the rest. Also measures a withdraw paying ten payees.
#[test]
fn ten_payees_account_exactly_over_many_withdraws() {
    let mut w = World::new();
    let bps: [u16; 10] = [1, 7, 333, 999, 1_234, 2_000, 500, 1, 4_000, 925];
    assert!(bps.iter().map(|b| u64::from(*b)).sum::<u64>() <= 10_000);
    let ks: Vec<Keypair> = (0..10).map(|_| w.env.funded(SOL)).collect();
    let list: Vec<PayeeArgs> = ks.iter().zip(bps).map(|(k, b)| pe(k.pubkey(), b)).collect();
    let beneficiary = w.wallet_with_sol(SOL);
    let (_, mint) = coin(&mut w, beneficiary.pubkey(), &list);
    let m = mint.pubkey();
    let sender = w.wallet_with_sol(SOL);
    let mut expect = [0u64; 10];
    let mut owed_before = 0u64;
    for round in 0..6usize {
        w.env.warp(31);
        trade(
            &mut w,
            &m,
            1 + round % 2,
            SOL / 2 + round as u64 * 1_234_567,
        );
        claim(&mut w, &m);
        let pending = companion_of(&w, &m).pending_beneficiary;
        let new = pending - owed_before;
        let mut rest = new;
        for i in 0..10 {
            let s = new * u64::from(bps[i]) / 10_000;
            expect[i] += s;
            rest -= s;
        }
        // Leave out a different payee each round, and every payee on round 3.
        let passed: Vec<Pubkey> = (0..10)
            .filter(|i| round != 3 && *i != round)
            .map(|i| ks[i].pubkey())
            .collect();
        let sp_before = spare(&w, &m);
        let b_before = w.env.lamports(&beneficiary.pubkey());
        let ix = companion::withdraw_with_payees(sender.pubkey(), m, beneficiary.pubkey(), &passed);
        let tx = w.env.send_paid_by(&[ix], &sender, &[]);
        tx.ok();
        if round == 0 {
            println!(
                "withdraw paying 9 payees: CU {} size {} height {}",
                tx.cu(),
                tx.size,
                tx.max_height()
            );
        }
        assert_eq!(
            w.env.lamports(&beneficiary.pubkey()) - b_before,
            rest + sp_before
        );
        let p = payees_of(&w, &m);
        for i in 0..10 {
            assert_eq!(
                p.payees[i].paid + p.payees[i].owed,
                expect[i],
                "payee {i} round {round}"
            );
        }
        owed_before = p.owed();
        assert_eq!(companion_of(&w, &m).pending_beneficiary, owed_before);
    }
    // All ten passed: everything owed paid, nothing pending.
    w.env.warp(31);
    trade(&mut w, &m, 1, SOL);
    claim(&mut w, &m);
    let all: Vec<Pubkey> = ks.iter().map(|k| k.pubkey()).collect();
    let ix = companion::withdraw_with_payees(sender.pubkey(), m, beneficiary.pubkey(), &all);
    let tx = w.env.send_paid_by(&[ix], &sender, &[]);
    tx.ok();
    println!(
        "withdraw paying 10 payees: CU {} size {} height {}",
        tx.cu(),
        tx.size,
        tx.max_height()
    );
    assert!(tx.cu() < 200_000, "fits the default compute budget");
    assert!(
        tx.size <= 1_232,
        "fits one transaction without a lookup table"
    );
    assert_eq!(companion_of(&w, &m).pending_beneficiary, 0);
    assert_eq!(payees_of(&w, &m).owed(), 0);
}

/// Verified: `set_payees` needs the mint's signature even with the right PDA (a third party's
/// payer and a forged mint can't reach someone else's companion), and the payees account can't be
/// substituted in `withdraw` (another coin's payees, a read-only one).
#[test]
fn payees_cant_be_set_or_substituted_by_others() {
    let mut w = World::new();
    let a = w.env.funded(SOL);
    let b = w.env.funded(SOL);
    let beneficiary = w.wallet_with_sol(SOL);
    let (_, mint1) = coin(&mut w, beneficiary.pubkey(), &[pe(a.pubkey(), 5_000)]);
    let (_, mint2) = coin(&mut w, beneficiary.pubkey(), &[pe(b.pubkey(), 100)]);
    let (m1, m2) = (mint1.pubkey(), mint2.pubkey());
    w.env.warp(31);
    trade(&mut w, &m1, 2, SOL);
    claim(&mut w, &m1);
    let sender = w.wallet_with_sol(SOL);
    // Coin 2's payees account (paying b 1%) in coin 1's withdraw: refused.
    let mut ix = companion::withdraw(sender.pubkey(), m1, beneficiary.pubkey());
    ix.accounts
        .push(AccountMeta::new(companion::payees_address(&m2), false));
    ix.accounts.push(AccountMeta::new(b.pubkey(), false));
    refused(
        &w.env.send_paid_by(&[ix], &sender, &[]),
        CompanionError::PayeesMissing,
    );
    // Its own, read-only: refused.
    let mut ix = companion::withdraw(sender.pubkey(), m1, beneficiary.pubkey());
    ix.accounts.push(AccountMeta::new_readonly(
        companion::payees_address(&m1),
        false,
    ));
    refused(
        &w.env.send_paid_by(&[ix], &sender, &[]),
        CompanionError::PayeesMissing,
    );
}

// ---- Wallet payouts -----------------------------------------------------------------------------

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

fn script_coin(
    w: &mut World,
    t: PayoutTermsArgs,
    payees: Option<Vec<PayeeArgs>>,
) -> (Keypair, Pubkey) {
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
    if let Some(p) = payees {
        ixs.push(companion::set_payees(launcher.pubkey(), m, p));
    }
    ixs.push(companion::allow_wallet_payouts(m));
    w.env.send_paid_by(&ixs, &launcher, &[&mint]).ok();
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

/// Fixed after this audit (exclusion by owner): a wallet payout into one of the protocol's own
/// accounts (the creator's bridged-SOL holding, the bridge's SOL vault, the companion's event
/// authority, the payees account, the payout authority) is refused, and nothing moves.
#[test]
fn wallet_payouts_into_protocol_accounts_are_refused() {
    let mut w = script_world();
    let payee = w.env.funded(SOL);
    let (_launcher, mint) = script_coin(
        &mut w,
        terms(10_000, 2_000),
        Some(vec![pe(payee.pubkey(), 5_000)]),
    );
    let creator = companion::creator_address(&mint);
    let creator_quote = bordrless_token::client::holding_address(&BRIDGED_SOL_MINT, &creator);
    let targets = [
        creator_quote,
        bordrless_bridge::client::sol_vault_address(),
        companion::event_authority(),
        companion::payees_address(&mint),
        PAYOUT_AUTHORITY,
    ];
    let pot = companion_of(&w, &mint).pending_pot;
    let cranker = w.wallet_with_sol(SOL);
    for k in targets {
        let tx = wallet_step(
            &mut w,
            &cranker,
            &mint,
            &wallets_answer(vec![(k, 1_000_000, 0)]),
            vec![AccountMeta::new(k, false)],
        );
        refused(&tx, CompanionError::WalletPayoutTarget);
    }
    assert_eq!(companion_of(&w, &mint).pending_pot, pot);
}

fn terms(step_bps: u16, holding_bps: u16) -> PayoutTermsArgs {
    PayoutTermsArgs {
        step_bps,
        holding_bps,
        window_bps: 10_000,
        window_secs: 300,
    }
}

/// Verified: the creator address can't be paid through a duplicate (named twice in the remaining
/// accounts under different indices), nor can the same wallet be paid twice through two indices;
/// a payout to the cranker adds to its bounty.
#[test]
fn wallet_payouts_resist_duplicates_and_pay_the_cranker() {
    let mut w = script_world();
    let (_l, mint) = script_coin(&mut w, terms(10_000, 10_000), None);
    let creator = companion::creator_address(&mint);
    let cranker = w.wallet_with_sol(SOL);
    let alice = w.env.funded(SOL);
    // The creator, passed again in the remaining accounts.
    let tx = wallet_step(
        &mut w,
        &cranker,
        &mint,
        &wallets_answer(vec![(creator, 1_000, 0)]),
        vec![AccountMeta::new(creator, false)],
    );
    refused(&tx, CompanionError::WalletPayoutTarget);
    // Alice at two indices.
    let tx = wallet_step(
        &mut w,
        &cranker,
        &mint,
        &wallets_answer(vec![(alice.pubkey(), 1_000, 0), (alice.pubkey(), 1_000, 0)]),
        vec![
            AccountMeta::new(alice.pubkey(), false),
            AccountMeta::new(alice.pubkey(), false),
        ],
    );
    refused(&tx, CompanionError::BadPayoutAnswer);
    // The cranker: paid its payout and its bounty.
    let before = w.env.lamports(&cranker.pubkey());
    let tx = wallet_step(
        &mut w,
        &cranker,
        &mint,
        &wallets_answer(vec![(cranker.pubkey(), 7_777, 0)]),
        vec![AccountMeta::new(cranker.pubkey(), false)],
    );
    tx.ok();
    let stepped: ScriptStepped = tx.event();
    assert_eq!(stepped.bounty, SCRIPT_BOUNTY / 8);
    let _ = before;
    let paid: ScriptWalletPayout = tx.event();
    assert_eq!((paid.to, paid.lamports), (cranker.pubkey(), 7_777));
}
