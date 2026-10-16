//! `claim_sol_for` (native kits): anyone pushes a holder's rewards to it in SOL, the holder not
//! signing. It pays exactly what the holder's own `claim_sol` would (the mirror's claimable, at
//! most the vault), only to the holding's owner, with the same checks (excluded owners, nothing
//! owed, a recipient left below rent) and the same `RewardsClaimed` event; the payer pays the
//! transaction fee and nothing else. The holder's reward bytes are written as a claim writes them,
//! so its own `claim_sol` then finds nothing until new rewards arrive. Several pushes to different
//! holders fit one transaction. Bridged kits refuse it (`NotNativeKit`), kits without holder
//! rewards too (`RewardsOff`).

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use bordrless_kit::client as kit;
use bordrless_kit::error::KitError;
use bordrless_kit::events::RewardsClaimed;
use bordrless_kit::mirror;
use bordrless_kit::state::KitConfig;
use bordrless_launch::client as launch;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::kit::refused_by_kit;
use bordrless_program_tests::launch::{SOL, VQ};
use bordrless_token::client as token;
use bordrless_token::state::Holding;
use solana_keypair::Keypair;
use solana_signer::Signer;

/// The fee of a transaction with one signature (no priority fee).
const FEE: u64 = 5_000;
/// The largest transaction on the wire.
const PACKET: usize = 1232;

/// Holder rewards 1% both sides and no cap (creator 1%).
fn rewards() -> LaunchRules {
    bordrless_program_tests::launch::rules(100, 100, 0, 0, 0, 0, 0, 0)
}

/// Sends `ix` signed by `signer`, paid by `payer`.
fn send(w: &mut World, payer: &Keypair, ix: Instruction, signer: &Keypair) -> Tx {
    let tx = w.env.send_paid_by(&[ix], payer, &[signer]);
    w.env.warp(1);
    tx
}

/// Sends `ixs` signed and paid by `payer` alone.
fn push(w: &mut World, payer: &Keypair, ixs: &[Instruction]) -> Tx {
    let tx = w.env.send_paid_by(ixs, payer, &[]);
    w.env.warp(1);
    tx
}

/// A `swap_v2` on a launch pool by `trader`, to itself.
fn v2_ix(w: &World, trader: &Pubkey, mint: &Pubkey, direction: u8, amount: u64) -> Instruction {
    let keys = w.launch_keys(mint);
    let slice = w.launch_base_slice(mint, trader, trader, direction == 1);
    let ix = launch::swap_v2_with_base_slice(&keys, *trader, *trader, direction, amount, 0, slice);
    w.with_quote_cut(ix, mint, trader)
}

/// `trader` buys `lamports` worth through `swap_v2`, `payer` paying the fee.
fn buy(w: &mut World, payer: &Keypair, trader: &Keypair, mint: &Pubkey, lamports: u64) {
    let ix = v2_ix(w, &trader.pubkey(), mint, 1, lamports);
    send(w, payer, ix, trader).ok();
}

/// A native kit's vault: its config's lamports above the base.
fn kit_vault(w: &World, mint: &Pubkey) -> u64 {
    let key = kit::kit_config_address(mint);
    let c: KitConfig = w.env.read(&key);
    assert!(c.is_native());
    c.native_vault(w.env.lamports(&key))
}

/// What `owner`'s claim would pay now, by the mirror: its claimable, at most the vault.
fn owed(w: &World, mint: &Pubkey, owner: &Pubkey) -> u64 {
    let c: KitConfig = w.env.read(&kit::kit_config_address(mint));
    let vault = kit_vault(w, mint);
    let h: Holding = w.env.read(&token::holding_address(mint, owner));
    mirror::claimable(&c, vault, owner, h.amount, &h.hook_data, w.env.now)
        .expect("the mirror computes")
        .min(vault)
}

fn hook_data(w: &World, mint: &Pubkey, owner: &Pubkey) -> [u8; 64] {
    w.env
        .read::<Holding>(&token::holding_address(mint, owner))
        .hook_data
}

