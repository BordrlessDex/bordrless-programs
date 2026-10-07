//! The kit's money (`docs/hooks-v2.md` §4.7, §4.8, §4.11, §4.13) on chain: the economics review's
//! hand example, a share streamed over its hour (and one made while another streams, waiting for
//! it), donations, a holder who sells out, claims and closes, and seeded walks of every operation
//! with the invariants after each step and every claim paying exactly what the mirror computed.

use anchor_lang::prelude::Pubkey;
use bordrless_kit::constants::{MIN_SHARE_LAMPORTS, SCALE, SHARE_STREAM_SECS};
use bordrless_kit::error::KitError;
use bordrless_kit::events::{RewardsClaimed, RewardsShared};
use bordrless_kit::{modules, HolderData};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::kit::{
    check_kit, claim_as_mirrored, kit_code, money_walk, refused_by_kit, DirectKit, DirectMarket,
    KitSpec, Walk, WalkReport, SOL,
};
use bordrless_token::client as token;
use solana_signer::Signer;

const SUPPLY: u64 = 1_000_000_000_000_000;
const MIN_ELIGIBLE: u64 = SUPPLY / 1_000;

fn rewards_kit(w: &mut World) -> DirectKit {
    w.direct_kit(&KitSpec::new(modules::HOLDER_REWARDS))
}

fn buy(w: &mut World, d: &DirectKit, to: &Pubkey, amount: u64) {
    w.send_tokens(&d.pool, d.token.mint, to, amount).ok();
}

#[test]
fn the_hand_example_pays_at_most_what_came_in() {
    // The economics review's example: one holder of 1.5e12 (min_eligible 1e12), inflows of 2, 0,
    // 1, 1, 1 lamports. The old lamport carry paid rounding twice; the scaled remainder cannot.
    for claim_each_time in [true, false] {
        let mut w = World::new();
        let d = rewards_kit(&mut w);
        let t = d.token.clone();
        let h = w.kit_holder(&t, SOL);
        let donor = w.wallet_with_sol(SOL);
        buy(&mut w, &d, &h.pubkey(), 1_500_000_000_000);
        let mut report = WalkReport::default();
        for inflow in [2u64, 0, 1, 1, 1] {
            if inflow > 0 {
                w.donate(&donor, &t, inflow).ok();
            }
            if claim_each_time {
                claim_as_mirrored(&mut w, &t, &h, &mut report);
            } else {
                // A sync that settles nobody: the launch sends one unit to the pool.
                w.send_tokens(&d.launch, t.mint, &t.pool, 1).ok();
            }
            check_kit(&w.env, &t, &[h.pubkey()]);
        }
        claim_as_mirrored(&mut w, &t, &h, &mut report);
        // Settled after every inflow, each half lamport is lost to rounding; settled once at the
        // end, one half. Never more than came in.
        assert!(report.claimed <= 5);
        assert_eq!(report.claimed, if claim_each_time { 3 } else { 4 });
        let c = w.env.kit_config(&t.mint);
        assert_eq!((c.total_distributed, c.total_claimed), (5, report.claimed));
        assert_eq!(w.env.vault_amount(&t), 5 - report.claimed);
        assert_eq!(c.acc_per_share, 3);
        assert_eq!(c.rem, SCALE / 2);
    }
}

