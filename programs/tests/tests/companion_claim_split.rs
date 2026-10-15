//! A fee claim tells the hook's SOL cut apart (2026-10-15): the launchpad pays a custom hook's SOL
//! cut (`Launch::quote_cut_accrued`) into the creator fee vault with the creator fee, and
//! `claim_fees` credits the part of each claim that is the cut not yet taken
//! (`Companion.quote_cut_claimed`) whole to the pot's bucket, splitting only the rest by the shares.
//!
//! The launches here have no SOL cut of their own: the cut is simulated by raising the launch's
//! counter by part of what its vault holds (the launchpad keeps the cut's SOL in that vault with the
//! creator fee, so a claim takes both). `companion_script.rs` has the same with a script's pot and
//! an author's share.

use anchor_lang::prelude::Pubkey;
use anchor_lang::{AccountDeserialize, AccountSerialize};
use bordrless_companion::client as companion;
use bordrless_companion::events::FeesClaimed;
use bordrless_companion::instructions::CreateArgs;
use bordrless_companion::state::{Companion, Split};
use bordrless_core::policy;
use bordrless_launch::client as launch;
use bordrless_launch::state::{Launch, LaunchRules};
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use solana_keypair::Keypair;
use solana_signer::Signer;

const CREATOR_FEE: u16 = 200;
/// A companion's size before `quote_cut_claimed`.
const OLD_LEN: usize = 300;

fn launched(w: &mut World, split: Split) -> Pubkey {
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint = Keypair::new();
    let args = CreateArgs {
        split,
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs: 0,
        fund: SOL / 2,
    };
    w.env
        .send_paid_by(
            &[companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                mint.pubkey(),
                args,
            )],
            &launcher,
            &[&mint],
        )
        .ok();
    let creator = companion::creator_address(&mint.pubkey());
    let launch_args = World::launch_args("CUT", CREATOR_FEE, VQ, LaunchRules::NONE);
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
    w.env.send_paid_by(&[ix], &launcher, &[&mint]).ok();
    w.env.warp(31);
    mint.pubkey()
}

fn trade(w: &mut World, mint: &Pubkey, wallets: usize, sol: u64) {
    for _ in 0..wallets {
        let t = w.wallet_with_sol(sol + SOL);
        w.buy(&t, mint, sol).ok();
        let held = w.env.holding(mint, &t.pubkey());
        w.sell(&t, mint, held / 4).ok();
    }
}

fn companion_of(w: &World, mint: &Pubkey) -> Companion {
    w.env.read(&companion::companion_address(mint))
}

fn vault(w: &World, mint: &Pubkey) -> u64 {
    w.env.holding(&w.sol, &launch::launch_address(mint))
}

/// The launch's hook cut counted `more` higher, as the launchpad counts it on a trade.
fn add_cut(w: &mut World, mint: &Pubkey, more: u64) {
    let key = launch::launch_address(mint);
    let mut account = w.env.account(&key).unwrap();
    let mut l = Launch::try_deserialize(&mut &account.data[..]).unwrap();
    l.add_quote_cut(more);
    let mut data = Vec::new();
    l.try_serialize(&mut data).unwrap();
    account.data[..data.len()].copy_from_slice(&data);
    w.env.put(key, account);
}

/// The companion as one made before the upgrade: its 300 bytes (rent for them only).
fn make_old(w: &mut World, mint: &Pubkey) {
    let key = companion::companion_address(mint);
    let mut account = w.env.account(&key).unwrap();
    assert_eq!(account.data.len(), Companion::LEN);
    assert!(account.data[OLD_LEN..].iter().all(|b| *b == 0));
    account.data.truncate(OLD_LEN);
    account.lamports = w.env.rent(OLD_LEN);
    w.env.put(key, account);
    let c = companion_of(w, mint);
    assert!(!c.quote_cut_claimed.stored);
}

fn claim(w: &mut World, mint: &Pubkey) -> Tx {
    let cranker = w.wallet_with_sol(SOL);
    let tx = w.env.send_paid_by(
        &[companion::claim_fees(cranker.pubkey(), *mint, None)],
        &cranker,
        &[],
    );
    tx.ok();
    tx
}

/// Every lamport of a claim of `got` lands in exactly one place.
fn adds_up(ev: &FeesClaimed, got: u64) {
    assert_eq!(ev.claimed, got);
    assert_eq!(
        ev.bounty + ev.to_buyback + ev.to_holders + ev.to_beneficiary,
        got
    );
}

const HALF: Split = Split {
    buyback_bps: 5_000,
    holders_bps: 0,
    beneficiary_bps: 5_000,
};