/// A native launch (native launches on before it is made) with holder rewards, `n` holders that
/// bought through `swap_v2`, and a last buy by another trader so that each of them is owed.
/// Answers the world, the mint, a payer for the holders' trades, the holders and the creator.
fn native_world(n: usize, rules: LaunchRules) -> (World, Pubkey, Keypair, Vec<Keypair>, Keypair) {
    let mut w = World::new();
    let payer = w.env.funded(1_000 * SOL);
    let admin = w.env.deployer.insecure_clone();
    w.env
        .send(
            &[launch::set_native_launches(admin.pubkey(), true)],
            &[&admin],
        )
        .ok();
    w.env.warp(1);
    let creator = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = w.create_launch_with(&creator, "PSH", 100, VQ, rules);
    tx.ok();
    let k: KitConfig = w.env.read(&kit::kit_config_address(&mint));
    assert!(k.is_native());
    w.env.warp(31);
    let holders: Vec<Keypair> = (0..n).map(|_| w.env.funded(10 * SOL)).collect();
    for (i, h) in holders.iter().enumerate() {
        w.holdings(h, mint, &[h.pubkey()]);
        buy(&mut w, &payer, h, &mint, SOL / 2 + (i as u64) * SOL / 10);
    }
    let last = w.env.funded(10 * SOL);
    w.holdings(&last, mint, &[last.pubkey()]);
    buy(&mut w, &payer, &last, &mint, 2 * SOL);
    (w, mint, payer, holders, creator)
}

/// A third party pushes a holder's rewards: the holder (not signing) receives exactly what its own
/// claim would pay, the vault pays it, the payer only the fee, and the event is `claim_sol`'s.
#[test]
fn a_third_party_pushes_and_the_owner_is_paid() {
    let (mut w, mint, _, holders, _) = native_world(2, rewards());
    let a = &holders[0];
    let pusher = w.env.funded(SOL);
    let expect = owed(&w, &mint, &a.pubkey());
    assert!(expect > 0);
    let (al, pl, vault) = (
        w.env.lamports(&a.pubkey()),
        w.env.lamports(&pusher.pubkey()),
        kit_vault(&w, &mint),
    );
    let claimed_before = w
        .env
        .read::<KitConfig>(&kit::kit_config_address(&mint))
        .total_claimed;
    let tx = push(
        &mut w,
        &pusher,
        &[kit::claim_sol_for(pusher.pubkey(), a.pubkey(), mint)],
    );
    tx.ok();
    println!("claim_sol_for: {} CU, {} bytes", tx.cu(), tx.size);
    let ev = tx.event::<RewardsClaimed>();
    assert_eq!(ev.mint, mint);
    assert_eq!(ev.owner, a.pubkey());
    assert_eq!(ev.amount, expect);
    assert_eq!(ev.owed_left, 0);
    assert_eq!(ev.total_claimed, claimed_before + expect);
    assert_eq!(w.env.lamports(&a.pubkey()), al + expect);
    assert_eq!(kit_vault(&w, &mint), vault - expect);
    assert_eq!(w.env.lamports(&pusher.pubkey()), pl - FEE, "the fee only");
    let c: KitConfig = w.env.read(&kit::kit_config_address(&mint));
    assert_eq!(c.total_claimed, claimed_before + expect);
    // The other holder's bytes are untouched and it is still owed.
    assert!(owed(&w, &mint, &holders[1].pubkey()) > 0);
}

/// The pool and the launch earn nothing: a push for either is refused (`NotAHolder`).
#[test]
fn an_excluded_owner_is_refused() {
    let (mut w, mint, _, _, _) = native_world(1, rewards());
    let pusher = w.env.funded(SOL);
    let pool = w.launch_pool_key(&mint);
    let launch_key = launch::launch_address(&mint);
    for owner in [pool, launch_key] {
        assert!(w.env.holding(&mint, &owner) > 0);
        let tx = push(
            &mut w,
            &pusher,
            &[kit::claim_sol_for(pusher.pubkey(), owner, mint)],
        );
        assert!(
            refused_by_kit(&tx, KitError::NotAHolder),
            "{}",
            tx.logs().join("\n")
        );
    }
}

