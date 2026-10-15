//! Companions (`docs/companions.md`): a launch whose creator is the companion's creator address,
//! so its creator fees are bought back and burned, streamed to holders or paid to its launcher by
//! code, every step permissionless.

use anchor_lang::prelude::Pubkey;
use bordrless_companion::client as companion;
use bordrless_companion::error::CompanionError;
use bordrless_companion::events::{BoughtBack, FeesClaimed};
use bordrless_companion::instructions::CreateArgs;
use bordrless_companion::state::{Companion, Split};
use bordrless_core::policy;
use bordrless_launch::client::{self as launch, LaunchKeys};
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_token::client as token;
use bordrless_token::state::Mint;
use solana_keypair::Keypair;
use solana_signer::Signer;

const CREATOR_FEE: u16 = 200;

fn args(split: Split, vest_secs: i64) -> CreateArgs {
    CreateArgs {
        split,
        bounty_bps: 50,
        max_buyback: SOL,
        buyback_interval: 60,
        vest_secs,
        fund: SOL / 2,
    }
}

fn code(e: CompanionError) -> u32 {
    u32::from(e)
}

/// A companion for a fresh mint, then the launch through it. Answers the mint.
fn launch_through(
    w: &mut World,
    launcher: &Keypair,
    split: Split,
    vest_secs: i64,
    rules: LaunchRules,
) -> (Pubkey, Tx) {
    let mint = Keypair::new();
    let ix = companion::create(
        launcher.pubkey(),
        launcher.pubkey(),
        mint.pubkey(),
        args(split, vest_secs),
    );
    w.env.send_paid_by(&[ix], launcher, &[&mint]).ok();
    let creator = companion::creator_address(&mint.pubkey());
    let launch_args = World::launch_args("CMP", CREATOR_FEE, VQ, rules);
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

/// Trades enough to accrue creator fees: buys and sells by a few wallets.
fn trade(w: &mut World, mint: &Pubkey, wallets: usize, sol: u64) {
    for _ in 0..wallets {
        let t = w.wallet_with_sol(sol + SOL);
        w.buy(&t, mint, sol).ok();
        let held = w.env.holding(mint, &t.pubkey());
        w.sell(&t, mint, held / 4).ok();
    }
}

/// Honest buybacks that wait while the price is above the reference, until it has caught up and
/// the next one would buy (the reference moves 5% an interval toward the price).
fn catch_up(w: &mut World, cranker: &Keypair, mint: &Pubkey) {
    for _ in 0..80 {
        let c = companion_of(w, mint);
        let p = w.launch_pool(mint);
        let spot = bordrless_companion::state::spot_price(
            p.quote_reserve,
            p.virtual_quote,
            p.base_reserve,
            p.virtual_base,
        )
        .unwrap();
        if spot <= c.reference_price * 10_300 / 10_000 {
            return;
        }
        w.env.warp(61);
        let tx = w.env.send_paid_by(
            &[companion::buyback(cranker.pubkey(), &keys(w, mint), false)],
            cranker,
            &[],
        );
        tx.ok();
        assert_eq!(
            tx.events::<bordrless_companion::events::BoughtBack>().len(),
            0
        );
    }
    panic!("the reference never caught up");
}

#[test]
fn the_token_that_buys_itself() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(5 * SOL);
    let split = Split {
        buyback_bps: 10_000,
        holders_bps: 0,
        beneficiary_bps: 0,
    };
    let (mint, tx) = launch_through(&mut w, &launcher, split, 0, LaunchRules::NONE);
    tx.ok();
    println!(
        "companion launch: CU {} size {} height {}",
        tx.cu(),
        tx.size,
        tx.max_height()
    );
    let l = w.launch(&mint);
    let creator = companion::creator_address(&mint);
    assert_eq!(l.creator, creator);
    let c = companion_of(&w, &mint);
    assert!(c.launched);

    w.env.warp(31);
    trade(&mut w, &mint, 4, 2 * SOL);
    let accrued = w.env.holding(&w.sol, &launch::launch_address(&mint));
    assert!(accrued > 0);

    // A stranger claims: paid its bounty in SOL; the rest waits for the buyback.
    let cranker = w.wallet_with_sol(SOL);
    let before = w.env.lamports(&cranker.pubkey());
    let tx = w.env.send_paid_by(
        &[companion::claim_fees(cranker.pubkey(), mint, None)],
        &cranker,
        &[],
    );
    tx.ok();
    let ev: FeesClaimed = tx.event();
    assert_eq!(ev.claimed, accrued);
    assert_eq!(ev.bounty, accrued * 50 / 10_000);
    assert_eq!(ev.to_buyback, accrued - ev.bounty);
    assert!(
        w.env.lamports(&cranker.pubkey()) > before - 10_000,
        "the bounty covers the fee"
    );
    assert_eq!(companion_of(&w, &mint).pending_buyback, accrued - ev.bounty);

    // Not before a minute after launch, and not while the price is more than 3% above the
    // reference (which the trades above moved it past): then bought and burned, the supply down by
    // what it bought.
    w.env.warp(60);
    catch_up(&mut w, &cranker, &mint);
    let pending = companion_of(&w, &mint).pending_buyback;
    let supply = w.env.read::<Mint>(&mint).supply;
    let tx = w.env.send_paid_by(
        &[companion::buyback(
            cranker.pubkey(),
            &keys(&w, &mint),
            false,
        )],
        &cranker,
        &[],
    );
    tx.ok();
    println!(
        "buyback: CU {} size {} height {}",
        tx.cu(),
        tx.size,
        tx.max_height()
    );
    let ev: BoughtBack = tx.event();
    assert!(ev.burned > 0);
    assert_eq!(w.env.read::<Mint>(&mint).supply, supply - ev.burned);
    assert_eq!(w.env.holding(&mint, &creator), 0);
    let c = companion_of(&w, &mint);
    // At most 1 SOL and 1% of the pool's quote side a buyback: what is left waits for the next.
    let pool = w.launch_pool(&mint);
    assert!(ev.spent + ev.bounty <= SOL.min((pool.quote_reserve + pool.virtual_quote) / 100) + 1);
    assert_eq!(c.pending_buyback, pending - ev.spent - ev.bounty);
    assert_eq!(c.burned_total, ev.burned);
    // The next buyback waits for the interval.
    trade(&mut w, &mint, 2, SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees(cranker.pubkey(), mint, None)],
            &cranker,
            &[],
        )
        .ok();
    let tx = w.env.send_paid_by(
        &[companion::buyback(
            cranker.pubkey(),
            &keys(&w, &mint),
            false,
        )],
        &cranker,
        &[],
    );
    tx.expect_code(code(CompanionError::BuybackNotDue));
    w.env.warp(60);
    w.env
        .send_paid_by(
            &[companion::buyback(
                cranker.pubkey(),
                &keys(&w, &mint),
                false,
            )],
            &cranker,
            &[],
        )
        .ok();
}