#[test]
fn the_cut_goes_to_the_buyback_and_the_launcher_takes_half_of_the_fee_only() {
    let mut w = World::new();
    let mint = launched(&mut w, HALF);
    assert!(companion_of(&w, &mint).quote_cut_claimed.stored);
    trade(&mut w, &mint, 4, 2 * SOL);
    let got = vault(&w, &mint);
    // PumpTek's proportions: 45.42 of 106.96 the cut.
    let cut = got * 4_542 / 10_696;
    add_cut(&mut w, &mint, cut);
    let tx = claim(&mut w, &mint);
    let ev: FeesClaimed = tx.event();
    adds_up(&ev, got);
    let bounty = got * 50 / 10_000;
    let rest = got - bounty;
    let from_hook = (u128::from(rest) * u128::from(cut) / u128::from(got)) as u64;
    let base = rest - from_hook;
    assert_eq!((ev.bounty, ev.from_hook), (bounty, from_hook));
    assert_eq!(ev.to_beneficiary, base / 2);
    assert_eq!(ev.to_buyback, base - base / 2 + from_hook);
    // Before: the launcher took half of everything.
    assert!(ev.to_beneficiary < rest / 2);
    let c = companion_of(&w, &mint);
    assert_eq!(c.quote_cut_claimed.amount, cut);
    assert_eq!(
        (c.pending_beneficiary, c.pending_buyback),
        (ev.to_beneficiary, ev.to_buyback)
    );

    // The next claim, with no more cut: split as ever.
    trade(&mut w, &mint, 2, SOL);
    let got = vault(&w, &mint);
    let ev: FeesClaimed = claim(&mut w, &mint).event();
    adds_up(&ev, got);
    assert_eq!(ev.from_hook, 0);
    let rest = got - ev.bounty;
    assert_eq!(ev.to_beneficiary, rest / 2);
    assert_eq!(companion_of(&w, &mint).quote_cut_claimed.amount, cut);
}

#[test]
fn an_old_companion_counts_the_cut_before_the_upgrade_as_taken() {
    let mut w = World::new();
    let mint = launched(&mut w, HALF);
    trade(&mut w, &mint, 4, 2 * SOL);
    // Cut taken before the upgrade (claimed and split as it was then, or still in the vault).
    let before_upgrade = vault(&w, &mint) / 3;
    add_cut(&mut w, &mint, before_upgrade);
    make_old(&mut w, &mint);
    let key = companion::companion_address(&mint);

    // The first claim after it: the account grown (the cranker paying the 8 bytes' rent), the cut
    // so far counted as taken, the claim split as before.
    let got = vault(&w, &mint);
    let tx = claim(&mut w, &mint);
    let ev: FeesClaimed = tx.event();
    adds_up(&ev, got);
    assert_eq!(ev.from_hook, 0);
    assert_eq!(ev.to_beneficiary, (got - ev.bounty) / 2);
    let account = w.env.account(&key).unwrap();
    assert_eq!(account.data.len(), Companion::LEN);
    assert_eq!(account.lamports, w.env.rent(Companion::LEN));
    let c = companion_of(&w, &mint);
    assert!(c.quote_cut_claimed.stored);
    assert_eq!(c.quote_cut_claimed.amount, before_upgrade);

    // The cut from then on goes to the buyback.
    trade(&mut w, &mint, 2, SOL);
    let got = vault(&w, &mint);
    let cut = got / 2;
    add_cut(&mut w, &mint, cut);
    let ev: FeesClaimed = claim(&mut w, &mint).event();
    adds_up(&ev, got);
    let rest = got - ev.bounty;
    let from_hook = (u128::from(rest) * u128::from(cut) / u128::from(got)) as u64;
    assert_eq!(ev.from_hook, from_hook);
    assert_eq!(ev.to_beneficiary, (rest - from_hook) / 2);
    assert_eq!(
        companion_of(&w, &mint).quote_cut_claimed.amount,
        before_upgrade + cut
    );
}

#[test]
fn a_cut_larger_than_the_claim_is_capped_and_the_rest_waits() {
    let mut w = World::new();
    let mint = launched(&mut w, HALF);
    trade(&mut w, &mint, 4, 2 * SOL);
    let got = vault(&w, &mint);
    let extra = got / 5;
    add_cut(&mut w, &mint, got + extra);
    let ev: FeesClaimed = claim(&mut w, &mint).event();
    adds_up(&ev, got);
    // All of it the hook's: nothing to the launcher.
    assert_eq!(ev.from_hook, got - ev.bounty);
    assert_eq!(ev.to_beneficiary, 0);
    assert_eq!(ev.to_buyback, got - ev.bounty);
    assert_eq!(companion_of(&w, &mint).quote_cut_claimed.amount, got);

    // The next claim: the rest of the cut first, then the fee split.
    trade(&mut w, &mint, 4, 2 * SOL);
    let got2 = vault(&w, &mint);
    assert!(got2 > extra);
    let ev: FeesClaimed = claim(&mut w, &mint).event();
    adds_up(&ev, got2);
    let rest = got2 - ev.bounty;
    let from_hook = (u128::from(rest) * u128::from(extra) / u128::from(got2)) as u64;
    assert_eq!(ev.from_hook, from_hook);
    assert_eq!(ev.to_beneficiary, (rest - from_hook) / 2);
    assert_eq!(
        companion_of(&w, &mint).quote_cut_claimed.amount,
        got + extra
    );
}

#[test]
fn a_beneficiary_only_split_has_nowhere_else_to_put_the_cut() {
    let mut w = World::new();
    let mint = launched(
        &mut w,
        Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 10_000,
        },
    );
    trade(&mut w, &mint, 2, SOL);
    let got = vault(&w, &mint);
    add_cut(&mut w, &mint, got / 2);
    let ev: FeesClaimed = claim(&mut w, &mint).event();
    adds_up(&ev, got);
    assert!(ev.from_hook > 0);
    assert_eq!((ev.to_buyback, ev.to_holders), (0, 0));
    assert_eq!(ev.to_beneficiary, got - ev.bounty);
}