/// Nothing owed: a holding that never held, and a second push right after the first, revert
/// (`NothingToClaim`), the payer losing only the fee.
#[test]
fn nothing_owed_reverts() {
    let (mut w, mint, _, holders, _) = native_world(1, rewards());
    let pusher = w.env.funded(SOL);
    let empty = w.env.funded(SOL);
    w.holdings(&empty, mint, &[empty.pubkey()]);
    let tx = push(
        &mut w,
        &pusher,
        &[kit::claim_sol_for(pusher.pubkey(), empty.pubkey(), mint)],
    );
    assert!(refused_by_kit(&tx, KitError::NothingToClaim));
    let a = holders[0].pubkey();
    push(
        &mut w,
        &pusher,
        &[kit::claim_sol_for(pusher.pubkey(), a, mint)],
    )
    .ok();
    let (al, pl) = (w.env.lamports(&a), w.env.lamports(&pusher.pubkey()));
    let tx = push(
        &mut w,
        &pusher,
        &[kit::claim_sol_for(pusher.pubkey(), a, mint)],
    );
    assert!(refused_by_kit(&tx, KitError::NothingToClaim));
    assert_eq!(w.env.lamports(&a), al);
    assert_eq!(w.env.lamports(&pusher.pubkey()), pl - FEE);
}

/// A holder whose wallet would be left below the rent-exempt minimum (a wallet with no SOL, owed
/// less than rent) is refused (`RecipientBelowRent`); once it has rent the push pays it.
#[test]
fn a_recipient_below_rent_is_refused() {
    let (mut w, mint, payer, holders, _) = native_world(1, rewards());
    let pusher = w.env.funded(SOL);
    let a = &holders[0];
    // A wallet that never had SOL receives tokens (its holding paid by `a`).
    let x = Keypair::new().pubkey();
    w.holdings(a, mint, &[x]);
    let amount = w.env.holding(&mint, &a.pubkey()) / 2;
    w.send_tokens(a, mint, &x, amount).ok();
    w.env.warp(1);
    assert_eq!(w.env.lamports(&x), 0);
    // A small trade: its holder fee is far below rent.
    let t = w.env.funded(SOL);
    w.holdings(&t, mint, &[t.pubkey()]);
    buy(&mut w, &payer, &t, &mint, SOL / 50);
    let pay = owed(&w, &mint, &x);
    let rent = w.env.rent(0);
    assert!(pay > 0 && pay < rent, "owed {pay}, rent {rent}");
    let tx = push(
        &mut w,
        &pusher,
        &[kit::claim_sol_for(pusher.pubkey(), x, mint)],
    );
    assert!(
        refused_by_kit(&tx, KitError::RecipientBelowRent),
        "{}",
        tx.logs().join("\n")
    );
    assert_eq!(w.env.lamports(&x), 0);
    // With rent, the same push pays.
    w.env.fund(x, rent);
    let pay = owed(&w, &mint, &x);
    let tx = push(
        &mut w,
        &pusher,
        &[kit::claim_sol_for(pusher.pubkey(), x, mint)],
    );
    tx.ok();
    assert_eq!(tx.event::<RewardsClaimed>().amount, pay);
    assert_eq!(w.env.lamports(&x), rent + pay);
}