#[test]
fn the_rug_proof_dev() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(10 * SOL);
    let split = Split {
        buyback_bps: 0,
        holders_bps: 5_000,
        beneficiary_bps: 5_000,
    };
    let rules = presets::burn(); // holder rewards 0.5%, burn 0.5%
    let vest = 30 * 86_400;
    let (mint, tx) = launch_through(&mut w, &launcher, split, vest, rules);
    tx.ok();
    let k = keys(&w, &mint);
    let creator = companion::creator_address(&mint);

    // The dev's buy is the companion's, vesting to them.
    let tx = w.env.send_paid_by(
        &[companion::dev_buy(launcher.pubkey(), &k, SOL / 2, 1)],
        &launcher,
        &[],
    );
    tx.ok();
    let bag = w.env.holding(&mint, &creator);
    assert!(bag > 0);
    assert_eq!(companion_of(&w, &mint).dev_tokens, bag);
    // Only the beneficiary buys for the bag.
    let stranger = w.wallet_with_sol(2 * SOL);
    let tx = w.env.send_paid_by(
        &[companion::dev_buy(stranger.pubkey(), &k, SOL / 2, 1)],
        &stranger,
        &[],
    );
    tx.expect_fail();

    // Half vests in half the time; release is permissionless and pays only the beneficiary.
    w.env.warp(vest / 2);
    w.env
        .send_paid_by(
            &[companion::release(
                stranger.pubkey(),
                &k,
                true,
                launcher.pubkey(),
            )],
            &stranger,
            &[],
        )
        .ok();
    let released = w.env.holding(&mint, &launcher.pubkey());
    assert!(released.abs_diff(bag / 2) <= 1, "{released} of {bag}");
    let tx = w.env.send_paid_by(
        &[companion::release(
            stranger.pubkey(),
            &k,
            true,
            launcher.pubkey(),
        )],
        &stranger,
        &[],
    );
    tx.expect_code(code(CompanionError::NothingToDo));

    // Fees: half to holders through the kit, half to the dev as SOL.
    trade(&mut w, &mint, 6, 3 * SOL);
    let tx = w.env.send_paid_by(
        &[companion::claim_fees(stranger.pubkey(), mint, None)],
        &stranger,
        &[],
    );
    tx.ok();
    let ev: FeesClaimed = tx.event();
    assert_eq!(ev.to_buyback, 0);
    assert!(ev.to_holders > 0 && ev.to_beneficiary > 0);
    let vault = launch::holder_vault_address(&mint, &w.sol);
    let in_vault = w.env.read::<bordrless_token::state::Holding>(&vault).amount;
    w.env
        .send_paid_by(&[companion::share(stranger.pubkey(), mint)], &stranger, &[])
        .ok();
    let shared = companion_of(&w, &mint).shared_total;
    assert!(shared > 0);
    assert_eq!(
        w.env.read::<bordrless_token::state::Holding>(&vault).amount,
        in_vault + shared
    );
    let before = w.env.lamports(&launcher.pubkey());
    w.env
        .send_paid_by(
            &[companion::withdraw(
                stranger.pubkey(),
                mint,
                launcher.pubkey(),
            )],
            &stranger,
            &[],
        )
        .ok();
    assert!(w.env.lamports(&launcher.pubkey()) >= before + ev.to_beneficiary);
}

