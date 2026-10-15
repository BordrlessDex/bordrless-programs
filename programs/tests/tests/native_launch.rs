//! Native-SOL launches (phase 2): the launchpad and the kit on native pools. A live launch with
//! accrued creator fees and holder rewards is migrated mid-curve (pool, then launch and kit, the DEX
//! paused) and keeps trading through both swaps, claiming in SOL and graduating; it reverts; a new
//! launch is native from creation when the admin turns it on (a companion coin is not); a custom
//! hook's SOL cut joins the creator fee as lamports. Lamports are conserved exactly on every
//! native trade: what leaves the trader reaches the pool, the launch and the kit config, and
//! nothing else.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_hook::token_flags;
use bordrless_kit::error::KitError;
use bordrless_kit::state::KitConfig;
use bordrless_launch::client as launch;
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::{CreatorFeesClaimed, LaunchNativeSet};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::{Launch, LaunchRules, QuoteCut};
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::{SOL, VQ};
use bordrless_swap::client as swap;
use bordrless_swap::error::SwapError;
use bordrless_swap::events::SwappedNative;
use bordrless_swap::state::Pool;
use bordrless_token::state::Mint;
use hook_tester::client as tester;
use hook_tester::quote_mode;
use solana_keypair::Keypair;
use solana_signer::Signer;

/// Holder rewards 1% both sides and no cap (creator 1%).
fn rewards() -> LaunchRules {
    bordrless_program_tests::launch::rules(100, 100, 0, 0, 0, 0, 0, 0)
}

fn admin(w: &World) -> Keypair {
    w.env.deployer.insecure_clone()
}

/// The DEX's `set_config` with `paused`, everything else as it stands.
fn dex_pause_ix(w: &World, paused: bool) -> Instruction {
    let c: bordrless_swap::state::Config = w.env.read(&swap::config_address());
    swap::set_config(
        c.admin,
        bordrless_swap::instructions::ConfigArgs {
            admin: c.admin,
            protocol_fee_bps: c.protocol_fee_bps,
            fee_collector: c.fee_collector,
            treasury: c.treasury,
            pool_creation_fee_lamports: c.pool_creation_fee_lamports,
            paused,
            launch_protocol_share_bps: c.launch_protocol_share_bps,
        },
    )
}

/// Sends `ixs` signed by the admin (who pays).
fn as_admin(w: &mut World, ixs: &[Instruction]) -> Tx {
    let a = admin(w);
    let tx = w.env.send(ixs, &[&a]);
    w.env.warp(1);
    tx
}

fn pool_of(w: &World, mint: &Pubkey) -> Pubkey {
    w.launch_pool_key(mint)
}

/// The migration of a live launch, as the runbook does it: pause the DEX, migrate the pool (its
/// whole quote to lamports), migrate the launch and its kit, unpause. Answers the launch's
/// migration.
fn migrate(w: &mut World, mint: &Pubkey) -> Tx {
    let a = admin(w).pubkey();
    let modules = w.launch(mint).modules;
    let pool = pool_of(w, mint);
    let sol = w.sol;
    {
        let ix = dex_pause_ix(w, true);
        as_admin(w, &[ix])
    }
    .ok();
    as_admin(w, &[swap::migrate_pool_to_native(a, pool, 0)]).ok();
    let tx = as_admin(
        w,
        &[launch::migrate_launch_to_native(a, *mint, sol, modules)],
    );
    {
        let ix = dex_pause_ix(w, false);
        as_admin(w, &[ix])
    }
    .ok();
    tx
}

/// The way back: pause, revert the launch, then its pool, unpause.
fn revert(w: &mut World, mint: &Pubkey) -> Tx {
    let a = admin(w).pubkey();
    let modules = w.launch(mint).modules;
    let pool = pool_of(w, mint);
    let sol = w.sol;
    {
        let ix = dex_pause_ix(w, true);
        as_admin(w, &[ix])
    }
    .ok();
    let tx = as_admin(
        w,
        &[launch::revert_launch_to_bridged(a, *mint, sol, modules)],
    );
    as_admin(w, &[swap::revert_launch_pool_to_bridged(a, pool, *mint)]).ok();
    {
        let ix = dex_pause_ix(w, false);
        as_admin(w, &[ix])
    }
    .ok();
    tx
}

/// A `swap_v2` on a launch pool by `trader`, to itself (the launch client's builder, with the
/// token's slice and any SOL-cut accounts as a client resolves them).
fn v2_ix(w: &World, trader: &Pubkey, mint: &Pubkey, direction: u8, amount: u64) -> Instruction {
    let keys = w.launch_keys(mint);
    let slice = w.launch_base_slice(mint, trader, trader, direction == 1);
    let ix = launch::swap_v2_with_base_slice(&keys, *trader, *trader, direction, amount, 0, slice);
    w.with_quote_cut(ix, mint, trader)
}

