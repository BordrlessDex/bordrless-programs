//! The kit's money (`docs/hooks-v2.md` §4.11) through real launches: stage 3's seeded walk of buys,
//! sells, transfers, burns, claims, shares and donations, with its trades real swaps on the launch
//! pool (the holder fee taken by the pool hook into the reward vault, the burns by the DEX) and
//! the buy that raises the threshold graduating the pool in the same transaction, as the site
//! sends it. After every step `eligible` is the holders' balances and the vault covers every
//! claimable amount, what is held and what still streams; every claim pays exactly what the mirror
//! computed; at the end everything is claimed and every lamport that arrived is accounted for.
//! And the trades the review of fix1 found around a share (§0.17, §4.10), which now lose money.

use bordrless_kit::constants::SHARE_STREAM_SECS;
use bordrless_kit::error::KitError;
use bordrless_launch::constants::STATUS_GRADUATED;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::kit::{money_walk, KitToken, Walk, WalkReport};
use bordrless_program_tests::launch::{presets, rules, LaunchMarket, DAY, SOL, VQ};
use solana_keypair::Keypair;
use solana_signer::Signer;

/// A launch opening at 2 SOL of virtual quote: it graduates once about 4 SOL is raised.
const SMALL_VQ: u64 = 2 * SOL;

struct Run {
    report: WalkReport,
    market: LaunchMarket,
    graduated: bool,
}

fn launch_walk(rules: LaunchRules, walk: Walk) -> Run {
    let mut w = World::new();
    let creator = w.wallet_with_sol(5 * SOL);
    let (mint, tx) = w.create_launch_with(&creator, "WALK", 50, SMALL_VQ, rules);
    tx.ok();
    let k = KitToken::of(&w.env, mint);
    let mut market = LaunchMarket {
        min_buy: 50_000,
        ..LaunchMarket::default()
    };
    let report = money_walk(&mut w, &k, &mut market, &walk);
    let graduated = w.launch(&mint).status == STATUS_GRADUATED;
    assert_eq!(w.env.kit_config(&mint).graduated, graduated);
    assert!(market.graduations <= 1);
    assert_eq!(market.graduations == 1, graduated);
    Run {
        report,
        market,
        graduated,
    }
}

fn print(name: &str, walk: &Walk, run: &Run) {
    let r = &run.report;
    println!(
        "{name}: seed {} steps {} | swaps {} buys {} sells | graduated {} (buy + graduate max \
         CU {}) | inflows {} claimed {} in {} claims ({} empty) | dust {} (max {}) | {} steps \
         below the threshold | refused {:?} | max CU {:?}",
        walk.seed,
        walk.steps,
        run.market.buys,
        run.market.sells,
        run.graduated,
        run.market.graduation_cu,
        r.inflows,
        r.claimed,
        r.claims,
        r.empty_claims,
        r.dust,
        r.max_dust,
        r.below_min_steps,
        r.refused,
        r.max_cu
    );
}

fn allowed() -> Vec<KitError> {
    vec![
        KitError::MaxWalletExceeded,
        KitError::EarlyLocked,
        KitError::CreatorLocked,
    ]
}

#[test]
fn a_launch_with_holder_rewards_burns_and_the_early_lock_walks_through_graduation() {
    // Holder rewards 1% both sides, burn 0.5% both sides, the creator wallet locked, buys in the
    // first minute locked for an hour.
    let walk = Walk {
        seed: 41,
        steps: 400,
        holders: 8,
        holder_sol: 20 * SOL,
        buy_max: SOL,
        thin_buy_max: 1_000_000,
        phase: 40,
        allowed: allowed(),
    };
    let run = launch_walk(
        rules(100, 100, 50, 50, 0, 30 * DAY, 60, 3_600),
        walk.clone(),
    );
    print("rewards + burn + locks", &walk, &run);
    assert!(run.graduated, "the walk raises the curve and graduates it");
    assert!(run.report.claims > 10 && run.report.below_min_steps > 0);
    assert!(run
        .report
        .refused
        .contains_key(&KitError::EarlyLocked.name()));
}

