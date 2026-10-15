//! Snowball (2026-10-12): a companion whose pot makes volume in its own token. It buys a share of
//! the pot when the price drops its buy step below the reference and sells a share of its tokens
//! when it rises its sell step above, the reference moving to the price each trade leaves; with flat
//! trades it also trades every interval when the price has not moved, on the side that rebalances.

use anchor_lang::prelude::Pubkey;
use bordrless_companion::client as companion;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::{PotFunded, SnowballFollowed, SnowballTraded};
use bordrless_companion::instructions::{CreateArgs, CreateSnowballArgs};
use bordrless_companion::state::{spot_price, Companion, Snowball, Split};
use bordrless_core::policy;
use bordrless_launch::client::{self as launch, LaunchKeys};
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use solana_keypair::Keypair;
use solana_signer::Signer;

const CREATOR_FEE: u16 = 200;
const STEP: u16 = 200;
const SIZE: u16 = 2_500;

fn code(e: CompanionError) -> u32 {
    u32::from(e)
}

/// 70% of every fee claim to the pot, 30% to the launcher; 2% steps; a quarter a trade; a minute
/// apart; up to 5% of the pool a trade; trades only on a step (flat trades off).
fn terms() -> CreateSnowballArgs {
    CreateSnowballArgs {
        split: Split {
            buyback_bps: 0,
            holders_bps: 0,
            beneficiary_bps: 3_000,
        },
        pot_bps: 7_000,
        buy_step_bps: STEP,
        sell_step_bps: STEP,
        buy_bps: SIZE,
        sell_bps: SIZE,
        interval: 60,
        max_trade: 100 * SOL,
        pool_bps: 500,
        flat_trades: false,
    }
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

/// A Snowball companion for a fresh mint, then the launch through it. Answers the mint.
fn launch_snowball(
    w: &mut World,
    launcher: &Keypair,
    args: CreateSnowballArgs,
    rules: LaunchRules,
) -> (Pubkey, Tx) {
    let mint = Keypair::new();
    let ixs = [
        companion::create(
            launcher.pubkey(),
            launcher.pubkey(),
            mint.pubkey(),
            create_args(),
        ),
        companion::create_snowball(launcher.pubkey(), mint.pubkey(), args),
    ];
    w.env.send_paid_by(&ixs, launcher, &[&mint]).ok();
    let creator = companion::creator_address(&mint.pubkey());
    let launch_args = World::launch_args("SNOW", CREATOR_FEE, VQ, rules);
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
    let tx = w.env.send_paid_by(&[ix], launcher, &[&mint]);
    (mint.pubkey(), tx)
}

fn keys(w: &World, mint: &Pubkey) -> LaunchKeys {
    LaunchKeys::of(&w.launch(mint))
}

fn companion_of(w: &World, mint: &Pubkey) -> Companion {
    w.env.read(&companion::companion_address(mint))
}

fn snowball_of(w: &World, mint: &Pubkey) -> Snowball {
    w.env.read(&companion::snowball_address(mint))
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

/// The pool's SOL side, real and virtual.
fn quote_side(w: &World, mint: &Pubkey) -> u64 {
    let p = w.launch_pool(mint);
    p.quote_reserve + p.virtual_quote
}

fn step(w: &mut World, cranker: &Keypair, mint: &Pubkey) -> Tx {
    let ix = companion::snowball_step(cranker.pubkey(), &keys(w, mint));
    w.env.send_paid_by(&[ix], cranker, &[])
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

/// Fees into the pot: trades, a whale's buy, a claim. Answers the whale and a cranker.
fn fill_pot(w: &mut World, mint: &Pubkey) -> (Keypair, Keypair) {
    w.env.warp(31);
    trade(w, mint, 4, 2 * SOL);
    let whale = w.wallet_with_sol(30 * SOL);
    w.buy(&whale, mint, 5 * SOL).ok();
    let cranker = w.wallet_with_sol(SOL);
    let tx = w.env.send_paid_by(
        &[companion::claim_fees(cranker.pubkey(), *mint, None)],
        &cranker,
        &[],
    );
    tx.ok();
    let funded: PotFunded = tx.event();
    assert!(funded.to_pot > 0);
    assert_eq!(funded.to_buyback, 0);
    assert_eq!(companion_of(w, mint).pending_pot, funded.to_pot);
    (whale, cranker)
}

/// On steps only: a rise with nothing to sell is followed; a dip is bought; a rise is sold; a price
/// within the steps waits.
fn on_steps(w: &mut World, mint: &Pubkey) {
    let (whale, cranker) = fill_pot(w, mint);
    // Not before a minute after the launch.
    step(w, &cranker, mint).expect_code(code(CompanionError::SnowballNotDue));
    w.env.warp(60);

    // The trades left the price far above the opening, and it holds no tokens to sell: it follows.
    let price = spot(w, mint);
    let tx = step(w, &cranker, mint);
    tx.ok();
    let followed: SnowballFollowed = tx.event();
    assert_eq!(followed.price, price);
    assert_eq!(snowball_of(w, mint).reference_price, price);
    assert_eq!(tx.events::<SnowballTraded>().len(), 0);
    // Not again within the interval.
    step(w, &cranker, mint).expect_code(code(CompanionError::SnowballNotDue));

    // A dump takes the price more than a step below: it buys a quarter of the pot.
    let held = w.env.holding(mint, &whale.pubkey());
    w.sell(&whale, mint, held / 2).ok();
    assert!(spot(w, mint) < Snowball::buy_below(price, STEP));
    w.env.warp(60);
    let pot = companion_of(w, mint).pending_pot;
    let tx = step(w, &cranker, mint);
    tx.ok();
    let buy: SnowballTraded = tx.event();
    assert_eq!((buy.side, buy.on_step), (1, true));
    let total = pot * u64::from(SIZE) / 10_000;
    assert_eq!(buy.sol + buy.bounty, total);
    assert_eq!(buy.bounty, total * 50 / 10_000);
    assert!(buy.tokens > 0);
    let s = snowball_of(w, mint);
    assert_eq!(s.tokens_held, buy.tokens);
    assert_eq!(s.reference_price, spot(w, mint));
    assert_eq!(s.reference_price, buy.price_after);
    assert!(buy.price_after > buy.price_before, "a buy lifts the price");
    assert_eq!(companion_of(w, mint).pending_pot, pot - total);
    // Within its steps of the reference, flat trades off: nothing due.
    w.env.warp(60);
    step(w, &cranker, mint).expect_code(code(CompanionError::SnowballNotDue));

    // A pump takes the price more than a step above: it sells a quarter of its tokens.
    w.buy(&whale, mint, 3 * SOL).ok();
    let reference = snowball_of(w, mint).reference_price;
    assert!(spot(w, mint) > Snowball::sell_above(reference, STEP));
    let pot = companion_of(w, mint).pending_pot;
    let before = w.env.lamports(&cranker.pubkey());
    let tx = step(w, &cranker, mint);
    tx.ok();
    let sell: SnowballTraded = tx.event();
    assert_eq!((sell.side, sell.on_step), (0, true));
    assert_eq!(sell.bounty, (sell.sol + sell.bounty) * 50 / 10_000);
    // The holding exists by now (the first buy made it, the cranker paying its rent): the bounty is paid in SOL.
    assert_eq!(
        w.env.lamports(&cranker.pubkey()) + 5_000,
        before + sell.bounty
    );
    assert_eq!(sell.tokens, buy.tokens * u64::from(SIZE) / 10_000);
    assert!(sell.sol > 0);
    assert!(
        sell.price_after < sell.price_before,
        "a sell lowers the price"
    );
    let s = snowball_of(w, mint);
    assert_eq!(s.tokens_held, buy.tokens - sell.tokens);
    assert_eq!(s.reference_price, sell.price_after);
    assert_eq!(companion_of(w, mint).pending_pot, pot + sell.sol);
    assert_eq!((s.buys, s.sells), (1, 1));
    assert_eq!(s.sol_spent, buy.sol);
    assert_eq!(s.sol_received, sell.sol);
    println!("snowball step: CU {} size {}", tx.cu(), tx.size);
}

#[test]
fn a_snowball_buys_drops_and_sells_rises() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(5 * SOL);
    let (mint, tx) = launch_snowball(&mut w, &launcher, terms(), LaunchRules::NONE);
    tx.ok();
    let c = companion_of(&w, &mint);
    assert!(c.is_snowball() && c.launched && !c.is_game());
    assert_eq!(c.pot_bps, 7_000);
    assert_eq!(c.split.beneficiary_bps, 3_000);
    on_steps(&mut w, &mint);
}

#[test]
fn a_snowball_with_kit_rules_never_sells_the_dev_bag() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(10 * SOL);
    // Holder rewards and a burn on both sides: the kit runs on every trade.
    let (mint, tx) = launch_snowball(&mut w, &launcher, terms(), presets::burn());
    tx.ok();
    let k = keys(&w, &mint);
    let creator = companion::creator_address(&mint);
    w.env
        .send_paid_by(
            &[companion::dev_buy(launcher.pubkey(), &k, SOL / 4, 1)],
            &launcher,
            &[],
        )
        .ok();
    let bag = companion_of(&w, &mint).dev_tokens;
    assert!(bag > 0);
    on_steps(&mut w, &mint);
    // The creator's holding is the dev bag and the market maker's tokens, apart.
    let s = snowball_of(&w, &mint);
    assert_eq!(w.env.holding(&mint, &creator), bag + s.tokens_held);
}

/// Flat trades: every interval it trades even when nobody else does, buying while its tokens are
/// worth less than its SOL and selling once they are worth more, so the pot keeps cycling.
#[test]
fn flat_trades_keep_the_pot_cycling_every_interval() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(5 * SOL);
    let args = CreateSnowballArgs {
        interval: 10,
        flat_trades: true,
        ..terms()
    };
    let (mint, tx) = launch_snowball(&mut w, &launcher, args, LaunchRules::NONE);
    tx.ok();
    let (_, cranker) = fill_pot(&mut w, &mint);
    w.env.warp(60);
    let mut sides = Vec::new();
    for i in 0..8 {
        let tx = step(&mut w, &cranker, &mint);
        tx.ok();
        let t: SnowballTraded = tx.event();
        // Nobody else trades: its own trades are the only moves, so none passes a step but the
        // first (the price ran far above the opening reference, and a sell is called with nothing
        // to sell: it buys instead, as a flat trade).
        assert!(!t.on_step, "trade {i} on a step");
        sides.push(t.side);
        // Not again within the 10 s interval.
        step(&mut w, &cranker, &mint).expect_code(code(CompanionError::SnowballNotDue));
        w.env.warp(10);
    }
    // Buys while its tokens are worth less than its SOL (a quarter at a time: three), then a sell
    // once they are worth more, and on around the balance.
    assert_eq!(&sides[..4], &[1, 1, 1, 0], "{sides:?}");
    assert!(
        sides[4..].contains(&0) && sides[4..].contains(&1),
        "{sides:?}"
    );
    let s = snowball_of(&w, &mint);
    assert_eq!(s.buys + s.sells, 8);
}