/// Sends `ix` signed by `signer`, paid by a separate payer, so the signer's lamports move only by
/// the instruction.
fn send(w: &mut World, payer: &Keypair, ix: Instruction, signer: &Keypair) -> Tx {
    let tx = w.env.send_paid_by(&[ix], payer, &[signer]);
    w.env.warp(1);
    tx
}

fn rent(w: &World, len: usize) -> u64 {
    w.env.rent(len)
}

/// The pool's invariant: lamports = rent + books - buffer, vault = buffer.
#[track_caller]
fn pool_inv(w: &World, pool: &Pubkey) {
    let p: Pool = w.env.read(pool);
    let vault = w.env.holding(&w.sol, pool);
    if p.quote_native() {
        assert_eq!(
            w.env.lamports(pool),
            rent(w, Pool::LEN) + p.quote_reserve + p.protocol_fees_quote - p.buffer_quote()
        );
        assert_eq!(vault, p.buffer_quote());
    } else {
        assert_eq!(w.env.lamports(pool), rent(w, Pool::LEN));
        assert_eq!(vault, p.quote_reserve + p.protocol_fees_quote);
    }
}

/// What the launch holds for its creator: lamports above rent (native) and its holding.
fn launch_fees(w: &World, mint: &Pubkey) -> (u64, u64) {
    let key = launch::launch_address(mint);
    (
        w.env.lamports(&key) - rent(w, Launch::LEN),
        w.env.holding(&w.sol, &key),
    )
}

/// A native kit's vault (its config's lamports above the base); its holding.
fn kit_vault(w: &World, mint: &Pubkey) -> (u64, u64) {
    let key = launch::kit_config_address(mint);
    let c: KitConfig = w.env.read(&key);
    let native = if c.is_native() {
        c.native_vault(w.env.lamports(&key))
    } else {
        0
    };
    (native, w.env.holding(&w.sol, &key))
}

fn kit_claim_sol(w: &mut World, payer: &Keypair, owner: &Keypair, mint: &Pubkey) -> Tx {
    send(
        w,
        payer,
        bordrless_kit::client::claim_sol(owner.pubkey(), *mint),
        owner,
    )
}