#[test]
fn what_a_companion_refuses() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(10 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let bad = |split, bounty_bps| CreateArgs {
        bounty_bps,
        ..args(split, 0)
    };
    let send = |w: &mut World, a: CreateArgs| {
        w.env.send_paid_by(
            &[companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                mint,
                a,
            )],
            &launcher,
            &[&mint_kp],
        )
    };
    send(
        &mut w,
        bad(
            Split {
                buyback_bps: 9_000,
                holders_bps: 0,
                beneficiary_bps: 0,
            },
            50,
        ),
    )
    .expect_code(code(CompanionError::BadSplit));
    send(
        &mut w,
        bad(
            Split {
                buyback_bps: 10_000,
                holders_bps: 0,
                beneficiary_bps: 0,
            },
            101,
        ),
    )
    .expect_code(code(CompanionError::BountyTooHigh));
    send(
        &mut w,
        CreateArgs {
            buyback_interval: 10,
            ..args(
                Split {
                    buyback_bps: 10_000,
                    holders_bps: 0,
                    beneficiary_bps: 0,
                },
                0,
            )
        },
    )
    .expect_code(code(CompanionError::BadBuybackLimits));

    // Holders without holder rewards, or the creator wallet lock, are refused at launch.
    let (_, tx) = launch_through(
        &mut w,
        &launcher,
        Split {
            buyback_bps: 0,
            holders_bps: 10_000,
            beneficiary_bps: 0,
        },
        0,
        LaunchRules::NONE,
    );
    tx.expect_code(code(CompanionError::HolderRewardsOff));
    let (_, tx) = launch_through(
        &mut w,
        &launcher,
        Split {
            buyback_bps: 10_000,
            holders_bps: 0,
            beneficiary_bps: 0,
        },
        0,
        presets::rewards_and_cap(),
    );
    tx.expect_code(code(CompanionError::CreatorLockUnsupported));

    // A step pointed at other accounts: the swap's recipient swapped for the cranker's holding.
    let (mint, tx) = launch_through(
        &mut w,
        &launcher,
        Split {
            buyback_bps: 10_000,
            holders_bps: 0,
            beneficiary_bps: 0,
        },
        0,
        LaunchRules::NONE,
    );
    tx.ok();
    w.env.warp(31);
    trade(&mut w, &mint, 3, 2 * SOL);
    let thief = w.wallet_with_sol(SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees(thief.pubkey(), mint, None)],
            &thief,
            &[],
        )
        .ok();
    w.env.warp(61);
    catch_up(&mut w, &thief, &mint);
    let mut ix = companion::buyback(thief.pubkey(), &keys(&w, &mint), false);
    let creator_holding = token::holding_address(&mint, &companion::creator_address(&mint));
    let theirs = token::holding_address(&mint, &thief.pubkey());
    for m in ix
        .accounts
        .iter_mut()
        .filter(|m| m.pubkey == creator_holding)
    {
        m.pubkey = theirs;
    }
    w.env
        .send_paid_by(&[ix], &thief, &[])
        .expect_code(code(CompanionError::MissingAccount));
    // Withdraw pays only the companion's beneficiary.
    w.env
        .send_paid_by(
            &[companion::withdraw(thief.pubkey(), mint, thief.pubkey())],
            &thief,
            &[],
        )
        .expect_fail();
}

