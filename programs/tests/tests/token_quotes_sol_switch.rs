//! The SOL switch (`docs/token-quotes.md` §7.5.2) against the venues' mainnet programs and live pools
//! (`bordrless_program_tests::venues`): Raydium AMM v4, CPMM and CLMM and Pump AMM. A coin quoted in
//! a pool's token, launched with the switch on, has its tokens sold to SOL on the token's reference
//! pool by the launchpad's `swap_to_sol`: its creator fees (`swap_creator_fees`, onto the launch,
//! claimed with `claim_creator_fees_sol`), its kit's holder fees (`swap_rewards_to_sol`, shared over
//! the hour), a companion's launcher's and holders' parts (`swap_bucket_to_sol`, then `withdraw` and
//! `share`). The reference: registered by anyone on each venue, against wrapped SOL or USDC, its
//! checks, its replacement; the minimum out from the pool's price (and its time-weighted price on
//! CPMM and CLMM); the cap per 8 slots against one seller, many coins on one token and a push.
//!
//! Ignored unless the venue programs' dumps are in `fixtures/venues` (`scripts/solana/
//! venue-fixtures.sh`): run with `--ignored`.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use bordrless_companion::client as companion;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::LegSwappedToSol;
use bordrless_companion::instructions::CreateArgs;
use bordrless_companion::state::{Companion, CompanionLegs, Split};
use bordrless_kit::client as kit;
use bordrless_kit::constants::REWARD_KIND_SOL;
use bordrless_kit::error::KitError;
use bordrless_kit::events::RewardsSwappedToSol;
use bordrless_kit::state::KitConfig;
use bordrless_launch::client as launch;
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::{QuotePoolRegistered, SwappedToSol};
use bordrless_launch::instructions::{CreateTokenLaunchArgs, TokenPayouts};
use bordrless_launch::state::{Launch, LaunchRules, QuoteReference};
use bordrless_launch::venues::{self, math, USDC_MINT};
use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::protocol_lookup_table;
use bordrless_program_tests::venues::{load_programs, Fixture, Venue, VENUE_NOW};
use bordrless_swap::spl;
use bordrless_token::client as token;
use solana_keypair::Keypair;
use solana_signer::Signer;

const FEE: u16 = 200;
const SOL: u64 = 1_000_000_000;
const TSLAX: Pubkey = Pubkey::from_str_const("XsDoVfqeBukxuZHWhdvWHBhgEHjGNst4MLodqsJHzoB");
const SPYX: Pubkey = Pubkey::from_str_const("XsoCS1TfEyfFhfvj8EtZ528L3CaKBDBRqRapnBbDF2W");
const ZAI: Pubkey = Pubkey::from_str_const("8vwqxHGz1H4XxyKajr99Yxm65HjYNVtqtCNoM2yWb13e");
const USELESS: Pubkey = Pubkey::from_str_const("Dz9mQ9NzkBcCsuGPFJ3r1bS4wgqKMHBPiVuniW8Mbonk");
const NEAR: Pubkey = Pubkey::from_str_const("3ZLekZYq2qkZiSpnSvabjit34tUkjSwD1JFuW9as9wBG");
const RAY: Pubkey = Pubkey::from_str_const("4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R");

fn lcode(e: LaunchError) -> u32 {
    u32::from(e)
}

/// A world with the venues' programs and `fixtures`' accounts, its clock after every fixture.
fn world(fixtures: &[&str]) -> World {
    let mut w = World::new();
    load_programs(&mut w.env);
    for f in fixtures {
        Fixture::load(f).put(&mut w.env);
    }
    w.env.now = VENUE_NOW;
    w.env.slot += 1_000;
    w.env.sync_clock();
    w
}

/// Moves the clock to the next 8-slot window (and a few seconds on).
fn next_window(w: &mut World) {
    w.env.slot += 8;
    w.env.now += 4;
    w.env.sync_clock();
}

fn register_with(w: &mut World, v: &Venue, usdc: Option<&Venue>, old: Option<&Venue>) -> Tx {
    let payer = w.env.funded(10 * SOL);
    let ix = launch::register_quote_pool(
        payer.pubkey(),
        v.token,
        v.accounts.clone(),
        usdc.map(|u| u.accounts.clone()),
        old.map(|o| o.accounts.clone()),
    );
    w.env.send_paid_by(&[ix], &payer, &[])
}

fn register(w: &mut World, v: &Venue, usdc: Option<&Venue>) -> Tx {
    register_with(w, v, usdc, None)
}

fn args(token: &Pubkey, vq: u64, rules: LaunchRules, switch: bool) -> CreateTokenLaunchArgs {
    CreateTokenLaunchArgs {
        payouts: TokenPayouts {
            flags: u8::from(switch),
            holder_round_secs: 0,
        },
        ..World::token_launch_args_with("SW", FEE, &[(*token, vq)], rules)
    }
}

/// A wallet holding plenty of `token`.
fn holder_of(w: &mut World, token: &Pubkey) -> Keypair {
    let program = w.env.account(token).expect("the token's mint").owner;
    let kp = w.env.funded(100 * SOL);
    w.env
        .token_account(&kp.pubkey(), token, &program, u64::MAX / 8);
    kp
}

/// A coin quoted in `token` (its virtual quote `vq` raw units), the switch `switch`, `rules`, its
/// leg open: the coin and its creator.
fn coin_with(
    w: &mut World,
    token: &Pubkey,
    vq: u64,
    rules: LaunchRules,
    switch: bool,
) -> (Pubkey, Keypair) {
    let creator = holder_of(w, token);
    let mint = Keypair::new();
    let ix = launch::create_token_launch(
        creator.pubkey(),
        mint.pubkey(),
        w.env.treasury.pubkey(),
        args(token, vq, rules, switch),
        &[*token],
    );
    w.env.send_paid_by(&[ix], &creator, &[&mint]).ok();
    w.open_all_legs(&mint.pubkey());
    (mint.pubkey(), creator)
}

fn coin(w: &mut World, token: &Pubkey, vq: u64) -> (Pubkey, Keypair) {
    coin_with(w, token, vq, LaunchRules::NONE, true)
}