/// A launch with holder rewards, two holders and fees in both places, migrated mid-curve: claims
/// before (bridged) and after (SOL), both swap paths, exact lamport conservation, the kit's
/// accounting untouched by the move, graduation, and the old paths refused where they must be.
#[test]
fn a_live_launch_migrates_mid_curve_and_keeps_every_fee() {
    let mut w = World::new();
    let payer = w.env.funded(100 * SOL);
    let creator = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = w.create_launch_with(&creator, "MIG", 100, VQ, rewards());
    tx.ok();
    w.env.warp(31);
    let pool = pool_of(&w, &mint);
    let (a, b) = (w.wallet_with_sol(30 * SOL), w.wallet_with_sol(30 * SOL));
    w.holdings(&a, mint, &[a.pubkey(), b.pubkey(), creator.pubkey()]);
    w.buy(&a, &mint, SOL / 2).ok();
    w.buy(&b, &mint, SOL / 2).ok();
    w.sell(&b, &mint, w.env.holding(&mint, &b.pubkey()) / 4)
        .ok();

    // Before: fees in bridged SOL, claimed the old way.
    let (_, held) = launch_fees(&w, &mint);
    assert!(held > 0);
    let cq = w.env.holding(&w.sol, &creator.pubkey());
    w.env
        .send(
            &[launch::claim_creator_fees(creator.pubkey(), mint, w.sol)],
            &[&creator],
        )
        .ok();
    assert_eq!(w.env.holding(&w.sol, &creator.pubkey()), cq + held);
    w.env.warp(1);
    w.buy(&b, &mint, SOL / 2).ok();
    let aq = w.env.holding(&w.sol, &a.pubkey());
    w.env
        .send(
            &[bordrless_kit::client::claim(a.pubkey(), mint, w.sol)],
            &[&a],
        )
        .ok();
    assert!(
        w.env.holding(&w.sol, &a.pubkey()) > aq,
        "a holder claims bridged SOL"
    );
    w.env.warp(1);
    // More fees, left unclaimed for the move.
    w.buy(&b, &mint, SOL).ok();
    let (_, held) = launch_fees(&w, &mint);
    let (_, kit_held) = kit_vault(&w, &mint);
    assert!(held > 0 && kit_held > 0);
    let kit_before: KitConfig = w.env.read(&launch::kit_config_address(&mint));
    let l_before = w.launch(&mint);
    let launch_lamports = w.env.lamports(&launch::launch_address(&mint));
    let supply = w.env.read::<Mint>(&w.sol).supply;
    let books = {
        let p: Pool = w.env.read(&pool);
        p.quote_reserve + p.protocol_fees_quote
    };

    // Only while the DEX is paused, only once the pool is native, only the admin.
    let ad = admin(&w).pubkey();
    let modules = l_before.modules;
    let sol = w.sol;
    as_admin(
        &mut w,
        &[launch::migrate_launch_to_native(ad, mint, sol, modules)],
    )
    .expect_code(u32::from(LaunchError::DexNotPaused));
    {
        let ix = dex_pause_ix(&w, true);
        as_admin(&mut w, &[ix])
    }
    .ok();
    as_admin(
        &mut w,
        &[launch::migrate_launch_to_native(ad, mint, sol, modules)],
    )
    .expect_code(u32::from(LaunchError::PoolNotNative));
    let stranger = w.env.funded(SOL);
    w.env
        .send(
            &[launch::migrate_launch_to_native(
                stranger.pubkey(),
                mint,
                sol,
                modules,
            )],
            &[&stranger],
        )
        .expect_code(u32::from(LaunchError::NotAdmin));
    {
        let ix = dex_pause_ix(&w, false);
        as_admin(&mut w, &[ix])
    }
    .ok();

    let tx = migrate(&mut w, &mint);
    tx.ok();
    let ev: LaunchNativeSet = tx.event();
    assert_eq!((ev.native, ev.moved), (true, held));
    assert!(w.launch(&mint).is_native());
    assert_eq!(launch_fees(&w, &mint), (held, 0));
    assert_eq!(
        w.env.lamports(&launch::launch_address(&mint)),
        launch_lamports + held
    );
    assert_eq!(kit_vault(&w, &mint), (kit_held, 0));
    let kit_after: KitConfig = w.env.read(&launch::kit_config_address(&mint));
    assert!(kit_after.is_native());
    assert_eq!(
        (
            kit_after.seen,
            kit_after.acc_per_share,
            kit_after.total_claimed,
            kit_after.held
        ),
        (
            kit_before.seen,
            kit_before.acc_per_share,
            kit_before.total_claimed,
            kit_before.held
        )
    );
    // Every bridged lamport that moved was burned and released one for one.
    assert_eq!(
        w.env.read::<Mint>(&w.sol).supply,
        supply - books - held - kit_held
    );
    let l = w.launch(&mint);
    assert_eq!(
        (
            l.creator_fees_accrued,
            l.creator_fees_claimed,
            l.quote_cut_accrued()
        ),
        (
            l_before.creator_fees_accrued,
            l_before.creator_fees_claimed,
            l_before.quote_cut_accrued()
        )
    );
    pool_inv(&w, &pool);
    // Again: nothing moves.
    let again = migrate(&mut w, &mint);
    again.ok();
    assert_eq!(launch_fees(&w, &mint), (held, 0));

    // A swap_v2 buy: every lamport the trader pays lands on the pool, the launch or the kit.
    let watch = |w: &World| {
        (
            w.env.lamports(&pool),
            w.env.lamports(&launch::launch_address(&mint)),
            w.env.lamports(&launch::kit_config_address(&mint)),
        )
    };
    let (p0, l0, k0) = watch(&w);
    let al = w.env.lamports(&a.pubkey());
    let ix = v2_ix(&w, &a.pubkey(), &mint, 1, SOL);
    let tx = send(&mut w, &payer, ix, &a);
    tx.ok();
    let (p1, l1, k1) = watch(&w);
    assert_eq!(al - w.env.lamports(&a.pubkey()), SOL);
    assert_eq!((p1 - p0) + (l1 - l0) + (k1 - k0), SOL, "lamports conserved");
    assert!(l1 > l0 && k1 > k0, "creator and holder fees as lamports");
    let nev: SwappedNative = tx.event();
    assert!(nev.quote_deltas.iter().all(|d| !d.wrapped));
    pool_inv(&w, &pool);

    // A swap_v2 sell: what leaves the pool reaches the seller, the launch and the kit.
    let (p0, l0, k0) = watch(&w);
    let bl = w.env.lamports(&b.pubkey());
    let ix = v2_ix(
        &w,
        &b.pubkey(),
        &mint,
        0,
        w.env.holding(&mint, &b.pubkey()) / 3,
    );
    send(&mut w, &payer, ix, &b).ok();
    let (p1, l1, k1) = watch(&w);
    let got = w.env.lamports(&b.pubkey()) - bl;
    assert_eq!(p0 - p1, got + (l1 - l0) + (k1 - k0), "lamports conserved");
    pool_inv(&w, &pool);

    // The old swap on the converted pool: the trader pays bridged SOL; the cuts still reach the
    // launch and the kit as lamports, paid by the pool, whose buffer takes the bridged SOL.
    let (p0, l0, k0) = watch(&w);
    let buffer0 = w.env.read::<Pool>(&pool).buffer_quote();
    let bq = w.env.holding(&w.sol, &b.pubkey());
    w.buy(&b, &mint, SOL / 2).ok();
    w.env.warp(1);
    let (p1, l1, k1) = watch(&w);
    let buffer1 = w.env.read::<Pool>(&pool).buffer_quote();
    assert_eq!(bq - w.env.holding(&w.sol, &b.pubkey()), SOL / 2);
    assert_eq!(buffer1 - buffer0, SOL / 2, "all of it into the buffer");
    assert_eq!(
        p0 - p1,
        (l1 - l0) + (k1 - k0),
        "the pool paid the cuts in lamports"
    );
    pool_inv(&w, &pool);
    // An old sell is paid from the buffer.
    w.sell(&b, &mint, w.env.holding(&mint, &b.pubkey()) / 10)
        .ok();
    w.env.warp(1);
    pool_inv(&w, &pool);

    // Claims in SOL: the creator's whole lamports above rent; the old claim finds nothing.
    let (fees, _) = launch_fees(&w, &mint);
    let cl = w.env.lamports(&creator.pubkey());
    let tx = send(
        &mut w,
        &payer,
        launch::claim_creator_fees_sol(creator.pubkey(), mint, None, None),
        &creator,
    );
    tx.ok();
    assert_eq!(tx.event::<CreatorFeesClaimed>().amount, fees);
    assert_eq!(w.env.lamports(&creator.pubkey()), cl + fees);
    assert_eq!(launch_fees(&w, &mint), (0, 0));
    assert_eq!(
        w.launch(&mint).creator_fees_claimed,
        l_before.creator_fees_claimed + fees
    );
    let tx = w.env.send(
        &[launch::claim_creator_fees(creator.pubkey(), mint, w.sol)],
        &[&creator],
    );
    tx.expect_code(u32::from(LaunchError::NothingToClaim));
    // Holders claim SOL; the old claim is refused clearly.
    let al = w.env.lamports(&a.pubkey());
    let (kv, _) = kit_vault(&w, &mint);
    let tx = kit_claim_sol(&mut w, &payer, &a, &mint);
    tx.ok();
    let got = w.env.lamports(&a.pubkey()) - al;
    assert!(got > 0);
    assert_eq!(kit_vault(&w, &mint).0, kv - got);
    let tx = w.env.send(
        &[bordrless_kit::client::claim(b.pubkey(), mint, w.sol)],
        &[&b],
    );
    tx.expect_code(u32::from(KitError::NativeKit));
    w.env.warp(1);

    // Graduation (old buys fill the curve through the buffer, then a rebalance), then both swaps.
    let (_, tx) = w.graduate_launch(&mint);
    tx.ok();
    assert_eq!(
        w.launch(&mint).status,
        bordrless_launch::constants::STATUS_GRADUATED
    );
    pool_inv(&w, &pool);
    let cranker = w.env.funded(SOL);
    w.env
        .send(&[swap::rebalance_pool(cranker.pubkey(), pool)], &[&cranker])
        .ok();
    w.env.warp(1);
    pool_inv(&w, &pool);
    let ix = v2_ix(
        &w,
        &a.pubkey(),
        &mint,
        0,
        w.env.holding(&mint, &a.pubkey()) / 2,
    );
    send(&mut w, &payer, ix, &a).ok();
    pool_inv(&w, &pool);
    let ix = v2_ix(&w, &a.pubkey(), &mint, 1, SOL / 4);
    send(&mut w, &payer, ix, &a).ok();
    pool_inv(&w, &pool);
    send(
        &mut w,
        &payer,
        launch::claim_creator_fees_sol(creator.pubkey(), mint, None, None),
        &creator,
    )
    .ok();
    kit_claim_sol(&mut w, &payer, &b, &mint).ok();
}