/// The launch through a companion fits mainnet's limits as the site sends it: v0 with the protocol
/// lookup table (extended with the companion program and its event authority), the longest
/// metadata the site sends, holder rewards and a burn, at stack height 5 (Solana's limit).
#[test]
fn a_companion_launch_fits_mainnet_limits() {
    use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price};
    let mut w = World::new();
    let mut addresses = protocol_lookup_table(&w);
    let base_table = w.env.put_lookup_table(Pubkey::new_unique(), &addresses);
    addresses.extend([
        bordrless_companion::ID,
        companion::event_authority(),
        bordrless_launch::ID,
        bordrless_swap::ID,
        bordrless_token::ID,
    ]);
    let table = w.env.put_lookup_table(Pubkey::new_unique(), &addresses);
    let launcher = w.wallet_with_sol(5 * SOL);
    let mint = Keypair::new();
    let split = Split {
        buyback_bps: 5_000,
        holders_bps: 5_000,
        beneficiary_bps: 0,
    };
    w.env
        .send_paid_by(
            &[companion::create(
                launcher.pubkey(),
                launcher.pubkey(),
                mint.pubkey(),
                args(split, 0),
            )],
            &launcher,
            &[&mint],
        )
        .ok();
    let creator = companion::creator_address(&mint.pubkey());
    let mut launch_args = World::launch_args("TENCHARSXX", CREATOR_FEE, VQ, presets::burn());
    launch_args.name = "N".repeat(32);
    launch_args.uri = format!("https://gateway.pinata.cloud/ipfs/{}", "b".repeat(94));
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
    let ixs = [
        compute_unit_limit(1_400_000),
        compute_unit_price(20_000),
        ix,
    ];
    let base = w.env.v0_size(&ixs, &launcher, &[&mint], &[base_table]);
    let tx = w.env.send_v0(&ixs, &launcher, &[&mint], &[table]);
    tx.ok();
    println!("companion launch v0: {} bytes with the protocol table, {} with it extended; {} CU; height {}", base, tx.size, tx.cu(), tx.max_height());
    assert!(tx.size <= 1_232, "{} bytes", tx.size);
    assert!(tx.max_height() <= 5);
    assert_eq!(w.launch(&mint.pubkey()).creator, creator);
}

// ---- Native SOL (phase 3) ---------------------------------------------------------------------------

/// Holder rewards 1% both sides, nothing else: a kit token whose holders the companion can pay.
fn rewards() -> LaunchRules {
    bordrless_program_tests::launch::rules(100, 100, 0, 0, 0, 0, 0, 0)
}