/// The leg's fee account, set to `amount` (as trades would have filled it).
fn fees(w: &mut World, mint: &Pubkey, amount: u64) {
    let keys = w.leg_keys(mint, 0);
    w.env.set_token_amount(&keys.fee_account(), amount);
}

fn sell_fees_with(
    w: &mut World,
    crank: &Keypair,
    mint: &Pubkey,
    venue: Vec<AccountMeta>,
    amount: u64,
) -> Tx {
    let keys = w.leg_keys(mint, 0);
    let ix = launch::swap_creator_fees(crank.pubkey(), &keys, 0, amount, venue);
    w.env.send_paid_by(&[ix], crank, &[])
}

fn sell_fees(w: &mut World, mint: &Pubkey, v: &Venue, amount: u64) -> Tx {
    let crank = w.env.funded(SOL);
    sell_fees_with(w, &crank, mint, v.accounts.clone(), amount)
}

fn reference(w: &World, token: &Pubkey) -> QuoteReference {
    w.env.read(&launch::quote_reference_address(token))
}

#[track_caller]
fn ok(tx: &Tx, what: &str) {
    if tx.result.is_err() {
        panic!(
            "{what}: {:?}\n{}",
            tx.result.as_ref().err().map(|e| e.err.clone()),
            tx.logs().join("\n")
        );
    }
}

/// The pool's vault of `token` among the venue's accounts, and its balance.
fn token_vault(w: &World, v: &Venue) -> u64 {
    v.accounts
        .iter()
        .map(|a| a.pubkey)
        .find_map(|k| {
            let a = w.env.account(&k)?;
            (a.data.len() >= 72
                && Pubkey::new_from_array(a.data[0..32].try_into().unwrap()) == v.token
                && (a.owner == spl::TOKEN_PROGRAM_ID || a.owner == spl::TOKEN_2022_PROGRAM_ID))
                .then(|| u64::from_le_bytes(a.data[64..72].try_into().unwrap()))
        })
        .expect("the pool's token vault")
}

// ---- Each venue --------------------------------------------------------------------------------------