/// Revert: the launch's lamports wrap back into its holding and the kit's into its vault, the
/// accounting untouched; the old claims pay them; idempotent.
#[test]
fn a_migrated_launch_reverts() {
    let mut w = World::new();
    let payer = w.env.funded(100 * SOL);
    let creator = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = w.create_launch_with(&creator, "REV", 100, VQ, rewards());
    tx.ok();
    w.env.warp(31);
    let a = w.wallet_with_sol(30 * SOL);
    w.holdings(&a, mint, &[a.pubkey(), creator.pubkey()]);
    w.buy(&a, &mint, SOL).ok();
    migrate(&mut w, &mint).ok();
    let ix = v2_ix(&w, &a.pubkey(), &mint, 1, SOL);
    send(&mut w, &payer, ix, &a).ok();
    let (fees, _) = launch_fees(&w, &mint);
    let (kv, _) = kit_vault(&w, &mint);
    let kit_before: KitConfig = w.env.read(&launch::kit_config_address(&mint));
    let admin_lamports = w.env.lamports(&admin(&w).pubkey());

    let tx = revert(&mut w, &mint);
    tx.ok();
    let ev: LaunchNativeSet = tx.event();
    assert_eq!((ev.native, ev.moved), (false, fees));
    assert!(!w.launch(&mint).is_native());
    assert_eq!(launch_fees(&w, &mint), (0, fees));
    assert_eq!(kit_vault(&w, &mint), (0, kv));
    let kit_after: KitConfig = w.env.read(&launch::kit_config_address(&mint));
    assert!(!kit_after.is_native());
    assert_eq!(
        (kit_after.seen, kit_after.acc_per_share),
        (kit_before.seen, kit_before.acc_per_share)
    );
    // The admin wrapped everything and ends where it started (less its transaction fees).
    assert!(admin_lamports - w.env.lamports(&admin(&w).pubkey()) < SOL / 100);
    pool_inv(&w, &pool_of(&w, &mint));
    // Idempotent.
    revert(&mut w, &mint).ok();
    assert_eq!(launch_fees(&w, &mint), (0, fees));

    // Bridged again: the old paths.
    w.buy(&a, &mint, SOL / 2).ok();
    let cq = w.env.holding(&w.sol, &creator.pubkey());
    w.env
        .send(
            &[launch::claim_creator_fees(creator.pubkey(), mint, w.sol)],
            &[&creator],
        )
        .ok();
    assert!(w.env.holding(&w.sol, &creator.pubkey()) > cq + fees);
    w.env
        .send(
            &[bordrless_kit::client::claim(a.pubkey(), mint, w.sol)],
            &[&a],
        )
        .ok();
    let tx = send(
        &mut w,
        &payer,
        launch::claim_creator_fees_sol(creator.pubkey(), mint, None, None),
        &creator,
    );
    tx.expect_code(u32::from(LaunchError::NotNativeLaunch));
}

