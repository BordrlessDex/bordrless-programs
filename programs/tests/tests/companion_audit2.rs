//! The companion audit's second round (2026-10-08): the buyback's reference price. Each finding is
//! kept as a regression test.

use anchor_lang::prelude::Pubkey;
use bordrless_companion::client as companion;
use bordrless_companion::events::{BoughtBack, BuybackWaited};
use bordrless_companion::instructions::CreateArgs;
use bordrless_companion::state::{spot_price, Companion, Split};
use bordrless_core::policy;
use bordrless_launch::client::{self as launch, LaunchKeys};
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_token::client as token;
use solana_keypair::Keypair;
use solana_signer::Signer;

const BUYBACK: Split = Split {
    buyback_bps: 10_000,
    holders_bps: 0,
    beneficiary_bps: 0,
};

fn companion_of(w: &World, mint: &Pubkey) -> Companion {
    w.env.read(&companion::companion_address(mint))
}

fn spot(w: &World, mint: &Pubkey) -> u128 {
    let p = w.launch_pool(mint);
    spot_price(
        p.quote_reserve,
        p.virtual_quote,
        p.base_reserve,
        p.virtual_base,
    )
    .unwrap()
}

/// A companion launch with `fee` creator fee and a large pending buyback.
fn setup(fee: u16, volume_rounds: usize) -> (World, Pubkey, Keypair) {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint = Keypair::new();
    let args = CreateArgs {
        split: BUYBACK,
        bounty_bps: 50,
        max_buyback: 8 * SOL,
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
    let a = World::launch_args("RAT", fee, VQ, LaunchRules::NONE);
    let inner = launch::create_launch_with(
        creator,
        mint.pubkey(),
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        a.clone(),
        None,
        None,
    );
    w.env
        .send_paid_by(
            &[companion::launch(
                launcher.pubkey(),
                mint.pubkey(),
                &inner,
                a,
            )],
            &launcher,
            &[&mint],
        )
        .ok();
    let mint = mint.pubkey();
    w.env.warp(31);
    for _ in 0..volume_rounds {
        let t = w.wallet_with_sol(60 * SOL);
        w.buy(&t, &mint, 50 * SOL).ok();
        let h = w.env.holding(&mint, &t.pubkey());
        w.sell(&t, &mint, h).ok();
    }
    let cranker = w.wallet_with_sol(500 * SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees(cranker.pubkey(), mint, None)],
            &cranker,
            &[],
        )
        .ok();
    w.env
        .send_paid_by(
            &[token::create_holding(
                cranker.pubkey(),
                mint,
                cranker.pubkey(),
            )],
            &cranker,
            &[],
        )
        .ok();
    w.env.warp(60);
    (w, mint, cranker)
}

/// The gross bridged SOL a buy needs to bring the pool's price to `target` (fee bound `fee_bps`).
fn pump_to(w: &World, mint: &Pubkey, target: u128, fee_bps: u64) -> u64 {
    let p = w.launch_pool(mint);
    let x = (p.quote_reserve + p.virtual_quote) as f64;
    let y = (p.base_reserve + p.virtual_base) as f64;
    let x_t = (target as f64 * x * y / 1e12).sqrt();
    if x_t <= x {
        return 0;
    }
    ((x_t - x) * 10_000.0 / (10_000 - fee_bps) as f64) as u64
}

fn wealth(w: &World, who: &Pubkey) -> i128 {
    w.env.holding(&w.sol, who) as i128 + w.env.lamports(who) as i128
}

/// Round 2, finding 1: at low fees a cranker who pumped to just under the 3% premium before every
/// buyback and sold right after profited, and each buy dragged the reference up toward the price it
/// left. Now a buyback spends less than a sandwich's fees (`pool_share_bps`) and a buy never raises
/// the reference: the sandwich earns less than honest cranking (its bounties) and the reference
/// ends no higher than the price once the pumps are sold.
#[test]
fn reference_ratchet_sandwich_at_low_fees() {
    for (fee, rounds) in [(50, 12), (10, 40)] {
        ratchet(fee, rounds);
    }
}