/// The launcher's cap: a trade moves at most `pool_bps` of the pool's SOL side.
#[test]
fn a_trade_is_capped_at_the_launchers_share_of_the_pool() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(5 * SOL);
    let args = CreateSnowballArgs {
        buy_bps: 10_000,
        pool_bps: 25,
        flat_trades: true,
        ..terms()
    };
    let (mint, tx) = launch_snowball(&mut w, &launcher, args, LaunchRules::NONE);
    tx.ok();
    let (_, cranker) = fill_pot(&mut w, &mint);
    w.env.warp(60);
    let side = quote_side(&w, &mint);
    let pot = companion_of(&w, &mint).pending_pot;
    let tx = step(&mut w, &cranker, &mint);
    tx.ok();
    let t: SnowballTraded = tx.event();
    let cap = side * 25 / 10_000;
    assert!(pot > cap, "the pot is above the cap");
    assert_eq!(t.sol + t.bounty, cap);
    assert_eq!(companion_of(&w, &mint).pending_pot, pot - cap);
}

#[test]
fn what_a_snowball_refuses() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(10 * SOL);

    let try_create = |w: &mut World, args: CreateSnowballArgs| -> Tx {
        let mint = Keypair::new();
        let ixs = [
            companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                mint.pubkey(),
                create_args(),
            ),
            companion::create_snowball(launcher.pubkey(), mint.pubkey(), args),
        ];
        w.env.send_paid_by(&ixs, &launcher, &[&mint])
    };
    // The split with the pot must add up, with a pot.
    let mut a = terms();
    a.pot_bps = 6_000;
    try_create(&mut w, a).expect_code(code(CompanionError::BadSplit));
    let mut a = terms();
    a.pot_bps = 0;
    a.split.beneficiary_bps = 10_000;
    try_create(&mut w, a).expect_code(code(CompanionError::BadSplit));
    // Terms out of bounds: steps under 0.5% or over 20%, a size over 100% or under 1%, a cap over
    // 10% or under 0.25% of the pool, under 10 s apart, a trade cap under 0.01 SOL.
    for bad in [
        CreateSnowballArgs {
            buy_step_bps: 49,
            ..terms()
        },
        CreateSnowballArgs {
            sell_step_bps: 2_001,
            ..terms()
        },
        CreateSnowballArgs {
            buy_bps: 10_001,
            ..terms()
        },
        CreateSnowballArgs {
            sell_bps: 99,
            ..terms()
        },
        CreateSnowballArgs {
            pool_bps: 1_001,
            ..terms()
        },
        CreateSnowballArgs {
            pool_bps: 24,
            ..terms()
        },
        CreateSnowballArgs {
            interval: 9,
            ..terms()
        },
        CreateSnowballArgs {
            max_trade: SOL / 100 - 1,
            ..terms()
        },
    ] {
        try_create(&mut w, bad).expect_code(code(CompanionError::BadSnowball));
    }
    // The widest terms are taken: the whole pot a trade, 10% of the pool, every 10 s.
    try_create(
        &mut w,
        CreateSnowballArgs {
            buy_bps: 10_000,
            sell_bps: 10_000,
            pool_bps: 1_000,
            interval: 10,
            flat_trades: true,
            ..terms()
        },
    )
    .ok();

    // Not after the launch, and not twice.
    let (_mint, tx) = launch_snowball(&mut w, &launcher, terms(), LaunchRules::NONE);
    tx.ok();
    let classic = Keypair::new();
    w.env
        .send_paid_by(
            &[companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                classic.pubkey(),
                create_args(),
            )],
            &launcher,
            &[&classic],
        )
        .ok();
    let ok = companion::create_snowball(launcher.pubkey(), classic.pubkey(), terms());
    w.env.send_paid_by(&[ok], &launcher, &[&classic]).ok();
    let again = companion::create_snowball(launcher.pubkey(), classic.pubkey(), terms());
    w.env
        .send_paid_by(&[again], &launcher, &[&classic])
        .expect_fail();
}

