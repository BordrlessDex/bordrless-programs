//! The companion audit's findings (2026-10-08), each kept as a regression test: every attack the
//! audit demonstrated is now refused, or no longer pays.

use anchor_lang::prelude::Pubkey;
use bordrless_companion::client as companion;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::{BoughtBack, BuybackWaited, FeesClaimed};
use bordrless_companion::instructions::CreateArgs;
use bordrless_companion::state::{Companion, Split};
use bordrless_core::policy;
use bordrless_kit::state::KitConfig;
use bordrless_launch::client::{self as launch, LaunchKeys};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_token::client as token;
use solana_keypair::Keypair;
use solana_signer::Signer;

fn cargs(split: Split, vest_secs: i64, max_buyback: u64) -> CreateArgs {
    CreateArgs {
        split,
        bounty_bps: 50,
        max_buyback,
        buyback_interval: 60,
        vest_secs,
        fund: SOL / 2,
    }
}

const BUYBACK: Split = Split {
    buyback_bps: 10_000,
    holders_bps: 0,
    beneficiary_bps: 0,
};
const HOLDERS: Split = Split {
    buyback_bps: 0,
    holders_bps: 10_000,
    beneficiary_bps: 0,
};

fn code(e: CompanionError) -> u32 {
    u32::from(e)
}

fn companion_of(w: &World, mint: &Pubkey) -> Companion {
    w.env.read(&companion::companion_address(mint))
}

fn launch_ix(
    w: &World,
    launcher: &Pubkey,
    mint: &Pubkey,
    rules: LaunchRules,
    fee: u16,
    config: Option<Pubkey>,
) -> anchor_lang::solana_program::instruction::Instruction {
    let creator = companion::creator_address(mint);
    let a = World::launch_args("AUD", fee, VQ, rules);
    let inner = launch::create_launch_with(
        creator,
        *mint,
        w.env.treasury.pubkey(),
        w.sol,
        policy::LP_FEE_BPS,
        a.clone(),
        config,
        None,
    );
    companion::launch(*launcher, *mint, &inner, a)
}

/// Create then launch, as the site does (two transactions). Answers the mint and the launch.
fn launch_through(
    w: &mut World,
    launcher: &Keypair,
    a: CreateArgs,
    rules: LaunchRules,
    fee: u16,
    config: Option<Pubkey>,
) -> (Pubkey, Tx) {
    let mint = Keypair::new();
    w.env
        .send_paid_by(
            &[companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                mint.pubkey(),
                a,
            )],
            launcher,
            &[&mint],
        )
        .ok();
    let ix = launch_ix(w, &launcher.pubkey(), &mint.pubkey(), rules, fee, config);
    let tx = w.env.send_paid_by(&[ix], launcher, &[&mint]);
    (mint.pubkey(), tx)
}

/// H1: a listed config's author could pay the companion outside its steps. Companion launches from
/// such configs are refused, and whatever reaches the creator's holding is split, never stranded.
#[test]
fn author_shares_are_refused_and_nothing_is_stranded() {
    let mut w = World::new();
    let author = w.wallet_with_sol(SOL);
    let listed = Keypair::new();
    let cfg = CreateConfigArgs {
        rules: LaunchRules::NONE,
        creator_fee_bps: 100,
        custom_hook: None,
        custom_hook_flags: 0,
        label: "Listed".into(),
    };
    w.env
        .send_paid_by(
            &[launch::create_listed_config(
                author.pubkey(),
                listed.pubkey(),
                cfg,
                3_000,
            )],
            &author,
            &[&listed],
        )
        .ok();
    let launcher = w.wallet_with_sol(5 * SOL);
    let (_, tx) = launch_through(
        &mut w,
        &launcher,
        cargs(BUYBACK, 0, SOL),
        LaunchRules::NONE,
        100,
        Some(listed.pubkey()),
    );
    tx.expect_code(code(CompanionError::AuthorShareUnsupported));

    // Bridged SOL sent to the creator's holding by anyone is split by the next claim.
    let (mint, tx) = launch_through(
        &mut w,
        &launcher,
        cargs(BUYBACK, 0, SOL),
        LaunchRules::NONE,
        100,
        None,
    );
    tx.ok();
    let donor = w.wallet_with_sol(5 * SOL);
    let creator = companion::creator_address(&mint);
    let donated = SOL / 10;
    w.env
        .send_paid_by(
            &[
                token::create_holding(donor.pubkey(), w.sol, donor.pubkey()),
                bordrless_bridge::client::wrap_sol(donor.pubkey(), donated),
                token::transfer(
                    donor.pubkey(),
                    token::holding_address(&w.sol, &donor.pubkey()),
                    token::holding_address(&w.sol, &creator),
                    w.sol,
                    None,
                    vec![],
                    donated,
                ),
            ],
            &donor,
            &[],
        )
        .ok();
    let tx = w.env.send_paid_by(
        &[companion::claim_fees(donor.pubkey(), mint, None)],
        &donor,
        &[],
    );
    tx.ok();
    assert_eq!(tx.event::<FeesClaimed>().claimed, donated);
}