/// A companion that buys back, shares with holders and pays its launcher, migrated in place
/// mid-life (pool, launch, companion, the DEX paused): every bucket keeps its meaning, the
/// creator's bridged SOL becomes its lamports one for one, its spare lamports go to the
/// beneficiary's part; then a claim (in SOL), a holders' share (the kit's `share_sol`), a buyback
/// (`swap_v2`, the creator address trading) and the beneficiary's withdraw all move exactly the
/// lamports they say; the old claim path is refused; and it reverts.
#[test]
fn a_companion_migrates_mid_life_and_keeps_every_lamport() {
    let mut w = World::new();
    let launcher = w.wallet_with_sol(5 * SOL);
    let split = Split {
        buyback_bps: 5_000,
        holders_bps: 3_000,
        beneficiary_bps: 2_000,
    };
    let (mint, tx) = launch_through(&mut w, &launcher, split, 0, rewards());
    tx.ok();
    w.env.warp(31);
    trade(&mut w, &mint, 4, 2 * SOL);
    let payer = w.env.funded(10 * SOL);
    let cranker = w.wallet_with_sol(SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees(cranker.pubkey(), mint, None)],
            &payer,
            &[&cranker],
        )
        .ok();
    // More fees, left in the launch for the migration to move.
    trade(&mut w, &mint, 2, SOL);
    let before = companion_of(&w, &mint);
    let set_aside = before.set_aside().unwrap();
    let (held, spare) = w.creator_sol(&mint);
    assert_eq!(
        held, set_aside,
        "the holding holds exactly what is set aside"
    );
    assert!(spare > 0, "the funding the launch did not spend");
    let creator = companion::creator_address(&mint);
    let creator_lamports = w.env.lamports(&creator);
    let launch_key = launch::launch_address(&mint);
    let launch_fees = w.env.holding(&w.sol, &launch_key);
    assert!(launch_fees > 0);

    // Not while the DEX trades, not before its launch is native, not by anyone else.
    let a = w.admin().pubkey();
    w.as_admin(companion::migrate_companion_to_native(a, mint))
        .expect_code(code(CompanionError::DexNotPaused));
    w.pause_dex(true);
    w.as_admin(companion::migrate_companion_to_native(a, mint))
        .expect_code(code(CompanionError::LaunchNotNative));
    let stranger = w.env.funded(SOL);
    w.env
        .send(
            &[companion::migrate_companion_to_native(
                stranger.pubkey(),
                mint,
            )],
            &[&stranger],
        )
        .expect_code(code(CompanionError::NotProtocolAuthority));
    w.pause_dex(false);

    let tx = w.migrate_companion_native(&mint);
    tx.ok();
    let ev: bordrless_companion::events::CompanionNativeSet = tx.event();
    assert_eq!((ev.native, ev.moved, ev.spare), (true, held, spare));
    let c = companion_of(&w, &mint);
    assert!(c.is_native());
    assert_eq!(
        w.env
            .account(&companion::companion_address(&mint))
            .unwrap()
            .data
            .len(),
        Companion::LEN + bordrless_companion::state::NATIVE_SOL_LEN
    );
    assert_eq!(w.creator_sol(&mint), (0, held + spare));
    assert_eq!(w.env.lamports(&creator), creator_lamports + held);
    assert_eq!(c.pending_beneficiary, before.pending_beneficiary + spare);
    assert_eq!(
        (c.pending_buyback, c.pending_holders, c.pending_pot),
        (
            before.pending_buyback,
            before.pending_holders,
            before.pending_pot
        )
    );
    assert_eq!(c.set_aside().unwrap(), held + spare);
    // The launch's fees moved to its lamports with it.
    assert_eq!(
        w.env.lamports(&launch_key) - w.env.rent(bordrless_launch::state::Launch::LEN),
        launch_fees
    );
    w.assert_pool_invariant(&w.launch_pool_key(&mint));
    let keys = keys(&w, &mint);

    // A claim: the launch's lamports to the creator address, the bounty to the cranker.
    let cl = w.env.lamports(&cranker.pubkey());
    let (l0, c0) = (w.env.lamports(&launch_key), w.env.lamports(&creator));
    let tx = w.env.send_paid_by(
        &[companion::native(
            companion::claim_fees(cranker.pubkey(), mint, None),
            &keys,
        )],
        &payer,
        &[&cranker],
    );
    tx.ok();
    let fees: FeesClaimed = tx.event();
    assert_eq!(fees.claimed, launch_fees);
    assert_eq!(l0 - w.env.lamports(&launch_key), launch_fees);
    assert_eq!(w.env.lamports(&cranker.pubkey()) - cl, fees.bounty);
    assert_eq!(
        w.env.lamports(&creator) - c0,
        launch_fees - fees.bounty,
        "lamports conserved"
    );
    let c = companion_of(&w, &mint);
    assert_eq!(c.set_aside().unwrap(), w.creator_sol(&mint).1);
    w.env.warp(1);

    // The holders' share: the kit's lamports.
    let kit = launch::kit_config_address(&mint);
    let (k0, c0) = (w.env.lamports(&kit), w.env.lamports(&creator));
    let pending = c.pending_holders;
    let tx = w.env.send_paid_by(
        &[companion::native(
            companion::share(cranker.pubkey(), mint),
            &keys,
        )],
        &payer,
        &[&cranker],
    );
    tx.ok();
    let shared: bordrless_companion::events::SharedWithHolders = tx.event();
    assert_eq!(shared.amount + shared.bounty, pending);
    assert_eq!(w.env.lamports(&kit) - k0, shared.amount);
    assert_eq!(c0 - w.env.lamports(&creator), pending);
    assert_eq!(
        companion_of(&w, &mint).set_aside().unwrap(),
        w.creator_sol(&mint).1
    );
    w.env.warp(1);

    // A buyback through swap_v2, once the reference has caught up.
    let mut bought = None;
    for _ in 0..80 {
        w.env.warp(61);
        let c0 = w.env.lamports(&creator);
        let pending = companion_of(&w, &mint).pending_buyback;
        let tx = w.env.send_paid_by(
            &[companion::native(
                companion::buyback(cranker.pubkey(), &keys, true),
                &keys,
            )],
            &payer,
            &[&cranker],
        );
        tx.ok();
        if let Some(ev) = tx.events::<BoughtBack>().into_iter().next() {
            assert_eq!(c0 - w.env.lamports(&creator), ev.spent + ev.bounty);
            assert_eq!(
                companion_of(&w, &mint).pending_buyback,
                pending - ev.spent - ev.bounty
            );
            bought = Some(ev);
            break;
        }
    }
    assert!(bought.is_some_and(|b| b.burned > 0));
    w.assert_pool_invariant(&w.launch_pool_key(&mint));

    // The beneficiary's part, the spare included, paid as SOL.
    let pending = companion_of(&w, &mint).pending_beneficiary;
    let bl = w.env.lamports(&launcher.pubkey());
    w.env
        .send_paid_by(
            &[companion::withdraw(payer.pubkey(), mint, launcher.pubkey())],
            &payer,
            &[],
        )
        .ok();
    assert_eq!(w.env.lamports(&launcher.pubkey()) - bl, pending);
    assert_eq!(
        companion_of(&w, &mint).set_aside().unwrap(),
        w.creator_sol(&mint).1
    );
    w.env.warp(1);

    // A claim built as before (no native accounts) serves a native companion too: its accounts
    // already name the launch, the creator and the launchpad, all the SOL claim needs.
    trade(&mut w, &mint, 1, SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees(cranker.pubkey(), mint, None)],
            &payer,
            &[&cranker],
        )
        .ok();
    assert_eq!(
        companion_of(&w, &mint).set_aside().unwrap(),
        w.creator_sol(&mint).1
    );

    // Revert: the creator's lamports above the minimum wrapped back; the old paths again.
    let (_, sol) = w.creator_sol(&mint);
    let tx = w.revert_companion_native(&mint);
    tx.ok();
    assert!(!companion_of(&w, &mint).is_native());
    assert_eq!(w.creator_sol(&mint), (sol, 0));
    trade(&mut w, &mint, 1, SOL);
    w.env
        .send_paid_by(
            &[companion::claim_fees(cranker.pubkey(), mint, None)],
            &payer,
            &[&cranker],
        )
        .ok();
    let c = companion_of(&w, &mint);
    assert_eq!(c.set_aside().unwrap(), w.creator_sol(&mint).0);
}