/// Pushes to eight different holders in one transaction (no lookup table, a compute budget
/// instruction in front): it fits the packet and every holder is paid what it was owed. Reports
/// the size and the compute units per push, and how many pushes fit.
#[test]
fn several_pushes_fit_one_transaction() {
    const N: usize = 8;
    let (mut w, mint, _, holders, _) = native_world(N, rewards());
    let pusher = w.env.funded(SOL);
    // A single push, for its compute units.
    let single = push(
        &mut w,
        &pusher,
        &[kit::claim_sol_for(
            pusher.pubkey(),
            holders[0].pubkey(),
            mint,
        )],
    );
    single.ok();
    let rest = &holders[1..];
    let fits: Vec<(usize, usize)> = (1..=12)
        .map(|n| {
            let ixs: Vec<Instruction> = (0..n)
                .map(|_| kit::claim_sol_for(pusher.pubkey(), Keypair::new().pubkey(), mint))
                .collect();
            let mut all = vec![bordrless_program_tests::env::compute_unit_limit(1_400_000)];
            all.extend(ixs);
            (n, w.env.v0_size(&all, &pusher, &[], &[]))
        })
        .collect();
    // A new trade, so that all eight are owed again.
    let t = w.env.funded(10 * SOL);
    w.holdings(&t, mint, &[t.pubkey()]);
    let payer = w.env.funded(SOL);
    buy(&mut w, &payer, &t, &mint, SOL);
    let mut owners: Vec<Pubkey> = vec![holders[0].pubkey()];
    owners.extend(rest.iter().map(Signer::pubkey));
    let expect: Vec<u64> = owners.iter().map(|o| owed(&w, &mint, o)).collect();
    let before: Vec<u64> = owners.iter().map(|o| w.env.lamports(o)).collect();
    assert!(expect.iter().all(|e| *e > 0));
    let ixs: Vec<Instruction> = owners
        .iter()
        .map(|o| kit::claim_sol_for(pusher.pubkey(), *o, mint))
        .collect();
    let pl = w.env.lamports(&pusher.pubkey());
    let tx = push(&mut w, &pusher, &ixs);
    tx.ok();
    assert!(tx.size <= PACKET, "{} bytes", tx.size);
    let events = tx.events::<RewardsClaimed>();
    assert_eq!(events.len(), N);
    for (i, o) in owners.iter().enumerate() {
        assert_eq!(events[i].owner, *o);
        assert_eq!(events[i].amount, expect[i]);
        assert_eq!(w.env.lamports(o), before[i] + expect[i]);
    }
    assert_eq!(w.env.lamports(&pusher.pubkey()), pl - FEE);
    println!(
        "one push: {} CU ({} bytes); {N} pushes: {} CU, {} per push, {} bytes",
        single.cu(),
        single.size,
        tx.cu(),
        tx.cu() / N as u64,
        tx.size
    );
    for (n, size) in &fits {
        println!(
            "{n:>2} pushes: {size} bytes{}",
            if *size <= PACKET { "" } else { " (too large)" }
        );
    }
    assert!(fits.iter().any(|(n, s)| *n == N && *s <= PACKET));
}

/// A push writes the holder's reward bytes as a claim does: its own `claim_sol` then finds
/// nothing; after new fees it pays only those; a push after that finds nothing again.
#[test]
fn the_push_updates_hook_data_and_claim_sol_still_works() {
    let (mut w, mint, payer, holders, _) = native_world(2, rewards());
    let pusher = w.env.funded(SOL);
    let a = &holders[0];
    let data0 = hook_data(&w, &mint, &a.pubkey());
    push(
        &mut w,
        &pusher,
        &[kit::claim_sol_for(pusher.pubkey(), a.pubkey(), mint)],
    )
    .ok();
    let data1 = hook_data(&w, &mint, &a.pubkey());
    assert_ne!(data0[..24], data1[..24], "the reward bytes are written");
    assert_eq!(data0[24..], data1[24..], "the rest as read");
    assert_eq!(owed(&w, &mint, &a.pubkey()), 0);
    let tx = send(&mut w, &payer, kit::claim_sol(a.pubkey(), mint), a);
    assert!(refused_by_kit(&tx, KitError::NothingToClaim));
    // New fees: the holder's own claim pays them, and only them.
    let t = w.env.funded(10 * SOL);
    w.holdings(&t, mint, &[t.pubkey()]);
    buy(&mut w, &payer, &t, &mint, SOL);
    let expect = owed(&w, &mint, &a.pubkey());
    assert!(expect > 0);
    let al = w.env.lamports(&a.pubkey());
    let tx = send(&mut w, &payer, kit::claim_sol(a.pubkey(), mint), a);
    tx.ok();
    let ev = tx.event::<RewardsClaimed>();
    assert_eq!((ev.owner, ev.amount, ev.owed_left), (a.pubkey(), expect, 0));
    assert_eq!(w.env.lamports(&a.pubkey()), al + expect);
    let tx = push(
        &mut w,
        &pusher,
        &[kit::claim_sol_for(pusher.pubkey(), a.pubkey(), mint)],
    );
    assert!(refused_by_kit(&tx, KitError::NothingToClaim));
    // The other holder, never pushed, claims as before.
    let b = &holders[1];
    let expect = owed(&w, &mint, &b.pubkey());
    let tx = send(&mut w, &payer, kit::claim_sol(b.pubkey(), mint), b);
    tx.ok();
    assert_eq!(tx.event::<RewardsClaimed>().amount, expect);
}