fn ratchet(fee: u16, rounds: usize) {
    let (mut w, mint, cranker) = setup(fee, rounds);
    let k = LaunchKeys::of(&w.launch(&mint));
    let start = wealth(&w, &cranker.pubkey());
    let opening = companion_of(&w, &mint).reference_price;
    let mut spent_total = 0u64;
    let mut bounties = 0i128;
    for i in 0..10 {
        let r = companion_of(&w, &mint).reference_price;
        let target = r * 10_280 / 10_000;
        let gross = pump_to(&w, &mint, target, 100);
        if gross > 0 {
            w.env
                .send_paid_by(
                    &[w.launch_swap_ix(&cranker.pubkey(), &mint, 1, gross, 0)],
                    &cranker,
                    &[],
                )
                .ok();
        }
        let tx = w.env.send_paid_by(
            &[companion::buyback(cranker.pubkey(), &k, false)],
            &cranker,
            &[],
        );
        tx.ok();
        let spent = tx.events::<BoughtBack>().first().map_or(0, |e| e.spent);
        bounties += tx.events::<BoughtBack>().first().map_or(0, |e| e.bounty) as i128;
        let waited = !tx.events::<BuybackWaited>().is_empty();
        spent_total += spent;
        let held = w.env.holding(&mint, &cranker.pubkey());
        if held > 0 {
            w.sell(&cranker, &mint, held).ok();
        }
        let r2 = companion_of(&w, &mint).reference_price;
        println!(
            "interval {i}: pumped {gross}, waited {waited}, spent {spent}; reference/opening {:.3}, \
             price after sell/opening {:.3}, cumulative profit {}",
            r2 as f64 / opening as f64,
            spot(&w, &mint) as f64 / opening as f64,
            wealth(&w, &cranker.pubkey()) - start
        );
        w.env.warp(60);
    }
    let profit = wealth(&w, &cranker.pubkey()) - start;
    println!("fee {fee}: total buyback spent {spent_total}, cranker profit {profit}, bounties {bounties}");
    assert!(spent_total > 0, "fee {fee}: no buyback ran");
    assert!(
        profit < bounties,
        "fee {fee}: the sandwich paid {profit} beyond {bounties} in bounties"
    );
    // The reference follows only the price the buybacks really made, never what the pumps showed.
    assert!(
        companion_of(&w, &mint).reference_price <= spot(&w, &mint),
        "fee {fee}: the reference rose above the price"
    );
}

/// Round 2, finding 2: a wait moved the reference 5% once however long it had been, so after a real
/// rise buybacks stayed off for dozens of unpaid cranks. Now one wait catches up a step for every
/// interval since the reference last moved, and the next interval buys.
#[test]
fn reference_lags_a_real_rise() {
    let (mut w, mint, cranker) = setup(100, 8);
    let k = LaunchKeys::of(&w.launch(&mint));
    // A real rise: the price triples and stays.
    let whale = w.wallet_with_sol(80 * SOL);
    w.buy(&whale, &mint, 30 * SOL).ok();
    let opening = companion_of(&w, &mint).reference_price;
    println!(
        "price/reference {:.2}",
        spot(&w, &mint) as f64 / opening as f64
    );
    // A whole day passes with nobody cranking: one crank still moves the reference 5%.
    w.env.warp(86_400);
    let mut waits = 0;
    loop {
        let before = wealth(&w, &cranker.pubkey());
        let tx = w.env.send_paid_by(
            &[companion::buyback(cranker.pubkey(), &k, false)],
            &cranker,
            &[],
        );
        tx.ok();
        if !tx.events::<BoughtBack>().is_empty() {
            break;
        }
        assert!(
            wealth(&w, &cranker.pubkey()) <= before,
            "a wait pays nothing"
        );
        waits += 1;
        w.env.warp(60);
        assert!(waits < 200);
    }
    println!("unpaid waits before a buyback: {waits}");
    assert_eq!(waits, 1);
}