#[test]
fn a_share_streams_over_the_hour() {
    let mut w = World::new();
    let d = rewards_kit(&mut w);
    let t = d.token.clone();
    let (alice, bob, sharer) = (
        w.kit_holder(&t, SOL),
        w.kit_holder(&t, SOL),
        w.kit_holder(&t, 10 * SOL),
    );
    // Refused while nobody is eligible: no holder, then holders below min_eligible.
    w.kit_share(&sharer, &t, SOL)
        .expect_code(kit_code(KitError::NoEligibleHolders));
    buy(&mut w, &d, &alice.pubkey(), MIN_ELIGIBLE / 2);
    w.kit_share(&sharer, &t, SOL)
        .expect_code(kit_code(KitError::NoEligibleHolders));
    buy(&mut w, &d, &alice.pubkey(), MIN_ELIGIBLE * 3 / 2);

    // 3.6 SOL shared at t0.
    let shared = 3_600_000_000u64;
    let t0 = w.env.now;
    let tx = w.kit_share(&sharer, &t, shared);
    tx.ok();
    println!("share: CU {} size {}", tx.cu(), tx.size);
    let ev: RewardsShared = tx.event();
    assert_eq!(
        (ev.amount, ev.total_shared, ev.from),
        (shared, shared, sharer.pubkey())
    );
    let c = w.env.kit_config(&t.mint);
    assert_eq!(
        (
            c.stream_remaining,
            c.stream_last,
            c.stream_end,
            c.seen,
            c.total_shared
        ),
        (shared, t0, t0 + SHARE_STREAM_SECS, shared, shared)
    );
    // Nothing is claimable at once: a share is not fresh, it streams.
    assert_eq!(w.env.claimable(&t, &alice.pubkey()), 0);
    // It releases linearly: a tenth of the hour, a tenth of the share.
    for secs in [1i64, 7, 360] {
        let mut probe = c.clone();
        probe.stream_last = t0;
        let released = bordrless_kit::mirror::released_at(&probe, t0 + secs).unwrap();
        assert_eq!(
            released,
            (u128::from(shared) * secs as u128 / SHARE_STREAM_SECS as u128) as u64
        );
    }

    // At t0 + 600 alice, the only holder, has a sixth.
    w.env.warp(600);
    assert_eq!(w.env.claimable(&t, &alice.pubkey()), 600_000_000);
    let mut report = WalkReport::default();
    claim_as_mirrored(&mut w, &t, &alice, &mut report);
    assert_eq!(report.claimed, 600_000_000);
    // bob buys as much as alice holds, after the share: he only shares what is released after his
    // buy, half of it.
    buy(&mut w, &d, &bob.pubkey(), MIN_ELIGIBLE * 2);
    w.env.warp(1_200);
    assert_eq!(w.env.claimable(&t, &alice.pubkey()), 600_000_000);
    assert_eq!(w.env.claimable(&t, &bob.pubkey()), 600_000_000);
    w.env.warp(SHARE_STREAM_SECS);
    assert_eq!(w.env.claimable(&t, &alice.pubkey()), 1_500_000_000);
    assert_eq!(w.env.claimable(&t, &bob.pubkey()), 1_500_000_000);
    claim_as_mirrored(&mut w, &t, &alice, &mut report);
    claim_as_mirrored(&mut w, &t, &bob, &mut report);
    assert_eq!(report.claimed, shared);
    assert_eq!(w.env.vault_amount(&t), 0);
    let c = w.env.kit_config(&t.mint);
    assert_eq!((c.stream_remaining, c.total_distributed), (0, shared));

    // A second share while the first streams waits for it: 0.36 SOL, then 0.36 SOL half an hour
    // later. The first streams on to its own end at its own rate (0.0001 SOL/s); the second
    // streams over the hour after it at its own rate (fix1 merged the two, and once the first was
    // out released the second at the first's rate plus its own, 0.0002 SOL/s).
    let second = MIN_SHARE_LAMPORTS * 360;
    w.kit_share(&sharer, &t, second).ok();
    let first_end = w.env.now + SHARE_STREAM_SECS;
    w.env.warp(1_800);
    w.kit_share(&sharer, &t, second).ok();
    let c = w.env.kit_config(&t.mint);
    assert_eq!(
        (c.stream_remaining, c.stream_next, c.stream_end),
        (second / 2, second, first_end)
    );
    let released = |secs: i64| bordrless_kit::mirror::released_at(&c, first_end + secs).unwrap();
    assert_eq!(released(-1), second / 2 - second / 2 / 1_800);
    assert_eq!(released(0), second / 2);
    assert_eq!(released(900), second / 2 + second / 4);
    assert_eq!(released(SHARE_STREAM_SECS), second / 2 + second);
    check_kit(&w.env, &t, &[alice.pubkey(), bob.pubkey(), sharer.pubkey()]);
    // On chain: alice and bob hold alike and each claims half of what was released, the first
    // share whole and a quarter of the second at its end + 900 s, the rest an hour later.
    w.env.warp(first_end + 900 - w.env.now);
    claim_as_mirrored(&mut w, &t, &alice, &mut report);
    claim_as_mirrored(&mut w, &t, &bob, &mut report);
    assert_eq!(report.claimed, shared + second + second / 4);
    let c = w.env.kit_config(&t.mint);
    assert_eq!(
        (c.stream_remaining, c.stream_next, c.stream_end),
        (second * 3 / 4, 0, first_end + SHARE_STREAM_SECS)
    );
    w.env.warp(SHARE_STREAM_SECS);
    claim_as_mirrored(&mut w, &t, &alice, &mut report);
    claim_as_mirrored(&mut w, &t, &bob, &mut report);
    assert_eq!(report.claimed, shared + 2 * second);
    let c = w.env.kit_config(&t.mint);
    assert_eq!(
        (c.stream_remaining, c.stream_next, c.total_distributed),
        (0, 0, shared + 2 * second)
    );
    check_kit(&w.env, &t, &[alice.pubkey(), bob.pubkey(), sharer.pubkey()]);
}