/// A kit with no holder rewards (a max wallet and a creator lock: modules 6, as live coins have):
/// the kit's id stands in for its reward vault, read-only, and the launch migrates and reverts
/// (found by the cloned-mainnet rehearsal: the slot was declared writable, so every such coin's
/// migration failed with `ConstraintMut`).
#[test]
fn a_kit_without_holder_rewards_migrates_and_reverts() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(20 * SOL);
    let no_rewards = bordrless_program_tests::launch::rules(0, 0, 0, 0, 200, 86_400, 0, 0);
    let (mint, tx) = w.create_launch_with(&creator, "NRW", 100, VQ, no_rewards);
    tx.ok();
    let modules = w.launch(&mint).modules;
    assert_ne!(modules, 0);
    assert_eq!(modules & bordrless_kit::modules::HOLDER_REWARDS, 0);
    w.env.warp(31);
    let a = w.wallet_with_sol(30 * SOL);
    w.holdings(&a, mint, &[a.pubkey(), creator.pubkey()]);
    w.buy(&a, &mint, SOL / 10).ok();
    let (_, fees) = launch_fees(&w, &mint);
    let tx = migrate(&mut w, &mint);
    tx.ok();
    assert!(w.launch(&mint).is_native());
    let kit: KitConfig = w.env.read(&launch::kit_config_address(&mint));
    assert!(kit.is_native());
    assert_eq!(launch_fees(&w, &mint), (fees, 0));
    pool_inv(&w, &pool_of(&w, &mint));
    revert(&mut w, &mint).ok();
    assert!(!w.launch(&mint).is_native());
    assert_eq!(launch_fees(&w, &mint), (0, fees));
}

