//! The router's `swap_v2` (phase 3): an app's integrator fee on a native pool's swap, paid in SOL
//! to the integrator's wallet and, Bordrless's 20%, to the DEX treasury, every lamport accounted.

use bordrless_launch::client as launch;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::{SOL, VQ};
use bordrless_router::{client as router, fee_split, RouterError};
use bordrless_swap::events::Swapped;
use solana_keypair::Keypair;
use solana_signer::Signer;

#[test]
fn the_router_takes_its_fee_in_sol_on_a_native_pool() {
    let mut w = World::new();
    let a = w.admin().pubkey();
    w.as_admin(launch::set_native_launches(a, true)).ok();
    let creator = w.wallet_with_sol(5 * SOL);
    let (mint, tx) = w.create_launch(&creator, "RTR", 100, VQ);
    tx.ok();
    w.env.warp(31);
    let keys = w.launch_keys(&mint);
    let trader = w.env.funded(20 * SOL);
    let payer = w.env.funded(SOL);
    w.holdings(&payer, mint, &[trader.pubkey()]);
    let integrator = w.env.funded(SOL);
    let treasury = w.env.treasury.pubkey();
    let fee_bps = 100;

    // A buy: the fee from the trader's wallet first, the rest swapped.
    let amount = 2 * SOL;
    let inner = launch::swap_v2(&keys, trader.pubkey(), trader.pubkey(), 1, amount, 0);
    let ix = router::swap_v2(
        trader.pubkey(),
        integrator.pubkey(),
        treasury,
        &inner,
        fee_bps,
        0,
    );
    let (t0, i0, r0) = (
        w.env.lamports(&trader.pubkey()),
        w.env.lamports(&integrator.pubkey()),
        w.env.lamports(&treasury),
    );
    let tx = w.env.send_paid_by(&[ix], &payer, &[&trader]);
    tx.ok();
    let (fee, share) = fee_split(amount, fee_bps);
    let ev: Swapped = tx.event();
    assert_eq!(ev.amount_in, amount - fee);
    assert_eq!(t0 - w.env.lamports(&trader.pubkey()), amount);
    assert_eq!(w.env.lamports(&integrator.pubkey()) - i0, fee - share);
    assert_eq!(w.env.lamports(&treasury) - r0, share);
    w.assert_pool_invariant(&w.launch_pool_key(&mint));
    w.env.warp(1);

    // A sell: delivered to the trader's wallet, the fee taken from what arrived.
    let held = w.env.holding(&mint, &trader.pubkey());
    let inner = launch::swap_v2(&keys, trader.pubkey(), trader.pubkey(), 0, held / 2, 0);
    let ix = router::swap_v2(
        trader.pubkey(),
        integrator.pubkey(),
        treasury,
        &inner,
        fee_bps,
        0,
    );
    let (t0, i0, r0) = (
        w.env.lamports(&trader.pubkey()),
        w.env.lamports(&integrator.pubkey()),
        w.env.lamports(&treasury),
    );
    let tx = w.env.send_paid_by(&[ix], &payer, &[&trader]);
    tx.ok();
    let delivered = tx.event::<Swapped>().delivered_out;
    let (fee, share) = fee_split(delivered, fee_bps);
    assert_eq!(w.env.lamports(&trader.pubkey()) - t0, delivered - fee);
    assert_eq!(w.env.lamports(&integrator.pubkey()) - i0, fee - share);
    assert_eq!(w.env.lamports(&treasury) - r0, share);
    w.env.warp(1);

    // A sell delivered to anyone but the trader, or a fee to a wallet that can't take it, refused.
    let other = Keypair::new().pubkey();
    let inner = launch::swap_v2(&keys, trader.pubkey(), other, 0, held / 4, 0);
    let ix = router::swap_v2(
        trader.pubkey(),
        integrator.pubkey(),
        treasury,
        &inner,
        fee_bps,
        0,
    );
    w.env
        .send_paid_by(&[ix], &payer, &[&trader])
        .expect_code(u32::from(RouterError::NotTradersHolding));
    let fresh = Keypair::new().pubkey();
    let inner = launch::swap_v2(&keys, trader.pubkey(), trader.pubkey(), 1, SOL / 100, 0);
    let ix = router::swap_v2(trader.pubkey(), fresh, treasury, &inner, fee_bps, 0);
    w.env
        .send_paid_by(&[ix], &payer, &[&trader])
        .expect_code(u32::from(RouterError::RecipientBelowRent));
}