// ---- Native SOL (phase 3) ---------------------------------------------------------------------------

/// A Snowball migrated in place mid-life, its pot full: its buys and sells run through `swap_v2`
/// with the creator address trading its own lamports, and every step moves its lamports exactly as
/// its pot (`pending_pot` and its other buckets always equal its SOL).
#[test]
fn a_snowball_migrated_mid_life_trades_its_lamports() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(5 * SOL);
    let (mint, tx) = launch_snowball(&mut w, &launcher, terms(), LaunchRules::NONE);
    tx.ok();
    let (whale, cranker) = fill_pot(&mut w, &mint);
    w.env.warp(60);
    step(&mut w, &cranker, &mint).ok();
    let pot = companion_of(&w, &mint).pending_pot;
    w.migrate_companion_native(&mint).ok();
    let c = companion_of(&w, &mint);
    assert!(c.is_native());
    assert_eq!(c.pending_pot, pot);
    assert_eq!(c.set_aside().unwrap(), w.creator_sol(&mint).1);
    let creator = companion::creator_address(&mint);
    let native_step = |w: &mut World| {
        let ix = companion::native(
            companion::snowball_step(cranker.pubkey(), &keys(w, &mint)),
            &keys(w, &mint),
        );
        w.env.send_paid_by(&[ix], &cranker, &[])
    };

    // A dump: a buy, paid from the creator's lamports.
    let held = w.env.holding(&mint, &whale.pubkey());
    let payer = w.env.funded(10 * SOL);
    let sell = launch::swap_v2(
        &keys(&w, &mint),
        whale.pubkey(),
        whale.pubkey(),
        0,
        held / 2,
        0,
    );
    w.env.send_paid_by(&[sell], &payer, &[&whale]).ok();
    w.env.warp(60);
    let (c0, p0) = (
        w.env.lamports(&creator),
        companion_of(&w, &mint).pending_pot,
    );
    let tx = native_step(&mut w);
    tx.ok();
    let buy: SnowballTraded = tx.event();
    assert_eq!(buy.side, 1);
    assert_eq!(c0 - w.env.lamports(&creator), buy.sol + buy.bounty);
    assert_eq!(
        p0 - companion_of(&w, &mint).pending_pot,
        buy.sol + buy.bounty
    );
    assert_eq!(snowball_of(&w, &mint).tokens_held, buy.tokens);
    assert_eq!(
        companion_of(&w, &mint).set_aside().unwrap(),
        w.creator_sol(&mint).1
    );
    w.assert_pool_invariant(&w.launch_pool_key(&mint));

    // A pump: a sell, its SOL to the creator's lamports.
    let pump = launch::swap_v2(
        &keys(&w, &mint),
        whale.pubkey(),
        whale.pubkey(),
        1,
        3 * SOL,
        0,
    );
    w.env.send_paid_by(&[pump], &payer, &[&whale]).ok();
    w.env.warp(60);
    let (c0, p0) = (
        w.env.lamports(&creator),
        companion_of(&w, &mint).pending_pot,
    );
    let tx = native_step(&mut w);
    tx.ok();
    let sell: SnowballTraded = tx.event();
    assert_eq!(sell.side, 0);
    assert_eq!(w.env.lamports(&creator) - c0, sell.sol);
    assert_eq!(companion_of(&w, &mint).pending_pot - p0, sell.sol);
    assert_eq!(
        companion_of(&w, &mint).set_aside().unwrap(),
        w.creator_sol(&mint).1
    );
    assert_eq!(w.creator_sol(&mint).0, 0, "no bridged SOL anywhere");
    w.assert_pool_invariant(&w.launch_pool_key(&mint));
}
