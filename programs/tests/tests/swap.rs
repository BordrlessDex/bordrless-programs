//! The DEX: pools, swaps, liquidity, protocol fees, pausing.

use anchor_lang::prelude::Pubkey;
use bordrless_core::{fee_amount, initial_lp, swap_out, withdraw_for_lp};
use bordrless_program_tests::fixture::World;
use bordrless_swap::client as swap;
use bordrless_swap::error::SwapError;
use bordrless_swap::events::{LiquidityAdded, LiquidityRemoved, PoolCreated, Swapped};
use bordrless_swap::instructions::{
    AddLiquidityArgs, ConfigArgs, CreatePoolArgs, RemoveLiquidityArgs, SwapArgs,
};
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use solana_keypair::Keypair;
use solana_signer::Signer;

const A_SUPPLY: u64 = 1_000_000_000_000; // 1M tokens, 6 decimals
const B_SUPPLY: u64 = 1_000_000_000_000; // 1k tokens, 9 decimals
const A_DEPOSIT: u64 = 100_000_000_000;
const B_DEPOSIT: u64 = 100_000_000_000;

fn pool_args(base: u64, quote: u64) -> CreatePoolArgs {
    CreatePoolArgs {
        lp_fee_bps: 30,
        hook_program: Pubkey::default(),
        hook_flags: 0,
        virtual_base: 0,
        virtual_quote: 0,
        base_amount: base,
        quote_amount: quote,
        base_hook_accounts: 0,
        quote_hook_accounts: 0,
        hook_data: vec![],
    }
}

fn swap_ix(
    trader: &Keypair,
    pool: Pubkey,
    a: Pubkey,
    b: Pubkey,
    direction: u8,
    amount_in: u64,
    min_out: u64,
) -> anchor_lang::solana_program::instruction::Instruction {
    swap::swap(
        &swap::SwapKeys {
            trader: trader.pubkey(),
            pool,
            base_mint: a,
            quote_mint: b,
            trader_base: token::holding_address(&a, &trader.pubkey()),
            trader_quote: token::holding_address(&b, &trader.pubkey()),
            hook_program: None,
            base_mint_writable: false,
            quote_mint_writable: false,
        },
        SwapArgs {
            direction,
            amount_in,
            min_amount_out: min_out,
            in_hook_accounts: 0,
            out_hook_accounts: 0,
            hook_data: vec![],
        },
        vec![],
    )
}