#[test]
fn a_launch_paying_holders_on_sells_walks_through_graduation() {
    let walk = Walk {
        seed: 42,
        steps: 400,
        holders: 8,
        holder_sol: 20 * SOL,
        buy_max: SOL,
        thin_buy_max: 1_000_000,
        phase: 50,
        allowed: allowed(),
    };
    let run = launch_walk(presets::paid_to_hold(), walk.clone());
    print("paid to hold", &walk, &run);
    assert!(run.graduated);
    assert!(run.report.claims > 10);
}

#[test]
fn a_launch_with_every_rule_walks_within_max_wallet() {
    // Max wallet 5% keeps ten holders from buying the curve out: no graduation here, many
    // refusals at the cap.
    let walk = Walk {
        seed: 43,
        steps: 300,
        holders: 10,
        holder_sol: 2 * SOL,
        buy_max: 80_000_000,
        thin_buy_max: 1_000_000,
        phase: 40,
        allowed: allowed(),
    };
    let run = launch_walk(presets::every_rule(), walk.clone());
    print("every rule", &walk, &run);
    assert!(run.report.claims > 5);
    assert!(run
        .report
        .refused
        .contains_key(&KitError::MaxWalletExceeded.name()));
}

#[test]
fn a_launch_without_holder_rewards_keeps_the_count() {
    // Max wallet and the early-buyer lock: no reward vault, `eligible` still exact (the
    // early-buyer lock makes the kit see burns).
    let walk = Walk {
        seed: 44,
        steps: 200,
        holders: 8,
        holder_sol: 2 * SOL,
        buy_max: 80_000_000,
        thin_buy_max: 1_000_000,
        phase: 40,
        allowed: allowed(),
    };
    let run = launch_walk(rules(0, 0, 25, 25, 500, 0, 60, 3_600), walk.clone());
    print("max wallet + early lock + burn", &walk, &run);
    assert_eq!(run.report.claims, 0);
}

/// When the traders around a share sell.
#[derive(Clone, Copy, Debug)]
enum Sell {
    /// At the running stream's end as the config shows it right after the share, or at once if
    /// that has passed (the back-run the review of fix1 ran: no foresight needed).
    AtStreamEnd,
    /// This many seconds after the share.
    After(i64),
}

/// One trade around a share.
struct ShareTrade {
    /// What the three traders gained together after every fee, in lamports (negative: lost).
    net: i128,
    /// What they claimed.
    claimed: u64,
    /// The stream right after the share: running, waiting, seconds to the running stream's end.
    stream: (u64, u64, i64),
}