#[test]
fn a_share_made_as_another_ends_streams_its_own_hour() {
    // The review of fix1, on chain: 36 SOL shared, then 1 SOL one second before the 36 SOL's hour
    // ends. fix1 merged the two into one stream at the sum of their rates and released the 1 SOL
    // within 98 s (37 times its own rate). Now it waits for the 36 SOL's last second and streams
    // its own hour: in the next L seconds at most that last second and L seconds of the 1 SOL at
    // its own rate are released.
    let mut w = World::new();
    let d = rewards_kit(&mut w);
    let t = d.token.clone();
    let (alice, sharer, victim) = (
        w.kit_holder(&t, SOL),
        w.kit_holder(&t, 40 * SOL),
        w.kit_holder(&t, 2 * SOL),
    );
    buy(&mut w, &d, &alice.pubkey(), 2 * MIN_ELIGIBLE);
    let (big, small) = (36 * SOL, SOL);
    let hour = SHARE_STREAM_SECS;
    let t1 = w.env.now;
    w.kit_share(&sharer, &t, big).ok();
    w.env.warp(hour - 1);
    w.kit_share(&victim, &t, small).ok();
    let at = w.env.now;
    let tail = big / hour as u64;
    let c = w.env.kit_config(&t.mint);
    assert_eq!(
        (c.stream_remaining, c.stream_next, c.stream_end),
        (tail, small, t1 + hour)
    );
    let d0 = c.total_distributed;
    let mut report = WalkReport::default();
    let mut syncs = 0u64;
    for secs in [1i64, 10, 98, 600, 1_800, 3_600, 3_601] {
        // alice's claim syncs the stream on chain.
        w.env.warp(at + secs - w.env.now);
        claim_as_mirrored(&mut w, &t, &alice, &mut report);
        syncs += 1;
        let released = w.env.kit_config(&t.mint).total_distributed - d0;
        assert!(
            u128::from(released) * hour as u128
                <= u128::from(tail) * hour as u128 + u128::from(small) * secs as u128,
            "{secs} s after the share: {released} released"
        );
        let own = (u128::from(small) * (secs - 1).clamp(0, hour) as u128 / hour as u128) as u64;
        assert!(
            released + syncs >= tail + own,
            "{secs} s after the share: {released} released, behind its hour"
        );
        println!("{secs} s after the 1 SOL share: {released} lamports released");
    }
    let c = w.env.kit_config(&t.mint);
    assert_eq!(
        (c.stream_remaining, c.stream_next, c.total_distributed),
        (0, 0, big + small)
    );
    assert!(report.claimed + 2 >= big + small && report.claimed <= big + small);
    check_kit(
        &w.env,
        &t,
        &[alice.pubkey(), sharer.pubkey(), victim.pubkey()],
    );
}