#[test]
fn pool_lifecycle() {
    let mut w = World::new();
    let lp = w.env.funded(50_000_000_000);
    let a = w.mint_to_owner(&lp, 6, A_SUPPLY, "AAA");
    let b = w.mint_to_owner(&lp, 9, B_SUPPLY, "BBB");
    let treasury = w.env.treasury.pubkey();
    let pool = swap::pool_address(&a, &b, 30, None);
    let lp_mint = swap::lp_mint_address(&pool);

    // Create with the first deposit.
    let keys = swap::CreatePoolKeys {
        payer: lp.pubkey(),
        authority: lp.pubkey(),
        treasury,
        base_mint: a,
        quote_mint: b,
        hook_caller: None,
    };
    let tx = w.env.send_paid_by(
        &[swap::create_pool(
            &keys,
            pool_args(A_DEPOSIT, B_DEPOSIT),
            vec![],
        )],
        &lp,
        &[],
    );
    println!("create_pool CU {} size {}", tx.cu(), tx.size);
    tx.ok();
    let ev: PoolCreated = tx.event();
    let (to_lp, total) = initial_lp(A_DEPOSIT, B_DEPOSIT).unwrap();
    assert_eq!(
        (
            ev.base_reserve,
            ev.quote_reserve,
            ev.lp_supply,
            ev.lp_minted,
            ev.curve
        ),
        (A_DEPOSIT, B_DEPOSIT, total, to_lp, false)
    );
    let p: Pool = w.env.read(&pool);
    assert_eq!(
        (
            p.base_reserve,
            p.quote_reserve,
            p.lp_supply,
            p.protocol_fee_bps,
            p.lp_fee_bps
        ),
        (A_DEPOSIT, B_DEPOSIT, total, 100, 30)
    );
    assert_eq!(w.env.holding(&lp_mint, &lp.pubkey()), to_lp);
    assert_eq!(w.env.holding(&a, &pool), A_DEPOSIT);
    // The same pair, fee and hook cannot be created twice.
    w.env
        .send_paid_by(
            &[swap::create_pool(&keys, pool_args(1, 1), vec![])],
            &lp,
            &[],
        )
        .expect_fail();

    // A trader buys A with 1 B.
    let trader = w.env.funded(50_000_000_000);
    let b_in = 1_000_000_000;
    let ixs = [
        token::create_holding(trader.pubkey(), a, trader.pubkey()),
        token::create_holding(trader.pubkey(), b, trader.pubkey()),
        token::transfer(
            lp.pubkey(),
            token::holding_address(&b, &lp.pubkey()),
            token::holding_address(&b, &trader.pubkey()),
            b,
            None,
            vec![],
            10 * b_in,
        ),
    ];
    w.env.send_paid_by(&ixs, &trader, &[&lp]).ok();
    let lp_fee = fee_amount(b_in, 30).unwrap();
    let protocol_fee = fee_amount(b_in, 100).unwrap();
    let expected_out = swap_out(b_in - lp_fee - protocol_fee, B_DEPOSIT, 0, A_DEPOSIT, 0).unwrap();
    // Too demanding a minimum fails; nothing moves.
    w.env
        .send_paid_by(
            &[swap_ix(&trader, pool, a, b, 1, b_in, expected_out + 1)],
            &trader,
            &[],
        )
        .expect_code(u32::from(SwapError::Slippage));
    assert_eq!(w.env.holding(&a, &trader.pubkey()), 0);
    let tx = w.env.send_paid_by(
        &[swap_ix(&trader, pool, a, b, 1, b_in, expected_out)],
        &trader,
        &[],
    );
    println!("swap CU {} size {}", tx.cu(), tx.size);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!(
        (
            ev.direction,
            ev.amount_in,
            ev.received_in,
            ev.lp_fee,
            ev.protocol_fee,
            ev.amount_out,
            ev.delivered_out
        ),
        (
            1,
            b_in,
            b_in,
            lp_fee,
            protocol_fee,
            expected_out,
            expected_out
        )
    );
    assert_eq!(w.env.holding(&a, &trader.pubkey()), expected_out);
    let p: Pool = w.env.read(&pool);
    assert_eq!(
        (
            p.base_reserve,
            p.quote_reserve,
            p.protocol_fees_quote,
            p.swap_count
        ),
        (
            A_DEPOSIT - expected_out,
            B_DEPOSIT + b_in - protocol_fee,
            protocol_fee,
            1
        )
    );
    // The vault holds the reserve plus the protocol fee.
    assert_eq!(
        w.env.holding(&b, &pool),
        p.quote_reserve + p.protocol_fees_quote
    );
    // k never falls.
    assert!(
        u128::from(p.base_reserve) * u128::from(p.quote_reserve)
            >= u128::from(A_DEPOSIT) * u128::from(B_DEPOSIT)
    );

    // And sells half of it back: the LP fee stays in the pool in A; the protocol fee is taken in B
    // from the curve's output and set aside in the B vault.
    let p0: Pool = w.env.read(&pool);
    let a_in = expected_out / 2;
    let lp_fee = fee_amount(a_in, 30).unwrap();
    let out_gross = swap_out(a_in - lp_fee, p0.base_reserve, 0, p0.quote_reserve, 0).unwrap();
    let protocol_fee = fee_amount(out_gross, 100).unwrap();
    let delivered = out_gross - protocol_fee;
    let b_before = w.env.holding(&b, &trader.pubkey());
    // The minimum is checked against what arrives, after the protocol fee.
    w.env
        .send_paid_by(
            &[swap_ix(&trader, pool, a, b, 0, a_in, delivered + 1)],
            &trader,
            &[],
        )
        .expect_code(u32::from(SwapError::Slippage));
    let tx = w.env.send_paid_by(
        &[swap_ix(&trader, pool, a, b, 0, a_in, delivered)],
        &trader,
        &[],
    );
    println!("sell CU {} size {}", tx.cu(), tx.size);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!(
        (
            ev.direction,
            ev.amount_in,
            ev.received_in,
            ev.lp_fee,
            ev.protocol_fee,
            ev.amount_out,
            ev.delivered_out
        ),
        (0, a_in, a_in, lp_fee, protocol_fee, out_gross, delivered)
    );
    assert!(ev.deltas_in.is_empty() && ev.deltas_out.is_empty());
    assert_eq!((ev.burn_in, ev.burn_out), (0, 0));
    assert_eq!(w.env.holding(&b, &trader.pubkey()), b_before + delivered);
    let p: Pool = w.env.read(&pool);
    assert_eq!(
        (p.base_reserve, p.quote_reserve, p.protocol_fees_quote),
        (
            p0.base_reserve + a_in,
            p0.quote_reserve - out_gross,
            p0.protocol_fees_quote + protocol_fee
        )
    );
    assert_eq!(w.env.holding(&a, &pool), p.base_reserve);
    assert_eq!(
        w.env.holding(&b, &pool),
        p.quote_reserve + p.protocol_fees_quote
    );

    // The trader adds liquidity at the pool's ratio and takes it out again.
    let p0: Pool = w.env.read(&pool);
    let lkeys = swap::LiquidityKeys {
        provider: trader.pubkey(),
        pool,
        base_mint: a,
        quote_mint: b,
        hook_program: None,
    };
    let a_offer = w.env.holding(&a, &trader.pubkey());
    let b_offer = 5_000_000_000;
    let ixs = [
        token::create_holding(trader.pubkey(), lp_mint, trader.pubkey()),
        swap::add_liquidity(
            &lkeys,
            AddLiquidityArgs {
                base_desired: a_offer,
                quote_desired: b_offer,
                min_lp: 1,
                base_hook_accounts: 0,
                quote_hook_accounts: 0,
                hook_data: vec![],
            },
            vec![],
        ),
    ];
    let tx = w.env.send_paid_by(&ixs, &trader, &[]);
    println!("add_liquidity CU {} size {}", tx.cu(), tx.size);
    tx.ok();
    let ev: LiquidityAdded = tx.event();
    assert!(ev.lp_minted > 0);
    // One side was taken in full, the other at the ratio.
    assert!(ev.base_amount == a_offer || ev.quote_amount == b_offer);
    let shares = w.env.holding(&lp_mint, &trader.pubkey());
    assert_eq!(shares, ev.lp_minted);
    let p1: Pool = w.env.read(&pool);
    assert_eq!(p1.lp_supply, p0.lp_supply + ev.lp_minted);
    let (wb, wq) =
        withdraw_for_lp(shares, p1.base_reserve, p1.quote_reserve, p1.lp_supply).unwrap();
    let tx = w.env.send_paid_by(
        &[swap::remove_liquidity(
            &lkeys,
            RemoveLiquidityArgs {
                lp_amount: shares,
                min_base: wb,
                min_quote: wq,
                base_hook_accounts: 0,
                quote_hook_accounts: 0,
                hook_data: vec![],
            },
            vec![],
        )],
        &trader,
        &[],
    );
    println!("remove_liquidity CU {} size {}", tx.cu(), tx.size);
    tx.ok();
    let ev: LiquidityRemoved = tx.event();
    assert_eq!(
        (ev.base_amount, ev.quote_amount, ev.lp_burned),
        (wb, wq, shares)
    );
    assert_eq!(w.env.holding(&lp_mint, &trader.pubkey()), 0);
    // Rounding only ever favours the pool.
    let p2: Pool = w.env.read(&pool);
    assert!(p2.base_reserve >= p0.base_reserve && p2.quote_reserve >= p0.quote_reserve);

    // The admin collects the protocol fees, all in B, into the collector's B holding.
    let deployer = w.env.deployer.insecure_clone();
    let p: Pool = w.env.read(&pool);
    let ixs = [
        token::create_holding(deployer.pubkey(), b, deployer.pubkey()),
        swap::collect_protocol_fees(deployer.pubkey(), pool, b, deployer.pubkey(), vec![]),
    ];
    w.env.send_paid_by(&ixs, &deployer, &[]).ok();
    assert_eq!(w.env.holding(&b, &deployer.pubkey()), p.protocol_fees_quote);
    assert_eq!(w.env.holding(&a, &deployer.pubkey()), 0);
    let p: Pool = w.env.read(&pool);
    assert_eq!(p.protocol_fees_quote, 0);
    assert_eq!(w.env.holding(&a, &pool), p.base_reserve);
    assert_eq!(w.env.holding(&b, &pool), p.quote_reserve);
    // Not the admin.
    w.env
        .send_paid_by(
            &[swap::collect_protocol_fees(
                trader.pubkey(),
                pool,
                b,
                deployer.pubkey(),
                vec![],
            )],
            &trader,
            &[],
        )
        .expect_code(u32::from(SwapError::NotAdmin));

    // Paused: no swaps.
    let paused = ConfigArgs {
        admin: deployer.pubkey(),
        protocol_fee_bps: 100,
        launch_protocol_share_bps: 2_500,
        fee_collector: deployer.pubkey(),
        treasury,
        pool_creation_fee_lamports: 0,
        paused: true,
    };
    w.env
        .send_paid_by(
            &[swap::set_config(deployer.pubkey(), paused.clone())],
            &deployer,
            &[],
        )
        .ok();
    w.env
        .send_paid_by(
            &[swap_ix(&trader, pool, a, b, 1, 1_000_000, 0)],
            &trader,
            &[],
        )
        .expect_code(u32::from(SwapError::Paused));
    w.env
        .send_paid_by(
            &[swap::set_config(
                deployer.pubkey(),
                ConfigArgs {
                    paused: false,
                    ..paused
                },
            )],
            &deployer,
            &[],
        )
        .ok();
    w.env
        .send_paid_by(
            &[swap_ix(&trader, pool, a, b, 1, 1_000_000, 0)],
            &trader,
            &[],
        )
        .ok();
}