/// New launches are native once the admin turns it on: the pool, the launch and the kit, from
/// creation; trades, claims (with rent for a fresh wallet) and graduation, end to end.
#[test]
fn a_new_launch_is_native_end_to_end() {
    let mut w = World::new();
    let payer = w.env.funded(100 * SOL);
    let ad = admin(&w).pubkey();
    let stranger = w.env.funded(SOL);
    w.env
        .send(
            &[launch::set_native_launches(stranger.pubkey(), true)],
            &[&stranger],
        )
        .expect_code(u32::from(LaunchError::NotAdmin));
    as_admin(&mut w, &[launch::set_native_launches(ad, true)]).ok();

    let creator = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = w.create_launch_with(&creator, "NEW", 100, VQ, rewards());
    tx.ok();
    let pool = pool_of(&w, &mint);
    let p: Pool = w.env.read(&pool);
    assert!(p.quote_native() && p.curve);
    assert!(w.launch(&mint).is_native());
    let k: KitConfig = w.env.read(&launch::kit_config_address(&mint));
    assert!(k.is_native());
    assert_eq!(k.lamports_base(), rent(&w, KitConfig::LEN));
    pool_inv(&w, &pool);
    w.env.warp(31);

    // A fresh wallet with only SOL buys and sells.
    let a = w.env.funded(40 * SOL);
    let b = w.env.funded(40 * SOL);
    w.holdings(&a, mint, &[a.pubkey(), b.pubkey(), creator.pubkey()]);
    for (t, amount) in [(&a, 2 * SOL), (&b, SOL)] {
        let ix = v2_ix(&w, &t.pubkey(), &mint, 1, amount);
        send(&mut w, &payer, ix, t).ok();
        pool_inv(&w, &pool);
    }
    let ix = v2_ix(
        &w,
        &b.pubkey(),
        &mint,
        0,
        w.env.holding(&mint, &b.pubkey()) / 2,
    );
    send(&mut w, &payer, ix, &b).ok();
    pool_inv(&w, &pool);
    assert_eq!(launch_fees(&w, &mint).1, 0, "nothing in the old holding");
    assert!(launch_fees(&w, &mint).0 > 0);

    // The holders claim SOL; anyone may share SOL.
    let al = w.env.lamports(&a.pubkey());
    kit_claim_sol(&mut w, &payer, &a, &mint).ok();
    assert!(w.env.lamports(&a.pubkey()) > al);
    send(
        &mut w,
        &payer,
        bordrless_kit::client::share_sol(b.pubkey(), mint, SOL / 100),
        &b,
    )
    .ok();
    let tx = w.env.send(
        &[bordrless_kit::client::share(
            b.pubkey(),
            mint,
            bordrless_token::client::holding_address(&w.sol, &b.pubkey()),
            w.sol,
            SOL / 100,
        )],
        &[&b],
    );
    tx.expect_code(u32::from(KitError::NativeKit));

    // The creator's claim, in SOL, to its wallet.
    let (fees, _) = launch_fees(&w, &mint);
    let cl = w.env.lamports(&creator.pubkey());
    send(
        &mut w,
        &payer,
        launch::claim_creator_fees_sol(creator.pubkey(), mint, None, None),
        &creator,
    )
    .ok();
    assert_eq!(w.env.lamports(&creator.pubkey()), cl + fees);
    send(
        &mut w,
        &payer,
        launch::claim_creator_fees_sol(creator.pubkey(), mint, None, None),
        &creator,
    )
    .expect_code(u32::from(LaunchError::NothingToClaim));

    // Graduation, then trades both ways.
    let (_, tx) = w.graduate_launch(&mint);
    tx.ok();
    pool_inv(&w, &pool);
    let cranker = w.env.funded(SOL);
    w.env
        .send(&[swap::rebalance_pool(cranker.pubkey(), pool)], &[&cranker])
        .ok();
    w.env.warp(1);
    let ix = v2_ix(
        &w,
        &a.pubkey(),
        &mint,
        0,
        w.env.holding(&mint, &a.pubkey()) / 2,
    );
    send(&mut w, &payer, ix, &a).ok();
    pool_inv(&w, &pool);
    kit_claim_sol(&mut w, &payer, &b, &mint).ok();
}

/// A fresh native pool holds no lamports yet: an old-path buy, whose cuts the pool pays in
/// lamports while the trader's bridged SOL fills the buffer, fails clearly until a swap_v2 trade
/// or a rebalance gives it some.
#[test]
fn an_old_buy_on_a_fresh_native_launch_waits_for_lamports() {
    let mut w = World::new();
    let ad = admin(&w).pubkey();
    as_admin(&mut w, &[launch::set_native_launches(ad, true)]).ok();
    let creator = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = w.create_launch(&creator, "OLD", 100, VQ);
    tx.ok();
    w.env.warp(31);
    let a = w.wallet_with_sol(10 * SOL);
    w.holdings(&a, mint, &[a.pubkey()]);
    w.buy(&a, &mint, SOL)
        .expect_code(u32::from(SwapError::LamportsShort));
    let payer = w.env.funded(10 * SOL);
    let ix = v2_ix(&w, &a.pubkey(), &mint, 1, SOL);
    send(&mut w, &payer, ix, &a).ok();
    w.buy(&a, &mint, SOL / 2).ok();
    pool_inv(&w, &pool_of(&w, &mint));
}