#[test]
fn a_later_share_never_stretches_an_earlier_one() {
    // The review's griefing: 3.6 SOL shared, then the smallest share (0.001 SOL) again and again
    // within the hour. With §4.10's restart a third of the 3.6 SOL was still streaming at the end
    // of its hour, and every small share pushed it on again. Now each small share waits for the
    // 3.6 SOL, whose end never moves, and they stream together over the hour after it.
    for gap in [600i64, 60] {
        let mut w = World::new();
        let d = rewards_kit(&mut w);
        let t = d.token.clone();
        let (alice, sharer, mallory) = (
            w.kit_holder(&t, SOL),
            w.kit_holder(&t, 10 * SOL),
            w.kit_holder(&t, SOL),
        );
        buy(&mut w, &d, &alice.pubkey(), 2 * MIN_ELIGIBLE);
        let big = 3_600_000_000u64;
        let t0 = w.env.now;
        w.kit_share(&sharer, &t, big).ok();
        let mut small = 0u64;
        let mut shares = 0;
        let mut at = gap;
        while at < SHARE_STREAM_SECS {
            w.env.warp(gap);
            w.kit_share(&mallory, &t, MIN_SHARE_LAMPORTS).ok();
            small += MIN_SHARE_LAMPORTS;
            shares += 1;
            let c = w.env.kit_config(&t.mint);
            // The big share's end never moves; the small ones wait for it, together.
            assert_eq!(
                (c.stream_end, c.stream_next),
                (t0 + SHARE_STREAM_SECS, small)
            );
            at += gap;
        }
        // The end of the big share's hour: it is all out, alice (the only holder) has it all, and
        // none of the small shares yet.
        w.env.warp(t0 + SHARE_STREAM_SECS - w.env.now);
        let c = w.env.kit_config(&t.mint);
        let still = bordrless_kit::mirror::synced(&c, w.env.vault_amount(&t), w.env.now)
            .unwrap()
            .streaming();
        assert_eq!(
            still, small,
            "gap {gap}: the small shares wait for their hour"
        );
        let claimable = w.env.claimable(&t, &alice.pubkey());
        assert!(
            claimable + 2 >= big && claimable <= big,
            "gap {gap}: claimable {claimable}"
        );
        let mut report = WalkReport::default();
        claim_as_mirrored(&mut w, &t, &alice, &mut report);
        // An hour later the small shares are out too (the scaled remainder keeps at most a
        // lamport).
        w.env.warp(SHARE_STREAM_SECS);
        claim_as_mirrored(&mut w, &t, &alice, &mut report);
        assert!(report.claimed <= big + small && report.claimed + 2 >= big + small);
        let c = w.env.kit_config(&t.mint);
        assert_eq!((c.stream_remaining, c.stream_next), (0, 0));
        println!(
            "a share every {gap} s ({shares} shares, {small} lamports): the 3.6 SOL ended at t0 + \
             {} s, the small shares at t0 + {} s",
            SHARE_STREAM_SECS,
            2 * SHARE_STREAM_SECS
        );
        check_kit(
            &w.env,
            &t,
            &[alice.pubkey(), sharer.pubkey(), mallory.pubkey()],
        );
    }
}

#[test]
fn a_newcomer_cannot_take_a_paused_share_in_one_transaction() {
    // The review's sequence: two holders above the threshold and a small one below it on its own;
    // 10 SOL shared; the two sell out after ten minutes; the small holder holds alone, below the
    // threshold, for the rest of the hour. Before the fix the stream kept releasing into `held`
    // meanwhile, and a newcomer took it in one transaction: buy past the threshold, claim, sell.
    // Now the stream waits while nobody is eligible.
    let mut w = World::new();
    let d = rewards_kit(&mut w);
    let t = d.token.clone();
    let (a, b, small, sharer, newcomer) = (
        w.kit_holder(&t, SOL),
        w.kit_holder(&t, SOL),
        w.kit_holder(&t, SOL),
        w.kit_holder(&t, 20 * SOL),
        w.kit_holder(&t, SOL),
    );
    buy(&mut w, &d, &a.pubkey(), 2 * MIN_ELIGIBLE);
    buy(&mut w, &d, &b.pubkey(), 2 * MIN_ELIGIBLE);
    buy(&mut w, &d, &small.pubkey(), MIN_ELIGIBLE / 3);
    let shared = 10 * SOL;
    w.kit_share(&sharer, &t, shared).ok();
    w.env.warp(600);
    for seller in [&a, &b] {
        w.send_tokens(seller, t.mint, &t.pool, 2 * MIN_ELIGIBLE)
            .ok();
        w.kit_claim(seller, &t).ok();
    }
    let c = w.env.kit_config(&t.mint);
    assert!(c.eligible < c.min_eligible);
    let streaming = c.stream_remaining;
    assert!(streaming >= shared / 6 * 5 - 1);
    w.env.warp(3_000);
    let small_before = w.env.claimable(&t, &small.pubkey());
    let vault_before = w.env.vault_amount(&t);

    // Buy past the threshold, claim, sell back: one transaction. The claim finds nothing.
    let pool = d.pool.insecure_clone();
    let ixs = [
        w.kit_transfer_ix(
            t.mint,
            pool.pubkey(),
            &t.pool,
            &newcomer.pubkey(),
            MIN_ELIGIBLE,
        ),
        bordrless_kit::client::claim(newcomer.pubkey(), t.mint, t.reward_mint),
        w.kit_transfer_ix(
            t.mint,
            newcomer.pubkey(),
            &newcomer.pubkey(),
            &t.pool,
            MIN_ELIGIBLE,
        ),
    ];
    let tx = w.env.send_paid_by(&ixs, &newcomer, &[&pool]);
    assert!(
        refused_by_kit(&tx, KitError::NothingToClaim),
        "{}",
        tx.logs().join("\n")
    );
    assert_eq!(w.env.vault_amount(&t), vault_before);
    assert_eq!(w.env.claimable(&t, &small.pubkey()), small_before);
    let c = w.env.kit_config(&t.mint);
    assert_eq!((c.held, c.stream_remaining), (0, streaming));

    // A newcomer who buys and holds ten minutes gets its share of those ten minutes, with the
    // small holder: a fifth of what the stream had left, in proportion to what each holds.
    buy(&mut w, &d, &newcomer.pubkey(), MIN_ELIGIBLE);
    w.env.warp(600);
    let fair = (u128::from(streaming / 5) * u128::from(MIN_ELIGIBLE)
        / u128::from(MIN_ELIGIBLE + MIN_ELIGIBLE / 3)) as u64;
    let mut report = WalkReport::default();
    claim_as_mirrored(&mut w, &t, &newcomer, &mut report);
    assert!(
        report.claimed + 2 >= fair && report.claimed <= fair + 4,
        "claimed {}, fair {fair}",
        report.claimed
    );
    check_kit(
        &w.env,
        &t,
        &[
            a.pubkey(),
            b.pubkey(),
            small.pubkey(),
            sharer.pubkey(),
            newcomer.pubkey(),
        ],
    );
}