/// The SOL goes only to the holding's owner: another account named as the owner, a read-only
/// owner and an unsigned payer are refused.
#[test]
fn only_the_holdings_owner_is_paid() {
    let (mut w, mint, _, holders, _) = native_world(2, rewards());
    let pusher = w.env.funded(SOL);
    let (a, b) = (holders[0].pubkey(), holders[1].pubkey());
    // `b`'s wallet with `a`'s holding.
    let mut ix = kit::claim_sol_for(pusher.pubkey(), a, mint);
    ix.accounts[1] = AccountMeta::new(b, false);
    let tx = push(&mut w, &pusher, &[ix]);
    assert!(refused_by_kit(&tx, KitError::WrongHolding));
    // The pusher as the owner of `a`'s holding.
    let mut ix = kit::claim_sol_for(pusher.pubkey(), a, mint);
    ix.accounts[1] = AccountMeta::new(pusher.pubkey(), true);
    let tx = push(&mut w, &pusher, &[ix]);
    assert!(refused_by_kit(&tx, KitError::WrongHolding));
    // A read-only owner.
    let mut ix = kit::claim_sol_for(pusher.pubkey(), a, mint);
    ix.accounts[1] = AccountMeta::new_readonly(a, false);
    push(&mut w, &pusher, &[ix]).expect_code(2000); // ConstraintMut
                                                    // A payer that does not sign.
    let other = w.env.funded(SOL);
    let mut ix = kit::claim_sol_for(other.pubkey(), a, mint);
    ix.accounts[0] = AccountMeta::new_readonly(other.pubkey(), false);
    push(&mut w, &pusher, &[ix]).expect_code(3010); // AccountNotSigner
    assert!(owed(&w, &mint, &a) > 0, "nothing was paid");
}

/// A bridged kit refuses the push (`NotNativeKit`), as it refuses `claim_sol`; a native kit
/// without holder rewards refuses it too (`RewardsOff`).
#[test]
fn a_bridged_kit_and_a_kit_without_rewards_refuse() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = w.create_launch_with(&creator, "BRG", 100, VQ, rewards());
    tx.ok();
    assert!(!w
        .env
        .read::<KitConfig>(&kit::kit_config_address(&mint))
        .is_native());
    w.env.warp(31);
    let a = w.wallet_with_sol(10 * SOL);
    w.holdings(&a, mint, &[a.pubkey()]);
    w.buy(&a, &mint, SOL).ok();
    w.env.warp(1);
    let b = w.wallet_with_sol(10 * SOL);
    w.holdings(&b, mint, &[b.pubkey()]);
    w.buy(&b, &mint, SOL).ok();
    w.env.warp(1);
    let pusher = w.env.funded(SOL);
    let tx = push(
        &mut w,
        &pusher,
        &[kit::claim_sol_for(pusher.pubkey(), a.pubkey(), mint)],
    );
    assert!(refused_by_kit(&tx, KitError::NotNativeKit));
    let tx = w.env.send(&[kit::claim_sol(a.pubkey(), mint)], &[&a]);
    assert!(refused_by_kit(&tx, KitError::NotNativeKit));
    // Its own bridged claim still pays.
    w.env
        .send(&[kit::claim(a.pubkey(), mint, w.sol)], &[&a])
        .ok();

    // A native kit with no holder rewards (the creator wallet lock only).
    let rules = bordrless_program_tests::launch::rules(0, 0, 0, 0, 0, 30 * 86_400, 0, 0);
    let (mut w, mint, _, holders, _) = native_world(1, rules);
    let c: KitConfig = w.env.read(&kit::kit_config_address(&mint));
    assert!(!c.rewards_on());
    let pusher = w.env.funded(SOL);
    let tx = push(
        &mut w,
        &pusher,
        &[kit::claim_sol_for(
            pusher.pubkey(),
            holders[0].pubkey(),
            mint,
        )],
    );
    assert!(refused_by_kit(&tx, KitError::RewardsOff));
}