/// A launch made before native launches were on stays bridged until migrated (a companion coin's
/// companion, phase 3, follows its launch: `companion.rs`); the launchpad's companion check.
#[test]
fn a_launch_made_before_native_launches_stays_bridged() {
    let mut w = World::new();
    let ad = admin(&w).pubkey();
    let creator = w.wallet_with_sol(20 * SOL);
    let (old, tx) = w.create_launch(&creator, "OLD", 100, VQ);
    tx.ok();
    as_admin(&mut w, &[launch::set_native_launches(ad, true)]).ok();
    assert!(!w.launch(&old).is_native());
    assert!(!w.env.read::<Pool>(&pool_of(&w, &old)).quote_native());
    // The companion check is the launchpad's own: the companion's creator PDA of a mint.
    let mint = Pubkey::new_unique();
    let companion_creator = Pubkey::find_program_address(
        &[
            bordrless_kit::constants::COMPANION_CREATOR_SEED,
            mint.as_ref(),
        ],
        &bordrless_kit::constants::COMPANION_ID,
    )
    .0;
    assert!(bordrless_launch::constants::is_companion_creator(
        &companion_creator,
        &mint
    ));
    assert!(!bordrless_launch::constants::is_companion_creator(
        &creator.pubkey(),
        &mint
    ));
}

/// A custom hook's SOL cut (`create_quote_cut_config`, four extra accounts) on a native launch:
/// it joins the creator fee as lamports on the launch, `quote_cut_accrued` counts it as before,
/// and the SOL claim pays it.
#[test]
fn the_sol_cut_works_on_a_native_launch() {
    let mut w = World::new();
    let payer = w.env.funded(100 * SOL);
    let ad = admin(&w).pubkey();
    as_admin(&mut w, &[launch::set_native_launches(ad, true)]).ok();
    let creator = w.wallet_with_sol(50 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_paid_by(
            &[
                tester::init_script(creator.pubkey(), mint, vec![]),
                tester::init_quote_state(creator.pubkey(), mint),
            ],
            &creator,
            &[],
        )
        .ok();
    let config = Keypair::new();
    let ix = launch::create_quote_cut_config(
        creator.pubkey(),
        config.pubkey(),
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: 100,
            custom_hook: Some(hook_tester::ID),
            custom_hook_flags: token_flags::BEFORE_TRANSFER,
            label: "Native cut".to_string(),
        },
        0,
        QuoteCut {
            max_buy_bps: 500,
            max_sell_bps: 500,
        },
    );
    w.env.send_paid_by(&[ix], &creator, &[&config]).ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "CUT", VQ, &config.pubkey());
    w.env.send_paid_by(&[ix], &creator, &[&mint_kp]).ok();
    assert!(w.launch(&mint).is_native());
    w.env.warp(31);
    w.env
        .send_paid_by(
            &[tester::set_quote_answer(
                creator.pubkey(),
                mint,
                quote_mode::RETURN,
                300u16.to_le_bytes().to_vec(),
            )],
            &creator,
            &[],
        )
        .ok();

    let t = w.env.funded(20 * SOL);
    w.holdings(&t, mint, &[t.pubkey()]);
    let pool = pool_of(&w, &mint);
    let (l0, p0) = (
        w.env.lamports(&launch::launch_address(&mint)),
        w.env.lamports(&pool),
    );
    let ix = v2_ix(&w, &t.pubkey(), &mint, 1, 2 * SOL);
    send(&mut w, &payer, ix, &t).ok();
    let l = w.launch(&mint);
    // 1% creator fee and the 3% cut, both lamports on the launch.
    let cut = l.quote_cut_accrued();
    assert_eq!(cut, 2 * SOL * 3 / 100);
    let creator_part = w.env.lamports(&launch::launch_address(&mint)) - l0;
    assert_eq!(creator_part, l.creator_fees_accrued);
    assert_eq!(creator_part, cut + 2 * SOL / 100);
    assert_eq!((w.env.lamports(&pool) - p0) + creator_part, 2 * SOL);
    pool_inv(&w, &pool);

    // A sell's cut likewise.
    let ix = v2_ix(
        &w,
        &t.pubkey(),
        &mint,
        0,
        w.env.holding(&mint, &t.pubkey()) / 2,
    );
    send(&mut w, &payer, ix, &t).ok();
    assert!(w.launch(&mint).quote_cut_accrued() > cut);
    pool_inv(&w, &pool);

    let (fees, _) = launch_fees(&w, &mint);
    let cl = w.env.lamports(&creator.pubkey());
    send(
        &mut w,
        &payer,
        launch::claim_creator_fees_sol(creator.pubkey(), mint, None, None),
        &creator,
    )
    .ok();
    assert_eq!(w.env.lamports(&creator.pubkey()), cl + fees);
}