#[test]
fn donations_are_divided_at_the_next_sync_or_held() {
    let mut w = World::new();
    let d = rewards_kit(&mut w);
    let t = d.token.clone();
    let (a, b) = (w.kit_holder(&t, SOL), w.kit_holder(&t, SOL));
    let donor = w.wallet_with_sol(10 * SOL);
    // Nobody eligible: a donation is held, not lost.
    w.donate(&donor, &t, SOL).ok();
    buy(&mut w, &d, &a.pubkey(), MIN_ELIGIBLE - 1);
    let c = w.env.kit_config(&t.mint);
    assert_eq!((c.held, c.total_distributed), (SOL, 0));
    assert_eq!(w.env.claimable(&t, &a.pubkey()), 0);
    // Below min_eligible it keeps waiting.
    w.donate(&donor, &t, SOL).ok();
    buy(&mut w, &d, &b.pubkey(), 1);
    assert_eq!(w.env.kit_config(&t.mint).held, 2 * SOL);
    // Once holders hold min_eligible, the next sync divides all of it among them.
    buy(&mut w, &d, &b.pubkey(), MIN_ELIGIBLE);
    let c = w.env.kit_config(&t.mint);
    assert_eq!((c.held, c.total_distributed), (0, 2 * SOL));
    // a held MIN - 1 and b 1 when it was divided: b's buy came after the sync.
    let a_share =
        (u128::from(2 * SOL) * u128::from(MIN_ELIGIBLE - 1) / u128::from(MIN_ELIGIBLE)) as u64;
    let claimable = w.env.claimable(&t, &a.pubkey());
    assert!(claimable <= a_share && claimable + 1 >= a_share);
    let mut report = WalkReport::default();
    claim_as_mirrored(&mut w, &t, &a, &mut report);
    claim_as_mirrored(&mut w, &t, &b, &mut report);
    assert!(report.claimed <= 2 * SOL);
    assert!(report.claimed >= 2 * SOL - 2);
    check_kit(&w.env, &t, &[a.pubkey(), b.pubkey()]);
}