/// M2: the mint must sign `create`, so nobody can make someone else's companion first.
#[test]
fn only_the_mints_holder_makes_its_companion() {
    let mut w = World::new();
    let attacker = w.wallet_with_sol(5 * SOL);
    let mint = Keypair::new();
    let mut ix = companion::create(
        attacker.pubkey(),
        attacker.pubkey(),
        mint.pubkey(),
        cargs(BUYBACK, 0, SOL),
    );
    for m in ix.accounts.iter_mut().filter(|m| m.pubkey == mint.pubkey()) {
        m.is_signer = false;
    }
    w.env.send_paid_by(&[ix], &attacker, &[]).expect_fail();
    assert!(w
        .env
        .account(&companion::companion_address(&mint.pubkey()))
        .is_none());
}

/// L1: `withdraw` before the launch is refused, so nobody can take the launch's funding away; with
/// the mint's signature it refunds a launch that never happened (round 2, finding 3).
#[test]
fn withdraw_waits_for_the_launch() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(5 * SOL);
    let griefer = w.wallet_with_sol(SOL);
    let mint = Keypair::new();
    w.env
        .send_paid_by(
            &[companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                mint.pubkey(),
                cargs(BUYBACK, 0, SOL),
            )],
            &launcher,
            &[&mint],
        )
        .ok();
    w.env
        .send_paid_by(
            &[companion::withdraw(
                griefer.pubkey(),
                mint.pubkey(),
                launcher.pubkey(),
            )],
            &griefer,
            &[],
        )
        .expect_code(code(CompanionError::NotLaunched));
    let creator = companion::creator_address(&mint.pubkey());
    let funded = w.env.lamports(&creator);
    let before = w.env.lamports(&launcher.pubkey());
    w.env
        .send_paid_by(
            &[companion::refund(
                griefer.pubkey(),
                mint.pubkey(),
                launcher.pubkey(),
            )],
            &griefer,
            &[&mint],
        )
        .ok();
    let refunded = w.env.lamports(&launcher.pubkey()) - before;
    assert!(refunded > 0 && refunded == funded - w.env.lamports(&creator));
    // Funded again, it still launches.
    w.env
        .send_paid_by(
            &[anchor_lang::solana_program::system_instruction::transfer(
                &launcher.pubkey(),
                &creator,
                SOL / 2,
            )],
            &launcher,
            &[],
        )
        .ok();
    let ix = launch_ix(
        &w,
        &launcher.pubkey(),
        &mint.pubkey(),
        LaunchRules::NONE,
        200,
        None,
    );
    w.env.send_paid_by(&[ix], &launcher, &[&mint]).ok();
}