/// A venue case: its fixture, its token (none for Pump AMM, which reads its base), its reader.
type Case = (&'static str, Pubkey, fn(&Fixture, Pubkey) -> Venue);

#[test]
#[ignore = "needs the venue programs: scripts/solana/venue-fixtures.sh"]
fn each_venue_sells_a_creator_fee_to_sol() {
    // (fixture, token, the venue's reader) on every venue, SPL Token and Token-2022 tokens.
    let cases: [Case; 8] = [
        ("amm_v4_sol_usdc", USDC_MINT, Venue::amm_v4),
        ("amm_v4_ray_wsol", RAY, Venue::amm_v4),
        ("cpmm_zai_wsol", ZAI, Venue::cpmm),
        ("cpmm_useless_wsol", USELESS, Venue::cpmm),
        ("clmm_tslax_wsol", TSLAX, Venue::clmm),
        ("clmm_spyx_wsol", SPYX, Venue::clmm),
        ("pump_ansem_wsol", Pubkey::default(), |f, _| Venue::pump(f)),
        ("pump_kimchi_wsol", Pubkey::default(), |f, _| Venue::pump(f)),
    ];
    for (fixture, token, venue) in cases {
        let mut w = world(&[fixture]);
        let f = Fixture::load(fixture);
        let v = venue(&f, token);
        let tx = register(&mut w, &v, None);
        ok(&tx, fixture);
        let e = tx.event::<QuotePoolRegistered>();
        let r = reference(&w, &v.token);
        assert_eq!((r.pool, r.via, r.venue), (v.pool, 0, e.venue), "{fixture}");
        let (mint, creator) = coin(&mut w, &v.token, 1_000_000_000);
        let keys = w.leg_keys(&mint, 0);
        // A five-thousandth of the pool's token side: well within the window.
        let amount = token_vault(&w, &v) / 5_000;
        fees(&mut w, &mint, amount);
        let launch_before = w.env.lamports(&launch::launch_address(&mint));
        let crank = w.env.funded(SOL);
        let crank_before = w.env.lamports(&crank.pubkey());
        let tx = sell_fees_with(&mut w, &crank, &mint, v.accounts.clone(), amount);
        ok(&tx, fixture);
        let s = tx.event::<SwappedToSol>();
        println!(
            "{fixture}: depth {} SOL fee {} ppm; sold {} for {} lamports (min {}), {} CU",
            e.depth_lamports / SOL,
            e.fee_ppm,
            s.amount_in,
            s.lamports_out,
            s.min_out,
            tx.cu()
        );
        assert!(s.lamports_out >= s.min_out && s.min_out > 0, "{fixture}");
        // The minimum is within 1.5% of what the reference's value less its fee gives.
        let value = math::value_of(s.amount_in, s.price);
        let after_fee = value * (1_000_000 - u128::from(e.fee_ppm)) / 1_000_000;
        assert!(
            u128::from(s.min_out) * 1_000 >= after_fee * 985,
            "{fixture}: the minimum's slack"
        );
        assert_eq!(
            w.env.lamports(&launch::launch_address(&mint)) - launch_before,
            s.lamports_out,
            "{fixture}: the SOL is on the launch"
        );
        assert_eq!(s.amount_in, amount, "{fixture}: the whole fee sold");
        assert_eq!(w.env.token_amount(&keys.fee_account()), 0);
        // The crank paid the wrapped-SOL account's rent and got it back: the fee only.
        assert_eq!(
            crank_before - w.env.lamports(&crank.pubkey()),
            5_000,
            "{fixture}: the crank's cost"
        );
        assert!(w
            .env
            .account(&spl::associated_token_address(
                &launch::launch_address(&mint),
                &spl::NATIVE_MINT,
                &spl::TOKEN_PROGRAM_ID
            ))
            .is_none_or(|a| a.lamports == 0));
        // The creator claims it in SOL.
        let before = w.env.lamports(&creator.pubkey());
        let ix = launch::claim_creator_fees_sol(creator.pubkey(), mint, None, None);
        ok(&w.env.send_paid_by(&[ix], &creator, &[]), fixture);
        assert!(
            w.env.lamports(&creator.pubkey()) + 5_000 >= before + s.lamports_out,
            "{fixture}: claimed"
        );
        let l: Launch = w.env.read(&launch::launch_address(&mint));
        assert_eq!(l.creator_fees_accrued, s.lamports_out);
    }
}

#[test]
#[ignore = "needs the venue programs: scripts/solana/venue-fixtures.sh"]
fn a_sale_through_usdc_chains_the_tokens_and_usdcs_references() {
    // USDC's own reference: Raydium AMM v4's SOL/USDC (a CLMM USDC/SOL pool also qualifies).
    for (fixture, token, venue) in [
        (
            "clmm_tslax_usdc",
            TSLAX,
            Venue::clmm as fn(&Fixture, Pubkey) -> Venue,
        ),
        ("cpmm_near_usdc", NEAR, Venue::cpmm),
        ("pump_cbucks_usdc", Pubkey::default(), |f: &Fixture, _| {
            Venue::pump(f)
        }),
    ] {
        let mut w = world(&["amm_v4_sol_usdc", fixture]);
        let usdc = Venue::amm_v4(&Fixture::load("amm_v4_sol_usdc"), USDC_MINT);
        let v = venue(&Fixture::load(fixture), token);
        // Without USDC's reference the token's USDC pool can't be valued.
        register(&mut w, &v, Some(&usdc)).expect_code(lcode(LaunchError::QuoteReferenceMissing));
        ok(&register(&mut w, &usdc, None), "USDC");
        let tx = register(&mut w, &v, Some(&usdc));
        ok(&tx, fixture);
        assert_eq!(reference(&w, &v.token).via, 1, "{fixture}: via USDC");
        let (mint, _creator) = coin(&mut w, &v.token, 1_000_000_000);
        let amount = token_vault(&w, &v) / 5_000;
        fees(&mut w, &mint, amount);
        let seller = launch::launch_address(&mint);
        let venue_accounts =
            launch::via_usdc_accounts(&seller, v.accounts.clone(), usdc.accounts.clone());
        let usdc_window = reference(&w, &USDC_MINT).window_value;
        let crank = w.env.funded(SOL);
        let tx = sell_fees_with(&mut w, &crank, &mint, venue_accounts, amount);
        ok(&tx, fixture);
        let s = tx.event::<SwappedToSol>();
        println!(
            "{fixture} via USDC: sold {} for {} lamports (min {}), {} CU",
            s.amount_in,
            s.lamports_out,
            s.min_out,
            tx.cu()
        );
        assert_eq!(s.via, 1);
        assert!(s.lamports_out >= s.min_out && s.lamports_out > 0);
        // Both windows count it; the seller's USDC account made and closed again.
        assert!(
            reference(&w, &USDC_MINT).window_value > usdc_window,
            "{fixture}: USDC's window"
        );
        assert!(reference(&w, &v.token).window_value > 0);
        let usdc_ata = spl::associated_token_address(&seller, &USDC_MINT, &spl::TOKEN_PROGRAM_ID);
        assert!(
            w.env.account(&usdc_ata).is_none_or(|a| a.lamports == 0),
            "{fixture}"
        );
    }
}

// ---- Registration -----------------------------------------------------------------------------------

#[test]
#[ignore = "needs the venue programs: scripts/solana/venue-fixtures.sh"]
fn registration_holds_a_pool_to_its_checks_and_replaces_it_only_with_a_deeper_one() {
    let mut w = world(&["amm_v4_sol_usdc", "clmm_usdc_wsol", "cpmm_zai_wsol"]);
    let amm = Venue::amm_v4(&Fixture::load("amm_v4_sol_usdc"), USDC_MINT);
    let clmm = Venue::clmm(&Fixture::load("clmm_usdc_wsol"), USDC_MINT);
    let zai = Venue::cpmm(&Fixture::load("cpmm_zai_wsol"), ZAI);
    // A pool of another token, a foreign vault, swaps off, a fee out of bounds, too shallow.
    let mut other = zai.accounts.clone();
    let tx = {
        let payer = w.env.funded(SOL);
        let ix = launch::register_quote_pool(payer.pubkey(), USDC_MINT, other.clone(), None, None);
        w.env.send_paid_by(&[ix], &payer, &[])
    };
    tx.expect_code(lcode(LaunchError::ReferencePoolInvalid));
    other[4].pubkey = amm.accounts[3].pubkey;
    let fake = Venue {
        accounts: other,
        ..Venue::cpmm(&Fixture::load("cpmm_zai_wsol"), ZAI)
    };
    register(&mut w, &fake, None).expect_code(lcode(LaunchError::WrongVenueAccounts));
    let pool = amm.pool;
    let original = w.env.account(&pool).unwrap();
    for (what, at, value, code) in [
        ("swaps off", 0usize, 4u64, LaunchError::ReferencePoolInvalid),
        ("a 5% fee", 176, 500, LaunchError::ReferencePoolInvalid),
        ("no fee", 176, 0, LaunchError::ReferencePoolInvalid),
    ] {
        let mut a = original.clone();
        a.data[at..at + 8].copy_from_slice(&value.to_le_bytes());
        w.env.put(pool, a);
        register(&mut w, &amm, None).expect_code(lcode(code));
        let _ = what;
    }
    w.env.put(pool, original.clone());
    let pc_vault = amm.accounts[4].pubkey;
    let pc = w.env.token_amount(&pc_vault);
    let coin_vault = amm.accounts[3].pubkey;
    let coin_amount = w.env.token_amount(&coin_vault);
    // 99 SOL on the SOL side (and its price kept): too shallow.
    w.env.set_token_amount(&coin_vault, 99 * SOL);
    w.env.set_token_amount(
        &pc_vault,
        (u128::from(pc) * u128::from(99 * SOL) / u128::from(coin_amount)) as u64,
    );
    register(&mut w, &amm, None).expect_code(lcode(LaunchError::ReferencePoolShallow));
    w.env.set_token_amount(&coin_vault, coin_amount);
    w.env.set_token_amount(&pc_vault, pc);
    // Registered: AMM v4's SOL/USDC.
    ok(&register(&mut w, &amm, None), "AMM v4");
    // The CLMM pool is shallower: within the hour, then not deeper; the registered pool's accounts
    // must be the registered pool's.
    register_with(&mut w, &clmm, None, Some(&amm))
        .expect_code(lcode(LaunchError::ReferenceReplacedRecently));
    register_with(&mut w, &clmm, None, Some(&zai))
        .expect_code(lcode(LaunchError::WrongVenueAccounts));
    // The same pool registered again by someone else refreshes its depth only: its registrant and
    // its time stay, so the hour runs from the first registration (nobody holds off a deeper pool
    // by registering the current one every hour).
    let first = reference(&w, &USDC_MINT);
    w.env.warp(3_000);
    ok(&register(&mut w, &amm, None), "again");
    let again = reference(&w, &USDC_MINT);
    assert_eq!(
        (again.registered_at, again.registrant),
        (first.registered_at, first.registrant)
    );
    w.env.warp(601);
    register_with(&mut w, &clmm, None, Some(&amm))
        .expect_code(lcode(LaunchError::ReferenceNotDeeper));
    // The registered pool fails a check now (swaps off): any qualifying pool replaces it at once.
    let mut a = original.clone();
    a.data[0..8].copy_from_slice(&4u64.to_le_bytes());
    w.env.put(pool, a);
    let tx = register_with(&mut w, &clmm, None, Some(&amm));
    ok(&tx, "replaced");
    let e = tx.event::<QuotePoolRegistered>();
    assert_eq!((e.pool, e.replaced), (clmm.pool, Some(amm.pool)));
    // Back on, AMM v4 is deeper than the CLMM pool by more than 25%, but not within the hour.
    w.env.put(pool, original);
    register_with(&mut w, &amm, None, Some(&clmm))
        .expect_code(lcode(LaunchError::ReferenceReplacedRecently));
    w.env.warp(3_601);
    ok(&register_with(&mut w, &amm, None, Some(&clmm)), "deeper");
    assert_eq!(reference(&w, &USDC_MINT).pool, amm.pool);
}

// ---- The window --------------------------------------------------------------------------------------

#[test]
#[ignore = "needs the venue programs: scripts/solana/venue-fixtures.sh"]
fn the_window_caps_every_seller_of_a_token_per_8_slots() {
    let mut w = world(&["clmm_tslax_wsol"]);
    let v = Venue::clmm(&Fixture::load("clmm_tslax_wsol"), TSLAX);
    ok(&register(&mut w, &v, None), "register");
    // Two coins quoted in TSLAx, each with far more fees than a window takes.
    let (one, _) = coin(&mut w, &TSLAX, 1_300_000_000);
    let (two, _) = coin(&mut w, &TSLAX, 1_300_000_000);
    let big = 100 * 100_000_000; // 100 TSLAx, about 358 SOL
    fees(&mut w, &one, big);
    fees(&mut w, &two, big);
    // The cap: half the fee times the depth (the SOL vault, under the virtual reserve here), as
    // the window opens.
    let depth = u128::from(w.env.token_amount(&v.accounts[3].pubkey));
    let cap = math::window_cap(2_500, depth);
    let tx = sell_fees(&mut w, &one, &v, big);
    ok(&tx, "first seller");
    let first = tx.event::<SwappedToSol>();
    let r = reference(&w, &TSLAX);
    assert!(
        u128::from(r.window_value) <= cap,
        "within the cap: {} of {cap}",
        r.window_value
    );
    assert!(
        u128::from(r.window_value) * 100 >= cap * 95,
        "the first seller took the window"
    );
    assert!(first.amount_in < big, "only the room was sold");
    // The second coin in the same window: nothing left.
    sell_fees(&mut w, &two, &v, big).expect_code(lcode(LaunchError::SwapWindowFull));
    // The next window: room again, for either coin.
    next_window(&mut w);
    let tx = sell_fees(&mut w, &two, &v, big);
    ok(&tx, "next window");
    assert!(tx.event::<SwappedToSol>().amount_in > 0);
    // Many coins in one transaction: still one window.
    next_window(&mut w);
    let crank = w.env.funded(SOL);
    let keys = [w.leg_keys(&one, 0), w.leg_keys(&two, 0)];
    let ixs: Vec<_> = keys
        .iter()
        .map(|k| launch::swap_creator_fees(crank.pubkey(), k, 0, big, v.accounts.clone()))
        .collect();
    w.env
        .send_paid_by(&ixs, &crank, &[])
        .expect_code(lcode(LaunchError::SwapWindowFull));
    let tx = w.env.send_paid_by(&ixs[..1], &crank, &[]);
    ok(&tx, "one of them");
    assert!(u128::from(reference(&w, &TSLAX).window_value) <= cap);
}

// ---- Pushes ----------------------------------------------------------------------------------------------

/// A wallet with `amount` of `v`'s token and an account of the other side.
fn pusher(w: &mut World, v: &Venue) -> (Keypair, Pubkey, Pubkey) {
    let kp = w.env.funded(100 * SOL);
    let source = w
        .env
        .token_account(&kp.pubkey(), &v.token, &v.token_program, u64::MAX / 8);
    let dest = w
        .env
        .token_account(&kp.pubkey(), &v.other, &spl::TOKEN_PROGRAM_ID, 0);
    (kp, source, dest)
}

#[test]
#[ignore = "needs the venue programs: scripts/solana/venue-fixtures.sh"]
fn a_push_of_the_spot_fails_the_minimum_where_the_venue_keeps_a_history() {
    for (fixture, token, venue) in [
        (
            "cpmm_zai_wsol",
            ZAI,
            Venue::cpmm as fn(&Fixture, Pubkey) -> Venue,
        ),
        ("clmm_tslax_wsol", TSLAX, Venue::clmm),
    ] {
        let mut w = world(&[fixture]);
        let v = venue(&Fixture::load(fixture), token);
        ok(&register(&mut w, &v, None), fixture);
        let (mint, _) = coin(&mut w, &v.token, 1_000_000_000);
        let amount = token_vault(&w, &v) / 5_000;
        fees(&mut w, &mint, amount);
        // In one transaction: the pusher sells a tenth of the pool's token side (the price falls
        // well over 3%), then the fees are sold: the reference holds 97% of the time-weighted
        // price, so the sale's minimum is above what the pushed pool pays.
        let (p, source, dest) = pusher(&mut w, &v);
        let push = v.sell_ix(p.pubkey(), source, dest, token_vault(&w, &v), 0);
        let keys = w.leg_keys(&mint, 0);
        let sale = launch::swap_creator_fees(p.pubkey(), &keys, 0, amount, v.accounts.clone());
        let tx = w.env.send_paid_by(&[push.clone(), sale.clone()], &p, &[]);
        tx.expect_fail();
        println!(
            "{fixture}: a push before the sale fails it: {:?}",
            tx.result.as_ref().err().map(|e| e.err.clone())
        );
        // Without the push it sells.
        ok(&w.env.send_paid_by(&[sale], &p, &[]), fixture);
    }
}

#[test]
#[ignore = "needs the venue programs: scripts/solana/venue-fixtures.sh"]
fn a_push_costs_more_than_the_sale_loses_where_the_venue_keeps_no_history() {
    // AMM v4 and Pump AMM price from the spot: a push in the same transaction lowers the sale's
    // price, but the window keeps the sale at most half the fee times the depth, so what the sale
    // loses against the fair price stays below the fee the push itself paid (and a push must be
    // bought back, paying the fee again).
    for (fixture, token, venue) in [
        (
            "amm_v4_ray_wsol",
            RAY,
            Venue::amm_v4 as fn(&Fixture, Pubkey) -> Venue,
        ),
        ("pump_ansem_wsol", Pubkey::default(), |f: &Fixture, _| {
            Venue::pump(f)
        }),
    ] {
        for push_share in [100u64, 20, 5] {
            let mut w = world(&[fixture]);
            let v = venue(&Fixture::load(fixture), token);
            ok(&register(&mut w, &v, None), fixture);
            let (mint, _) = coin(&mut w, &v.token, 1_000_000_000);
            // As much as the window takes.
            let big = token_vault(&w, &v) / 50;
            fees(&mut w, &mint, big);
            // The fair value of the sale: what the pool pays now, by a dry run on a copy (the sale
            // alone, no push).
            let keys = w.leg_keys(&mint, 0);
            let crank = w.env.funded(SOL);
            let mut fair = w.env.svm.clone();
            let sale = launch::swap_creator_fees(crank.pubkey(), &keys, 0, big, v.accounts.clone());
            let alone = {
                let launch_key = launch::launch_address(&mint);
                let before = fair.get_account(&launch_key).unwrap().lamports;
                let mut env2 = bordrless_program_tests::env::Env {
                    svm: std::mem::replace(&mut fair, litesvm::LiteSVM::new()),
                    payer: w.env.payer.insecure_clone(),
                    now: w.env.now,
                    slot: w.env.slot,
                    deployer: w.env.deployer.insecure_clone(),
                    treasury: w.env.treasury.insecure_clone(),
                };
                let tx = env2.send_paid_by(std::slice::from_ref(&sale), &crank, &[]);
                ok(&tx, "alone");
                let s = tx.event::<SwappedToSol>();
                (s.amount_in, env2.lamports(&launch_key) - before)
            };
            // The push and the sale.
            let (p, source, dest) = pusher(&mut w, &v);
            let pushed = token_vault(&w, &v) / push_share;
            let push = v.sell_ix(p.pubkey(), source, dest, pushed, 0);
            let launch_key = launch::launch_address(&mint);
            let before = w.env.lamports(&launch_key);
            let tx = w.env.send_paid_by(&[push, sale], &crank, &[&p]);
            ok(&tx, fixture);
            let s = tx.event::<SwappedToSol>();
            let got = w.env.lamports(&launch_key) - before;
            // What the same tokens fetch without the push (the window may let a pushed price sell
            // a few more tokens: compared at the unpushed sale's own rate).
            let fair = u128::from(alone.1) * u128::from(s.amount_in) / u128::from(alone.0);
            let loss = fair.saturating_sub(u128::from(got));
            // The push's fee: the pool's fee on what the push sold, at the price before it.
            let r = reference(&w, &v.token);
            let _ = r;
            let push_value = u128::from(pushed) * u128::from(alone.1) / u128::from(alone.0);
            let push_fee = push_value
                * u128::from(if fixture.starts_with("amm") {
                    2_500u32
                } else {
                    3_000
                })
                / 1_000_000;
            println!(
                "{fixture}: push of 1/{push_share} of the pool: the sale got {got} instead of {} (loss {loss}); the push's fee {push_fee}",
                alone.1
            );
            assert!(loss < push_fee, "{fixture}: a push of 1/{push_share} pays");
        }
    }
}

// ---- Refusals -------------------------------------------------------------------------------------------

#[test]
#[ignore = "needs the venue programs: scripts/solana/venue-fixtures.sh"]
fn only_a_custodian_of_a_switched_coin_sells_and_only_its_creator_takes_tokens() {
    let mut w = world(&["clmm_tslax_wsol"]);
    let v = Venue::clmm(&Fixture::load("clmm_tslax_wsol"), TSLAX);
    let (mint, creator) = coin(&mut w, &TSLAX, 1_300_000_000);
    fees(&mut w, &mint, 10_000_000);
    // No reference yet.
    sell_fees(&mut w, &mint, &v, 10_000_000).expect_fail();
    ok(&register(&mut w, &v, None), "register");
    // A wallet that is no custodian, selling its own tokens.
    let stranger = holder_of(&mut w, &TSLAX);
    let keys = w.leg_keys(&mint, 0);
    let source = keys.quote_account(&stranger.pubkey());
    let ix = launch::swap_to_sol(
        stranger.pubkey(),
        stranger.pubkey(),
        &keys,
        source,
        10_000_000,
        v.accounts.clone(),
    );
    w.env
        .send_paid_by(&[ix], &stranger, &[])
        .expect_code(lcode(LaunchError::NotACustodian));
    // Too little: under 0.01 SOL.
    sell_fees(&mut w, &mint, &v, 1_000).expect_code(lcode(LaunchError::SwapTooSmall));
    // Paused by the admin (only the admin), then resumed.
    let admin = w.env.deployer.insecure_clone();
    let ix = launch::pause_quote_reference(stranger.pubkey(), TSLAX, true);
    w.env.send_paid_by(&[ix], &stranger, &[]).expect_fail();
    let ix = launch::pause_quote_reference(admin.pubkey(), TSLAX, true);
    ok(&w.env.send_paid_by(&[ix], &admin, &[]), "pause");
    sell_fees(&mut w, &mint, &v, 10_000_000).expect_code(lcode(LaunchError::ReferencePaused));
    let ix = launch::pause_quote_reference(admin.pubkey(), TSLAX, false);
    ok(&w.env.send_paid_by(&[ix], &admin, &[]), "resume");
    // With the switch on, the tokens go to the creator only when it signs.
    let payer = w.env.funded(SOL);
    let ix = launch::claim_creator_fees_token(payer.pubkey(), creator.pubkey(), &keys, 0);
    w.env
        .send_paid_by(&[ix], &payer, &[])
        .expect_code(lcode(LaunchError::CreatorSignsTokenClaim));
    let mut ix = launch::claim_creator_fees_token(creator.pubkey(), creator.pubkey(), &keys, 0);
    for a in ix.accounts.iter_mut() {
        if a.pubkey == creator.pubkey() {
            a.is_signer = true;
        }
    }
    ok(
        &w.env.send_paid_by(&[ix], &creator, &[]),
        "the creator takes tokens",
    );
    // A coin without the switch: no sale, the SOL claim refused.
    let (off, off_creator) = coin_with(&mut w, &TSLAX, 1_300_000_000, LaunchRules::NONE, false);
    fees(&mut w, &off, 10_000_000);
    sell_fees(&mut w, &off, &v, 10_000_000).expect_code(lcode(LaunchError::SolSwitchOff));
    let ix = launch::claim_creator_fees_sol(off_creator.pubkey(), off, None, None);
    w.env
        .send_paid_by(&[ix], &off_creator, &[])
        .expect_code(lcode(LaunchError::NotNativeLaunch));
}

// ---- The kit's holder rewards in SOL ----------------------------------------------------------------

#[test]
#[ignore = "needs the venue programs: scripts/solana/venue-fixtures.sh"]
fn the_kit_sells_its_holder_fees_to_sol_and_streams_them() {
    let mut w = world(&["clmm_tslax_wsol"]);
    let v = Venue::clmm(&Fixture::load("clmm_tslax_wsol"), TSLAX);
    ok(&register(&mut w, &v, None), "register");
    let rules = LaunchRules {
        holder_fee_buy_bps: 100,
        holder_fee_sell_bps: 100,
        ..LaunchRules::NONE
    };
    let (mint, _) = coin_with(&mut w, &TSLAX, 1_300_000_000, rules, true);
    let k: KitConfig = w.env.read(&kit::kit_config_address(&mint));
    assert_eq!(k.reward_kind(), REWARD_KIND_SOL);
    assert!(k.is_native());
    let (a, b) = (holder_of(&mut w, &TSLAX), holder_of(&mut w, &TSLAX));
    w.leg_buy(&a, &mint, 0, 200_000_000).ok();
    w.leg_buy(&b, &mint, 0, 200_000_000).ok();
    w.leg_buy(&a, &mint, 0, 100_000_000).ok();
    let keys = w.leg_keys(&mint, 0);
    let vault = launch::kit_vault_address(&keys);
    let held = w.env.token_amount(&vault);
    assert!(held > 0, "the holder fees, in TSLAx");
    let config = kit::kit_config_address(&mint);
    let before = w.env.lamports(&config);
    let crank = w.env.funded(SOL);
    let rest = launch::swap_to_sol_rest(
        &config,
        &mint,
        &TSLAX,
        &keys.quote_program,
        v.accounts.clone(),
    );
    let ix = kit::swap_rewards_to_sol(crank.pubkey(), mint, 0, TSLAX, keys.quote_program, 0, rest);
    let tx = w.env.send_paid_by(&[ix], &crank, &[]);
    ok(&tx, "swap_rewards_to_sol");
    let e = tx.event::<RewardsSwappedToSol>();
    println!(
        "swap_rewards_to_sol: {} TSLAx raw for {} lamports, {} CU",
        e.amount_in,
        e.lamports,
        tx.cu()
    );
    assert!(e.shared);
    assert_eq!(w.env.lamports(&config) - before, e.lamports);
    assert_eq!(w.env.token_amount(&vault), held - e.amount_in);
    let k: KitConfig = w.env.read(&config);
    assert_eq!(
        k.stream_remaining + k.stream_next,
        e.lamports,
        "streamed over the hour"
    );
    // An hour on, holders claim SOL; a pushed claim reaches only the holder; a leg's pool and a
    // claim without the `Legs` are refused.
    w.env.warp(3_700);
    let tx = w
        .env
        .send_paid_by(&[kit::claim_sol_legs(a.pubkey(), mint)], &a, &[]);
    ok(&tx, "claim_sol");
    let payer = w.env.funded(SOL);
    ok(
        &w.env.send_paid_by(
            &[kit::claim_sol_for_legs(payer.pubkey(), b.pubkey(), mint)],
            &payer,
            &[],
        ),
        "claim_sol_for",
    );
    w.env
        .send_paid_by(&[kit::claim_sol(a.pubkey(), mint)], &a, &[])
        .expect_code(u32::from(KitError::LegsMissing));
    let pool = keys.pool();
    w.env
        .send_paid_by(
            &[kit::claim_sol_for_legs(payer.pubkey(), pool, mint)],
            &payer,
            &[],
        )
        .expect_fail();
    // Its token claims are this kit's no more.
    let ix = kit::claim_token(a.pubkey(), mint, TSLAX, keys.quote_program);
    w.env.send_paid_by(&[ix], &a, &[]).expect_fail();
}

// ---- A companion's parts in SOL -----------------------------------------------------------------------

fn companion_coin(
    w: &mut World,
    token: &Pubkey,
    split: Split,
    rules: LaunchRules,
) -> (Pubkey, Keypair) {
    let launcher = w.env.funded(100 * SOL);
    let mint = Keypair::new();
    let ix = companion::create(
        launcher.pubkey(),
        launcher.pubkey(),
        mint.pubkey(),
        CreateArgs {
            split,
            bounty_bps: 0,
            max_buyback: SOL,
            buyback_interval: 60,
            vest_secs: 0,
            fund: 2 * SOL,
        },
    );
    ok(&w.env.send_paid_by(&[ix], &launcher, &[&mint]), "create");
    let creator = companion::creator_address(&mint.pubkey());
    let a = args(token, 1_300_000_000, rules, true);
    let inner = launch::create_token_launch(
        creator,
        mint.pubkey(),
        w.env.treasury.pubkey(),
        a.clone(),
        &[*token],
    );
    let ix = companion::launch_token(launcher.pubkey(), mint.pubkey(), &inner, a);
    ok(
        &w.env.send_paid_by(&[ix], &launcher, &[&mint]),
        "launch_token",
    );
    w.open_all_legs(&mint.pubkey());
    (mint.pubkey(), launcher)
}

#[test]
#[ignore = "needs the venue programs: scripts/solana/venue-fixtures.sh"]
fn a_companions_launcher_and_holders_parts_are_sold_to_sol_then_paid() {
    let mut w = world(&["clmm_tslax_wsol"]);
    let v = Venue::clmm(&Fixture::load("clmm_tslax_wsol"), TSLAX);
    ok(&register(&mut w, &v, None), "register");
    let rules = LaunchRules {
        holder_fee_buy_bps: 100,
        holder_fee_sell_bps: 100,
        ..LaunchRules::NONE
    };
    let split = Split {
        buyback_bps: 2_000,
        holders_bps: 4_000,
        beneficiary_bps: 4_000,
    };
    let (mint, launcher) = companion_coin(&mut w, &TSLAX, split, rules);
    let legs: CompanionLegs = w.env.read(&companion::companion_legs_address(&mint));
    assert!(!legs.pays_in_token(0), "the switch: the paid parts in SOL");
    // Trades fill the leg's fee account; the claim splits it into the buckets (in TSLAx).
    let (a, b) = (holder_of(&mut w, &TSLAX), holder_of(&mut w, &TSLAX));
    w.leg_buy(&a, &mint, 0, 300_000_000).ok();
    w.leg_buy(&b, &mint, 0, 300_000_000).ok();
    let keys = w.leg_keys(&mint, 0);
    let crank = w.env.funded(10 * SOL);
    ok(
        &w.env.send_paid_by(
            &[companion::claim_fees_leg(crank.pubkey(), &keys, 0, false)],
            &crank,
            &[],
        ),
        "claim_fees_leg",
    );
    let pot = w
        .env
        .read::<CompanionLegs>(&companion::companion_legs_address(&mint))
        .legs[0];
    assert!(pot.pending_beneficiary > 0 && pot.pending_holders > 0 && pot.pending_buyback > 0);
    // The token steps refuse the switched parts.
    let ix = companion::withdraw_leg(crank.pubkey(), launcher.pubkey(), &keys, 0);
    w.env
        .send_paid_by(&[ix], &crank, &[])
        .expect_code(u32::from(CompanionError::SolSwitchOn));
    let ix = companion::share_leg(crank.pubkey(), &keys, 0, false);
    w.env
        .send_paid_by(&[ix], &crank, &[])
        .expect_code(u32::from(CompanionError::SolSwitchOn));
    // The launcher's part to SOL, then withdrawn.
    for part in [0u8, 1] {
        next_window(&mut w);
        let c_before: Companion = w.env.read(&companion::companion_address(&mint));
        let ix = companion::swap_bucket_to_sol(crank.pubkey(), &keys, 0, part, v.accounts.clone());
        let tx = w.env.send_paid_by(&[ix], &crank, &[]);
        ok(&tx, "swap_bucket_to_sol");
        let e = tx.event::<LegSwappedToSol>();
        println!(
            "swap_bucket_to_sol part {part}: {} TSLAx raw for {} lamports, {} CU",
            e.amount_in,
            e.lamports,
            tx.cu()
        );
        let c: Companion = w.env.read(&companion::companion_address(&mint));
        if part == 0 {
            assert_eq!(
                c.pending_beneficiary,
                c_before.pending_beneficiary + e.lamports
            );
        } else {
            assert_eq!(c.pending_holders, c_before.pending_holders + e.lamports);
        }
    }
    let pot2 = w
        .env
        .read::<CompanionLegs>(&companion::companion_legs_address(&mint))
        .legs[0];
    assert_eq!((pot2.pending_beneficiary, pot2.pending_holders), (0, 0));
    assert_eq!(
        pot2.pending_buyback, pot.pending_buyback,
        "the buyback keeps its TSLAx"
    );
    let before = w.env.lamports(&launcher.pubkey());
    ok(
        &w.env.send_paid_by(
            &[companion::withdraw(crank.pubkey(), mint, launcher.pubkey())],
            &crank,
            &[],
        ),
        "withdraw",
    );
    assert!(
        w.env.lamports(&launcher.pubkey()) > before,
        "the launcher paid in SOL"
    );
    // The holders' part shared with the kit in SOL.
    let config = kit::kit_config_address(&mint);
    let before = w.env.lamports(&config);
    ok(
        &w.env
            .send_paid_by(&[companion::share(crank.pubkey(), mint)], &crank, &[]),
        "share",
    );
    assert!(
        w.env.lamports(&config) > before,
        "the kit got the holders' SOL"
    );
    // The swap of a part that is not one: refused.
    let ix = companion::swap_bucket_to_sol(crank.pubkey(), &keys, 0, 2, v.accounts.clone());
    w.env
        .send_paid_by(&[ix], &crank, &[])
        .expect_code(u32::from(CompanionError::NotSwitchedPart));
}

// ---- Figures ------------------------------------------------------------------------------------------

fn switch_table(w: &World, extra: &[Pubkey]) -> Vec<Pubkey> {
    let mut t = protocol_lookup_table(w);
    t.extend([
        spl::TOKEN_PROGRAM_ID,
        spl::TOKEN_2022_PROGRAM_ID,
        spl::ATA_PROGRAM_ID,
        spl::NATIVE_MINT,
        launch::event_authority(),
        bordrless_launch::ID,
        kit::event_authority(),
        bordrless_kit::ID,
        venues::MEMO_PROGRAM,
    ]);
    t.extend_from_slice(extra);
    t.sort();
    t.dedup();
    t
}

#[test]
#[ignore = "needs the venue programs: scripts/solana/venue-fixtures.sh"]
fn measures_switch_compute_and_sizes() {
    for (fixture, token, venue) in [
        (
            "amm_v4_sol_usdc",
            USDC_MINT,
            Venue::amm_v4 as fn(&Fixture, Pubkey) -> Venue,
        ),
        ("cpmm_zai_wsol", ZAI, Venue::cpmm),
        ("clmm_tslax_wsol", TSLAX, Venue::clmm),
        ("pump_ansem_wsol", Pubkey::default(), |f: &Fixture, _| {
            Venue::pump(f)
        }),
    ] {
        let mut w = world(&[fixture]);
        let v = venue(&Fixture::load(fixture), token);
        // The venue's fixed accounts in the table, as the keeper's would hold them.
        let fixed: Vec<Pubkey> = v.accounts.iter().map(|a| a.pubkey).collect();
        let table = w
            .env
            .put_lookup_table(Pubkey::new_unique(), &switch_table(&w, &fixed));
        let payer = w.env.funded(10 * SOL);
        let ix =
            launch::register_quote_pool(payer.pubkey(), v.token, v.accounts.clone(), None, None);
        let ixs = vec![compute_unit_limit(400_000), compute_unit_price(10_000), ix];
        let without = w.env.v0_size(&ixs, &payer, &[], &[]);
        let tx = w.env.send_v0(&ixs, &payer, &[], std::slice::from_ref(&table));
        ok(&tx, fixture);
        println!(
            "register_quote_pool, {fixture}: {} CU, {} B with the table / {without} B without",
            tx.cu(),
            tx.size
        );
        let (mint, _) = coin(&mut w, &v.token, 1_000_000_000);
        let amount = token_vault(&w, &v) / 5_000;
        fees(&mut w, &mint, amount);
        let keys = w.leg_keys(&mint, 0);
        let crank = w.env.funded(SOL);
        let ix = launch::swap_creator_fees(crank.pubkey(), &keys, 0, amount, v.accounts.clone());
        let n = ix.accounts.len();
        let ixs = vec![compute_unit_limit(400_000), compute_unit_price(10_000), ix];
        let without = w.env.v0_size(&ixs, &crank, &[], &[]);
        let tx = w.env.send_v0(&ixs, &crank, &[], &[table]);
        ok(&tx, fixture);
        println!(
            "swap_creator_fees, {fixture}: {} CU, {n} accounts, {} B with the table / {without} B without",
            tx.cu(),
            tx.size
        );
        assert!(tx.size <= 1_232);
    }
    let _ = token::event_authority();
}

/// The kit's sale runs one call deeper than the launchpad's (the kit, the launchpad, the venue, then
/// its fee program or the token program): through Pump AMM (which calls its fee program) and through
/// USDC (two venues in one sale), within the runtime's call depth.
#[test]
#[ignore = "needs the venue programs: scripts/solana/venue-fixtures.sh"]
fn the_kits_sale_reaches_pump_and_usdc_within_the_call_depth() {
    let rules = LaunchRules {
        holder_fee_buy_bps: 100,
        holder_fee_sell_bps: 100,
        ..LaunchRules::NONE
    };
    for via_usdc in [false, true] {
        let (mut w, v, usdc) = if via_usdc {
            let mut w = world(&["amm_v4_sol_usdc", "clmm_tslax_usdc"]);
            let usdc = Venue::amm_v4(&Fixture::load("amm_v4_sol_usdc"), USDC_MINT);
            ok(&register(&mut w, &usdc, None), "USDC");
            let v = Venue::clmm(&Fixture::load("clmm_tslax_usdc"), TSLAX);
            (w, v, Some(usdc))
        } else {
            let w = world(&["pump_ansem_wsol"]);
            (w, Venue::pump(&Fixture::load("pump_ansem_wsol")), None)
        };
        ok(&register(&mut w, &v, usdc.as_ref()), "register");
        let token = v.token;
        let (mint, _) = coin_with(&mut w, &token, 1_000_000_000, rules, true);
        let keys = w.leg_keys(&mint, 0);
        // The kit's vault of the leg, as holder fees would have filled it: a five-thousandth of the
        // pool's token side.
        let vault = launch::kit_vault_address(&keys);
        let amount = token_vault(&w, &v) / 5_000;
        w.env.set_token_amount(&vault, amount);
        let config = kit::kit_config_address(&mint);
        let before = w.env.lamports(&config);
        let venue = match &usdc {
            Some(u) => launch::via_usdc_accounts(&config, v.accounts.clone(), u.accounts.clone()),
            None => v.accounts.clone(),
        };
        let rest = launch::swap_to_sol_rest(&config, &mint, &token, &keys.quote_program, venue);
        let crank = w.env.funded(SOL);
        let ix =
            kit::swap_rewards_to_sol(crank.pubkey(), mint, 0, token, keys.quote_program, 0, rest);
        let tx = w.env.send_paid_by(&[ix], &crank, &[]);
        ok(
            &tx,
            if via_usdc {
                "the kit via USDC"
            } else {
                "the kit on Pump AMM"
            },
        );
        let e = tx.event::<RewardsSwappedToSol>();
        println!(
            "swap_rewards_to_sol {}: {} raw for {} lamports, {} CU",
            if via_usdc {
                "via USDC (CLMM, AMM v4)"
            } else {
                "on Pump AMM"
            },
            e.amount_in,
            e.lamports,
            tx.cu()
        );
        assert_eq!(e.amount_in, amount);
        assert!(e.lamports > 0);
        assert_eq!(w.env.lamports(&config) - before, e.lamports);
        assert_eq!(w.env.token_amount(&vault), 0);
    }
}