#[test]
fn a_holder_who_sells_out_claims_and_closes_the_holding() {
    let mut w = World::new();
    let d = rewards_kit(&mut w);
    let t = d.token.clone();
    let (a, b) = (w.kit_holder(&t, SOL), w.kit_holder(&t, 3 * SOL));
    buy(&mut w, &d, &a.pubkey(), MIN_ELIGIBLE);
    buy(&mut w, &d, &b.pubkey(), MIN_ELIGIBLE);
    w.donate(&b, &t, SOL).ok();
    // a sells everything: settled at its balance before the sell, its snapshot cleared.
    w.send_tokens(&a, t.mint, &t.pool, MIN_ELIGIBLE).ok();
    let left = HolderData::read(&w.env.holding_state(&t.mint, &a.pubkey()).1);
    assert_eq!(
        (left.owed, left.snapshot, left.early_locked),
        (SOL / 2, 0, 0)
    );
    // Later inflows are not a's.
    w.donate(&b, &t, SOL).ok();
    assert_eq!(w.env.claimable(&t, &a.pubkey()), SOL / 2);
    let holding = token::holding_address(&t.mint, &a.pubkey());
    let close = token::close_holding(a.pubkey(), t.mint, holding, a.pubkey());
    w.env
        .send_paid_by(std::slice::from_ref(&close), &a, &[])
        .expect_code(u32::from(
            bordrless_token::error::TokenError::HookDataNotEmpty,
        ));
    let tx = w.kit_claim(&a, &t);
    tx.ok();
    println!("claim after selling out: CU {} size {}", tx.cu(), tx.size);
    let ev: RewardsClaimed = tx.event();
    assert_eq!((ev.amount, ev.owed_left), (SOL / 2, 0));
    assert_eq!(w.env.holding_state(&t.mint, &a.pubkey()), (0, [0; 64]));
    w.env.send_paid_by(&[close], &a, &[]).ok();
    assert!(w.env.account(&holding).is_none());
    // b's share of both donations is intact.
    assert_eq!(w.env.claimable(&t, &b.pubkey()), SOL / 2 + SOL);
    check_kit(&w.env, &t, &[a.pubkey(), b.pubkey()]);
}

fn direct_walk(modules: u8, seed: u64, steps: usize) -> WalkReport {
    let mut w = World::new();
    let d = w.direct_kit(&KitSpec::new(modules));
    let payer = w.wallet_with_sol(10_000 * SOL);
    let mut market = DirectMarket {
        pool: d.pool.insecure_clone(),
        payer,
        units_per_lamport: 10_000,
    };
    let allowed = [
        KitError::MaxWalletExceeded,
        KitError::CreatorLocked,
        KitError::EarlyLocked,
    ]
    .into_iter()
    .filter(|e| match e {
        KitError::MaxWalletExceeded => modules & modules::MAX_WALLET != 0,
        KitError::CreatorLocked => modules & modules::CREATOR_WALLET_LOCK != 0,
        _ => modules & modules::EARLY_BUYER_LOCK != 0,
    })
    .collect();
    // With max wallet (a cap of 20 * MIN_ELIGIBLE) buys are large enough for the cap to bite.
    let buy_max = if modules & modules::MAX_WALLET != 0 {
        12 * MIN_ELIGIBLE
    } else {
        3 * MIN_ELIGIBLE
    };
    let walk = Walk {
        seed,
        steps,
        holders: 5,
        holder_sol: 50 * SOL,
        buy_max,
        thin_buy_max: MIN_ELIGIBLE / 10,
        phase: 40,
        allowed,
    };
    let report = money_walk(&mut w, &d.token, &mut market, &walk);
    println!("modules {modules} seed {seed}: {report:?}");
    report
}

#[test]
fn seeded_money_walks_keep_the_vault_solvent() {
    for (modules, seed, steps) in [
        (modules::HOLDER_REWARDS, 11, 500),
        (modules::HOLDER_REWARDS, 12, 500),
        (modules::HOLDER_REWARDS | modules::EARLY_BUYER_LOCK, 13, 300),
        (modules::ALL, 14, 300),
    ] {
        let r = direct_walk(modules, seed, steps);
        assert!(r.claims > 10, "claims paid");
        assert!(
            r.empty_claims > 0,
            "claims of nothing were refused as the mirror said"
        );
        assert!(r.below_min_steps > 0, "a stretch below min_eligible");
        assert!(r.inflows > 0 && r.claimed > 0);
        assert!(r.ops.get("share").copied().unwrap_or(0) > 0);
        assert!(r.ops.get("burn").copied().unwrap_or(0) > 0);
        if modules == modules::ALL {
            assert!(!r.refused.is_empty(), "the rules refused something");
        }
    }
}

#[test]
fn a_walk_without_holder_rewards_keeps_the_counter() {
    // Max wallet and the early-buyer lock: no vault; eligible still follows holders (burns are
    // seen through the early-buyer lock's flags).
    let r = direct_walk(modules::MAX_WALLET | modules::EARLY_BUYER_LOCK, 21, 200);
    assert_eq!((r.inflows, r.claimed), (0, 0));
    assert!(r.ops.get("burn").copied().unwrap_or(0) > 0);
}