/// With native launches on, a companion coin is native from its launch: its launch, its pool and
/// its companion (the spare funding booked to the beneficiary), and its first claim pays SOL.
#[test]
fn a_companion_coin_is_native_from_its_launch() {
    let mut w = World::new();
    let a = w.admin().pubkey();
    w.as_admin(launch::set_native_launches(a, true)).ok();
    let launcher = w.wallet_with_sol(5 * SOL);
    let split = Split {
        buyback_bps: 0,
        holders_bps: 0,
        beneficiary_bps: 10_000,
    };
    let (mint, tx) = launch_through(&mut w, &launcher, split, 0, LaunchRules::NONE);
    tx.ok();
    assert!(w.launch(&mint).is_native());
    let c = companion_of(&w, &mint);
    assert!(c.is_native());
    let (held, sol) = w.creator_sol(&mint);
    assert_eq!((held, c.pending_beneficiary), (0, sol));
    w.env.warp(31);
    let payer = w.env.funded(10 * SOL);
    let t = w.env.funded(10 * SOL);
    w.holdings(&t, mint, &[t.pubkey()]);
    let keys = keys(&w, &mint);
    let buy = launch::swap_v2(&keys, t.pubkey(), t.pubkey(), 1, 2 * SOL, 0);
    w.env.send_paid_by(&[buy], &payer, &[&t]).ok();
    let cranker = w.env.funded(SOL);
    let tx = w.env.send_paid_by(
        &[companion::native(
            companion::claim_fees(cranker.pubkey(), mint, None),
            &keys,
        )],
        &payer,
        &[&cranker],
    );
    tx.ok();
    let ev: FeesClaimed = tx.event();
    assert_eq!(ev.claimed, 2 * SOL * u64::from(CREATOR_FEE) / 10_000);
    let c = companion_of(&w, &mint);
    assert_eq!(c.set_aside().unwrap(), w.creator_sol(&mint).1);
    let bl = w.env.lamports(&launcher.pubkey());
    w.env
        .send_paid_by(
            &[companion::withdraw(payer.pubkey(), mint, launcher.pubkey())],
            &payer,
            &[],
        )
        .ok();
    assert_eq!(
        w.env.lamports(&launcher.pubkey()) - bl,
        c.pending_beneficiary
    );
}