/// A launch with holder rewards 1% both sides, creator 0.5%, max wallet 2% and the creator
/// wallet locked (the policy's 25 SOL opening) with three holders who bought 0.5 SOL each;
/// `first` lamports shared
/// at t1 (nothing when 0); then 1 SOL shared at t1 + 3,599 s. Three traders (max wallet is 2%)
/// buy 0.5 SOL each, just before the 1 SOL share (`before`) or one second after it, sell
/// everything as `sell` says, and claim what they are owed.
fn trade_around_a_share(first: u64, before: bool, sell: Sell) -> ShareTrade {
    let mut w = World::new();
    let creator = w.wallet_with_sol(5 * SOL);
    let (mint, tx) = w.create_launch_with(&creator, "SHARE", 50, VQ, presets::rewards_and_cap());
    tx.ok();
    let k = KitToken::of(&w.env, mint);
    w.env.warp(60);
    for _ in 0..3 {
        let holder = w.wallet_with_sol(SOL);
        w.buy(&holder, &mint, SOL / 2).ok();
    }
    let project = w.wallet_with_sol(first + SOL);
    let victim = w.wallet_with_sol(2 * SOL);
    let traders: Vec<Keypair> = (0..3).map(|_| w.wallet_with_sol(SOL)).collect();
    let sol = w.sol;
    let start: Vec<u64> = traders
        .iter()
        .map(|t| w.env.holding(&sol, &t.pubkey()))
        .collect();
    let buy_in = |w: &mut World| {
        for t in &traders {
            w.buy(t, &mint, SOL / 2).ok();
        }
    };
    w.env.warp(60);
    let t1 = w.env.now;
    if first > 0 {
        w.kit_share(&project, &k, first).ok();
    }
    w.env.warp(SHARE_STREAM_SECS - 1);
    if before {
        buy_in(&mut w);
    }
    w.kit_share(&victim, &k, SOL).ok();
    let shared_at = w.env.now;
    assert_eq!(shared_at, t1 + SHARE_STREAM_SECS - 1);
    let c = w.env.kit_config(&mint);
    let stream = (c.stream_remaining, c.stream_next, c.stream_end - shared_at);
    if !before {
        w.env.warp(1);
        buy_in(&mut w);
    }
    let sell_at = match sell {
        Sell::AtStreamEnd => c.stream_end.max(w.env.now),
        Sell::After(secs) => shared_at + secs,
    };
    w.env.warp(sell_at - w.env.now);
    let mut claimed = 0;
    for t in &traders {
        let tokens = w.env.holding(&mint, &t.pubkey());
        w.sell(t, &mint, tokens).ok();
        let owed = w.env.claimable(&k, &t.pubkey());
        if owed > 0 {
            w.kit_claim(t, &k).ok();
            claimed += owed;
        }
    }
    let net = traders
        .iter()
        .zip(&start)
        .map(|(t, s)| i128::from(w.env.holding(&sol, &t.pubkey())) - i128::from(*s))
        .sum();
    ShareTrade {
        net,
        claimed,
        stream,
    }
}

#[test]
fn trading_around_a_share_loses_money() {
    // The review of fix1 (§0.17, §4.10): with the shares merged into one stream, 1 SOL shared one
    // second before a 36 SOL share's hour ended was out within 98 s, and three wallets that bought
    // 0.5 SOL each a second after it and sold at the stream's end kept about 0.40 SOL after every
    // fee (0.40 SOL bought just before it). Now the 1 SOL waits for the 36 SOL's last second and
    // streams its own hour, so each of these trades loses money, as it does around a share
    // streaming its hour on its own. (The fees these trades pay are now the share model's:
    // Bordrless takes a quarter of the creator and holder fees instead of a flat 0.25%, which
    // changes the loss by a few lamports and nothing else.)
    let thirty_six = 36 * SOL;
    let tail = thirty_six / SHARE_STREAM_SECS as u64;
    for (name, first, before, sell) in [
        (
            "back-run, sell at the stream's end",
            thirty_six,
            false,
            Sell::AtStreamEnd,
        ),
        ("back-run, hold 98 s", thirty_six, false, Sell::After(98)),
        ("sandwich, hold 98 s", thirty_six, true, Sell::After(98)),
        (
            "a share on its own hour, sandwich, hold 98 s",
            0,
            true,
            Sell::After(98),
        ),
    ] {
        let r = trade_around_a_share(first, before, sell);
        println!(
            "{name}: stream after the share {:?}; claimed {} lamports; net {} lamports",
            r.stream, r.claimed, r.net
        );
        // With fix1's merge the first case claimed 480,354,377 lamports and gained 398,413,162.
        assert!(r.net < 0, "{name}: the traders gained {} lamports", r.net);
        if first > 0 {
            // The 36 SOL's end did not move; the 1 SOL waits for its last second.
            assert_eq!(r.stream, (tail, SOL, 1), "{name}");
        } else {
            assert_eq!(r.stream, (SOL, 0, SHARE_STREAM_SECS), "{name}");
        }
    }
}