#[test]
fn curves_need_a_hook_that_creates_them() {
    let mut w = World::new();
    let lp = w.env.funded(50_000_000_000);
    let a = w.mint_to_owner(&lp, 6, A_SUPPLY, "AAA");
    let b = w.mint_to_owner(&lp, 9, B_SUPPLY, "BBB");
    let keys = swap::CreatePoolKeys {
        payer: lp.pubkey(),
        authority: lp.pubkey(),
        treasury: w.env.treasury.pubkey(),
        base_mint: a,
        quote_mint: b,
        hook_caller: None,
    };
    let mut args = pool_args(A_DEPOSIT, 0);
    args.virtual_quote = 1_000_000_000;
    args.hook_program = bordrless_launch::ID;
    args.hook_flags = bordrless_launch::constants::LAUNCH_HOOK_FLAGS;
    w.env
        .send_paid_by(&[swap::create_pool(&keys, args.clone(), vec![])], &lp, &[])
        .expect_code(u32::from(SwapError::CurveNeedsHook));
    // An ordinary pool with the launchpad as its hook is refused by the launchpad itself.
    args.virtual_quote = 0;
    args.quote_amount = B_DEPOSIT;
    w.env
        .send_paid_by(&[swap::create_pool(&keys, args, vec![])], &lp, &[])
        .expect_code(u32::from(
            bordrless_launch::error::LaunchError::OnlyLaunchpadCreatesPools,
        ));
}
