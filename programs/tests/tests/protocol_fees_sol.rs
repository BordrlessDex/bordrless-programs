//! `collect_protocol_fees_sol`: a launch pool's protocol fees (the LP fee and the share of the
//! rules' cuts, all Bordrless's, kept as bridged SOL in the pool's quote vault) are unwrapped by the
//! bridge and paid to the fee collector as SOL. Anyone may send it; only the configured collector
//! can receive; the pool's books and its own lamports stay as they were; a pool quoted in anything
//! else is refused.

use anchor_lang::prelude::Pubkey;
use bordrless_launch::client as launch;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_swap::client as swap;
use bordrless_swap::error::SwapError;
use bordrless_swap::events::ProtocolFeesCollected;
use bordrless_swap::instructions::CreatePoolArgs;
use bordrless_token::state::Mint;
use solana_signer::Signer;

#[test]
fn anyone_pays_a_launch_pools_fees_to_the_collector_in_sol() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = w.create_launch(&creator, "PFS", 100, VQ);
    tx.ok();
    w.env.warp(31);
    let trader = w.wallet_with_sol(10 * SOL);
    w.holdings(&trader, mint, &[trader.pubkey()]);
    w.buy(&trader, &mint, SOL).ok();
    let held = w.env.holding(&mint, &trader.pubkey());
    w.sell(&trader, &mint, held / 2).ok();

    let pool = launch::pool_address(&mint, &w.sol, bordrless_core::policy::LP_FEE_BPS);
    let before = w.launch_pool(&mint);
    let fees = before.protocol_fees_quote;
    assert!(fees > 0);
    let collector = w.env.deployer.pubkey();
    let collector_before = w.env.account(&collector).map_or(0, |a| a.lamports);
    let pool_lamports = w.env.account(&pool).unwrap().lamports;
    let supply = w.env.read::<Mint>(&w.sol).supply;
    let vault = w.env.holding(&w.sol, &pool);
    assert_eq!(vault, before.quote_reserve + fees);

    // A stranger sends it; the collector the config names receives the SOL.
    let stranger = w.env.funded(bordrless_program_tests::launch::SOL);
    let tx = w.env.send_paid_by(
        &[swap::collect_protocol_fees_sol(
            stranger.pubkey(),
            pool,
            collector,
        )],
        &stranger,
        &[],
    );
    tx.ok();
    let ev: ProtocolFeesCollected = tx.event();
    assert_eq!(
        (ev.pool, ev.quote_amount, ev.collector),
        (pool, fees, collector)
    );
    assert_eq!(
        w.env.account(&collector).unwrap().lamports,
        collector_before + fees
    );
    let after = w.launch_pool(&mint);
    assert_eq!(after.protocol_fees_quote, 0);
    assert_eq!(after.quote_reserve, before.quote_reserve);
    assert_eq!(w.env.holding(&w.sol, &pool), before.quote_reserve);
    assert_eq!(w.env.read::<Mint>(&w.sol).supply, supply - fees);
    assert_eq!(w.env.account(&pool).unwrap().lamports, pool_lamports);

    // Again: nothing accrued, nothing paid.
    w.env.warp(1);
    let tx = w.env.send_paid_by(
        &[swap::collect_protocol_fees_sol(
            stranger.pubkey(),
            pool,
            collector,
        )],
        &stranger,
        &[],
    );
    tx.ok();
    assert_eq!(tx.event::<ProtocolFeesCollected>().quote_amount, 0);
    assert_eq!(
        w.env.account(&collector).unwrap().lamports,
        collector_before + fees
    );

    // Trades accrue again; nobody but the configured collector can receive them.
    w.buy(&trader, &mint, SOL).ok();
    let tx = w.env.send_paid_by(
        &[swap::collect_protocol_fees_sol(
            stranger.pubkey(),
            pool,
            stranger.pubkey(),
        )],
        &stranger,
        &[],
    );
    tx.expect_code(u32::from(SwapError::WrongHolding));
}

#[test]
fn a_pool_quoted_in_anything_but_bridged_sol_is_refused() {
    let mut w = World::new();
    let lp = w.env.funded(50_000_000_000);
    let a = w.mint_to_owner(&lp, 6, 1_000_000_000_000, "AAA");
    let b = w.mint_to_owner(&lp, 9, 1_000_000_000_000, "BBB");
    let pool = swap::pool_address(&a, &b, 30, None);
    let keys = swap::CreatePoolKeys {
        payer: lp.pubkey(),
        authority: lp.pubkey(),
        treasury: w.env.treasury.pubkey(),
        base_mint: a,
        quote_mint: b,
        hook_caller: None,
    };
    let args = CreatePoolArgs {
        lp_fee_bps: 30,
        hook_program: Pubkey::default(),
        hook_flags: 0,
        virtual_base: 0,
        virtual_quote: 0,
        base_amount: 100_000_000_000,
        quote_amount: 100_000_000_000,
        base_hook_accounts: 0,
        quote_hook_accounts: 0,
        hook_data: vec![],
    };
    w.env
        .send_paid_by(&[swap::create_pool(&keys, args, vec![])], &lp, &[])
        .ok();
    let tx = w.env.send_paid_by(
        &[swap::collect_protocol_fees_sol(
            lp.pubkey(),
            pool,
            w.env.deployer.pubkey(),
        )],
        &lp,
        &[],
    );
    tx.expect_code(u32::from(SwapError::NotBridgedSol));
}