/// M1: the dev bag is held to max wallet, never released before the early-buyer unlock, and
/// released no faster than max wallet lets the dev hold; dev buys only right after the launch.
#[test]
fn the_dev_bag_keeps_max_wallet_and_the_early_lock() {
    let mut w = World::new();
    // Holder rewards 1%, max wallet 1%, early window 300 s locked 7 days, no creator lock.
    let r = rules(100, 100, 0, 0, 100, 0, 300, 7 * 86_400);
    let launcher = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = launch_through(&mut w, &launcher, cargs(HOLDERS, 0, SOL), r, 50, None);
    tx.ok();
    let k = LaunchKeys::of(&w.launch(&mint));
    let kit: KitConfig = w.env.read(&launch::kit_config_address(&mint));
    // 5 SOL would buy far above the 1% cap: refused.
    w.env
        .send_paid_by(
            &[companion::dev_buy(launcher.pubkey(), &k, 5 * SOL, 1)],
            &launcher,
            &[],
        )
        .expect_code(code(CompanionError::DevBagOverMaxWallet));
    // Under the cap it lands; with no vesting it is released only after the early-buyer unlock.
    w.env
        .send_paid_by(
            &[companion::dev_buy(launcher.pubkey(), &k, SOL / 10, 1)],
            &launcher,
            &[],
        )
        .ok();
    let bag = companion_of(&w, &mint).dev_tokens;
    assert!(bag > 0 && bag <= kit.max_wallet_amount);
    w.env
        .send_paid_by(
            &[companion::release(
                launcher.pubkey(),
                &k,
                true,
                launcher.pubkey(),
            )],
            &launcher,
            &[],
        )
        .expect_code(code(CompanionError::EarlyLocked));
    w.env.warp(7 * 86_400 + 1);
    w.env
        .send_paid_by(
            &[companion::release(
                launcher.pubkey(),
                &k,
                true,
                launcher.pubkey(),
            )],
            &launcher,
            &[],
        )
        .ok();
    assert_eq!(w.env.holding(&mint, &launcher.pubkey()), bag);
    // A dev buy long after the launch is refused.
    w.env
        .send_paid_by(
            &[companion::dev_buy(launcher.pubkey(), &k, SOL / 100, 1)],
            &launcher,
            &[],
        )
        .expect_code(code(CompanionError::DevBuyWindowClosed));
}

/// H2: a cranker who pumps the price before a buyback only makes it wait; one who pumps less than
/// the premium gains less than the round trip costs. Either way the sandwich loses money.
#[test]
fn a_buyback_sandwich_does_not_pay() {
    for front in [SOL / 5, 5 * SOL, 30 * SOL] {
        let (spent, profit) = sandwich(front);
        println!("sandwich with a front-run of {front}: buyback spent {spent}, cranker profit {profit} lamports");
        assert!(profit < 0, "front-run {front}: the sandwich paid {profit}");
    }
}

fn sandwich(front: u64) -> (u64, i128) {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(5 * SOL);
    let (mint, tx) = launch_through(
        &mut w,
        &launcher,
        cargs(BUYBACK, 0, 8 * SOL),
        LaunchRules::NONE,
        100,
        None,
    );
    tx.ok();
    w.env.warp(31);
    for _ in 0..8 {
        let t = w.wallet_with_sol(60 * SOL);
        w.buy(&t, &mint, 50 * SOL).ok();
        let h = w.env.holding(&mint, &t.pubkey());
        w.sell(&t, &mint, h).ok();
    }
    let cranker = w.wallet_with_sol(200 * SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees(cranker.pubkey(), mint, None)],
            &cranker,
            &[],
        )
        .ok();
    w.env.warp(60);
    let sol_before = w.env.holding(&w.sol, &cranker.pubkey());
    let lam_before = w.env.lamports(&cranker.pubkey());
    let k = LaunchKeys::of(&w.launch(&mint));
    let ixs = [
        token::create_holding(cranker.pubkey(), mint, cranker.pubkey()),
        w.launch_swap_ix(&cranker.pubkey(), &mint, 1, front, 0),
    ];
    w.env.send_paid_by(&ixs, &cranker, &[]).ok();
    let got = w.env.holding(&mint, &cranker.pubkey());
    let tx = w.env.send_paid_by(
        &[companion::buyback(cranker.pubkey(), &k, false)],
        &cranker,
        &[],
    );
    tx.ok();
    let waited = !tx.events::<BuybackWaited>().is_empty();
    let spent = tx.events::<BoughtBack>().first().map_or(0, |e| e.spent);
    assert!(waited || spent > 0);
    w.sell(&cranker, &mint, got).ok();
    let sol_after = w.env.holding(&w.sol, &cranker.pubkey());
    let lam_after = w.env.lamports(&cranker.pubkey());
    let profit =
        (sol_after as i128 - sol_before as i128) + (lam_after as i128 - lam_before as i128);
    (spent, profit)
}