/// A native launch from a partner config that also pays its listed author: every SOL claim (the
/// creator's, the author's, anyone's `distribute_fees_sol`) pays each party its exact part as
/// lamports to its wallet, the parts summing to what the launch held; a party whose wallet would be
/// left below the rent-exempt minimum is refused clearly.
#[test]
fn author_and_partner_claims_pay_sol_to_every_wallet() {
    let mut w = World::new();
    let payer = w.env.funded(100 * SOL);
    let ad = admin(&w).pubkey();
    as_admin(&mut w, &[launch::set_native_launches(ad, true)]).ok();
    let author = w.wallet_with_sol(5 * SOL);
    let partner = Keypair::new();
    let config = Keypair::new();
    let (author_bps, partner_bps) = (2_000u16, 3_000u16);
    let ix = launch::create_partner_config(
        author.pubkey(),
        config.pubkey(),
        CreateConfigArgs {
            rules: LaunchRules::NONE,
            creator_fee_bps: 200,
            custom_hook: None,
            custom_hook_flags: 0,
            label: "Partnered".to_string(),
        },
        author_bps,
        QuoteCut::default(),
        partner.pubkey(),
        partner_bps,
    );
    w.env.send_paid_by(&[ix], &author, &[&config]).ok();
    let creator = w.wallet_with_sol(5 * SOL);
    let mint = Keypair::new();
    let ix = w.create_launch_from_config_ix(
        &creator.pubkey(),
        &mint.pubkey(),
        "PRT",
        VQ,
        &config.pubkey(),
    );
    w.env.send_paid_by(&[ix], &creator, &[&mint]).ok();
    let mint = mint.pubkey();
    let l = w.launch(&mint);
    assert!(l.is_native());
    assert_eq!(
        (l.author_share_bps, l.partner_share_bps()),
        (author_bps, partner_bps)
    );
    w.env.warp(31);
    let t = w.env.funded(50 * SOL);
    w.holdings(&t, mint, &[t.pubkey()]);
    let buy = |w: &mut World, amount: u64| {
        let ix = v2_ix(w, &t.pubkey(), &mint, 1, amount);
        send(w, &payer, ix, &t).ok();
    };
    let wallets = [creator.pubkey(), author.pubkey(), partner.pubkey()];
    let lamports = |w: &World| wallets.map(|k| w.env.lamports(&k));
    let split = |amount: u64| {
        let a = amount * u64::from(author_bps) / 10_000;
        let p = amount * u64::from(partner_bps) / 10_000;
        [amount - a - p, a, p]
    };
    let (cfg, au, pa) = (config.pubkey(), author.pubkey(), partner.pubkey());

    // A small claim: the partner's wallet does not exist and its part can't open it.
    buy(&mut w, SOL / 100);
    send(
        &mut w,
        &payer,
        launch::claim_creator_fees_sol(creator.pubkey(), mint, Some((cfg, au)), Some((cfg, pa))),
        &creator,
    )
    .expect_code(u32::from(LaunchError::RecipientBelowRent));

    // The creator's claim: each its part.
    buy(&mut w, 5 * SOL);
    let (fees, _) = launch_fees(&w, &mint);
    let before = lamports(&w);
    send(
        &mut w,
        &payer,
        launch::claim_creator_fees_sol(creator.pubkey(), mint, Some((cfg, au)), Some((cfg, pa))),
        &creator,
    )
    .ok();
    let after = lamports(&w);
    assert_eq!(
        [0, 1, 2].map(|i| after[i] - before[i]),
        split(fees),
        "creator, author, partner"
    );
    assert_eq!(launch_fees(&w, &mint), (0, 0));
    let l = w.launch(&mint);
    assert_eq!(l.author_fees_paid, split(fees)[1]);

    // The author's claim, then anyone's distribution.
    buy(&mut w, 3 * SOL);
    let (fees, _) = launch_fees(&w, &mint);
    let before = lamports(&w);
    send(
        &mut w,
        &payer,
        launch::claim_author_fees_sol(au, mint, cfg, creator.pubkey(), Some(pa)),
        &author,
    )
    .ok();
    let after = lamports(&w);
    assert_eq!([0, 1, 2].map(|i| after[i] - before[i]), split(fees));
    buy(&mut w, 3 * SOL);
    let (fees, _) = launch_fees(&w, &mint);
    let before = lamports(&w);
    let anyone = w.env.funded(SOL);
    w.env
        .send_paid_by(
            &[launch::distribute_fees_sol(
                mint,
                cfg,
                creator.pubkey(),
                pa,
                Some(au),
            )],
            &anyone,
            &[],
        )
        .ok();
    let after = lamports(&w);
    assert_eq!([0, 1, 2].map(|i| after[i] - before[i]), split(fees));
    assert_eq!(launch_fees(&w, &mint), (0, 0));
}
