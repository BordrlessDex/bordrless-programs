//! The launchpad end to end: create, trade through the sniper window, graduate, trade on, claim.

use bordrless_core::{fee_amount, policy, protocol_share, sniper_lp_fee, spot_price_wad, swap_out};
use bordrless_launch::client as launch;
use bordrless_launch::constants::{STATUS_CURVE, STATUS_GRADUATED};
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::{CreatorFeesClaimed, Graduated, LaunchCreated};
use bordrless_program_tests::fixture::World;
use bordrless_swap::client as swap;
use bordrless_swap::constants::{FEE_MODEL_FLAT, FEE_MODEL_SHARE};
use bordrless_swap::events::{DeltaPaid, PoolCreated, Swapped};
use bordrless_swap::instructions::{AddLiquidityArgs, CreatePoolArgs, RemoveLiquidityArgs};
use bordrless_swap::state::Pool;
use bordrless_token::client as token;
use bordrless_token::state::Mint;
use solana_signer::Signer;

const SOL: u64 = 1_000_000_000;
const VQ: u64 = 28_125_000_000;

#[test]
fn launch_trades_and_graduates() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(20 * SOL);
    let treasury_before = w.env.lamports(&w.env.treasury.pubkey());
    let (mint, tx) = w.create_launch(&creator, "LNCH", 100, VQ);
    println!("create_launch CU {} size {}", tx.cu(), tx.size);
    tx.ok();
    assert!(
        tx.size <= 1_232,
        "the launch transaction must fit: {} bytes",
        tx.size
    );
    let ev: LaunchCreated = tx.event();
    assert_eq!(
        (
            ev.curve_tokens,
            ev.reserve_tokens,
            ev.virtual_base,
            ev.graduation_quote
        ),
        (
            750_000_000_000_000,
            250_000_000_000_000,
            375_000_000_000_000,
            56_250_000_000
        )
    );
    assert_eq!(
        w.env.lamports(&w.env.treasury.pubkey()),
        treasury_before + policy::LAUNCH_FEE_LAMPORTS
    );
    let l = w.launch(&mint);
    assert_eq!(
        (l.status, l.creator, l.creator_fee_bps),
        (STATUS_CURVE, creator.pubkey(), 100)
    );
    let p = w.launch_pool(&mint);
    assert!(p.curve);
    assert_eq!(
        (
            p.base_reserve,
            p.quote_reserve,
            p.virtual_base,
            p.virtual_quote,
            p.lp_supply
        ),
        (750_000_000_000_000, 0, 375_000_000_000_000, VQ, 0)
    );
    let m: Mint = w.env.read(&mint);
    assert_eq!(
        (
            m.supply,
            m.mint_authority,
            m.metadata_authority,
            m.hook_program
        ),
        (policy::TOKEN_SUPPLY, None, None, None)
    );
    assert_eq!(
        w.env.holding(&mint, &launch::launch_address(&mint)),
        250_000_000_000_000
    );
    // The opening market cap is what the policy promised: 25 SOL.
    let price = spot_price_wad(
        p.base_reserve,
        p.virtual_base,
        p.quote_reserve,
        p.virtual_quote,
    )
    .unwrap();
    let mcap = price * u128::from(policy::TOKEN_SUPPLY) / bordrless_core::WAD;
    assert!(
        (mcap as i128 - 25 * SOL as i128).abs() < 1_000,
        "mcap {mcap}"
    );

    // The creator's first buy, in the launch's own moment, pays the normal LP fee and the creator
    // fee, sent from the input to the launch's quote holding.
    let launch_quote = token::holding_address(&w.sol, &launch::launch_address(&mint));
    let tx = w.buy(&creator, &mint, SOL);
    println!("first buy CU {} size {}", tx.cu(), tx.size);
    tx.ok();
    let ev: Swapped = tx.event();
    let creator_fee = fee_amount(SOL, 100).unwrap();
    assert_eq!(ev.lp_fee_bps, policy::LP_FEE_BPS);
    assert_eq!(
        ev.deltas_in,
        vec![DeltaPaid {
            holding: launch_quote,
            amount: creator_fee
        }]
    );
    assert_eq!((ev.burn_in, ev.burn_out), (0, 0));
    assert!(ev.deltas_out.is_empty());
    assert_eq!(ev.received_in, SOL - creator_fee);
    // Bordrless takes a quarter of what the launch's rules collected (of a 1% creator fee on 1
    // SOL, 0.25% of the trade) and the LP fee on what reached the vault, in SOL: nothing
    // compounds in the pool, whose LP is locked for ever.
    assert_eq!(ev.cuts_in, creator_fee);
    let share = protocol_share(creator_fee, policy::LAUNCH_PROTOCOL_SHARE_BPS).unwrap();
    assert_eq!(share, SOL / 400);
    let lp_fee = fee_amount(SOL - creator_fee, policy::LP_FEE_BPS).unwrap();
    assert_eq!((ev.lp_fee, ev.protocol_fee), (0, share + lp_fee));
    assert_eq!(w.launch(&mint).creator_fees_accrued, creator_fee);
    assert_eq!(
        w.env.holding(&w.sol, &launch::launch_address(&mint)),
        creator_fee
    );
    let creator_tokens = w.env.holding(&mint, &creator.pubkey());
    assert!(creator_tokens > 0);

    // A sniper five seconds in pays the elevated LP fee, which goes to Bordrless in SOL.
    w.env.warp(5);
    let sniper = w.wallet_with_sol(10 * SOL);
    let before = w.launch_pool(&mint);
    let tx = w.buy(&sniper, &mint, SOL);
    tx.ok();
    let ev: Swapped = tx.event();
    let expected = sniper_lp_fee(
        w.env.now,
        w.launch(&mint).created_at,
        policy::SNIPER_WINDOW_SECS,
        policy::SNIPER_START_BPS,
        policy::LP_FEE_BPS,
    );
    assert_eq!(ev.lp_fee_bps, expected);
    assert_eq!(expected, 6_672);
    let after = w.launch_pool(&mint);
    assert_eq!(
        after.quote_reserve - before.quote_reserve,
        ev.received_in - ev.protocol_fee
    );
    // Snipers get far less than a patient buyer would.
    assert!(w.env.holding(&mint, &sniper.pubkey()) < creator_tokens / 2);

    // After the window: the normal fee. A sell pays the creator fee from its output.
    w.env.warp(policy::SNIPER_WINDOW_SECS);
    let buyer = w.wallet_with_sol(200 * SOL);
    let tx = w.buy(&buyer, &mint, SOL);
    tx.ok();
    assert_eq!(tx.event::<Swapped>().lp_fee_bps, policy::LP_FEE_BPS);
    // The sell: the tokens go into the curve whole; the LP fee (Bordrless's) leaves its output in
    // SOL; the hook takes the creator fee from the rest; Bordrless's share of it is held back
    // from the delivery.
    let held = w.env.holding(&mint, &buyer.pubkey());
    let before = w.launch_pool(&mint);
    let accrued = w.launch(&mint).creator_fees_accrued;
    let tokens = held / 2;
    let out_gross = swap_out(
        tokens,
        before.base_reserve,
        before.virtual_base,
        before.quote_reserve,
        before.virtual_quote,
    )
    .unwrap();
    let lp_fee = fee_amount(out_gross, policy::LP_FEE_BPS).unwrap();
    let told = out_gross - lp_fee;
    let creator_fee = fee_amount(told, 100).unwrap();
    let share = protocol_share(creator_fee, policy::LAUNCH_PROTOCOL_SHARE_BPS).unwrap();
    let protocol_fee = lp_fee + share;
    let sol_before = w.env.holding(&w.sol, &buyer.pubkey());
    let tx = w.sell(&buyer, &mint, tokens);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!(
        (
            ev.direction,
            ev.received_in,
            ev.lp_fee,
            ev.amount_out,
            ev.protocol_fee,
            ev.delivered_out
        ),
        (
            0,
            tokens,
            0,
            out_gross,
            protocol_fee,
            out_gross - protocol_fee - creator_fee
        )
    );
    assert_eq!(
        ev.deltas_out,
        vec![DeltaPaid {
            holding: launch_quote,
            amount: creator_fee
        }]
    );
    assert!(ev.deltas_in.is_empty());
    assert_eq!((ev.cuts_in, ev.cuts_out), (0, creator_fee));
    assert_eq!(
        w.env.holding(&w.sol, &buyer.pubkey()),
        sol_before + ev.delivered_out
    );
    assert_eq!(w.launch(&mint).creator_fees_accrued, accrued + creator_fee);
    let after = w.launch_pool(&mint);
    assert_eq!(
        (
            after.base_reserve,
            after.quote_reserve,
            after.protocol_fees_quote
        ),
        (
            before.base_reserve + tokens,
            before.quote_reserve - out_gross,
            before.protocol_fees_quote + protocol_fee
        )
    );
    assert_eq!(w.env.holding(&mint, &l_pool(&w, &mint)), after.base_reserve);
    assert_eq!(
        w.env.holding(&w.sol, &l_pool(&w, &mint)),
        after.quote_reserve + after.protocol_fees_quote
    );

    // Not ready: the threshold is not reached.
    let cranker = w.env.funded(SOL);
    w.env
        .send_paid_by(&[w.graduate_ix(&cranker.pubkey(), &mint)], &cranker, &[])
        .expect_code(u32::from(LaunchError::NotReady));

    // Buy until the pool has raised the threshold.
    let mut rounds = 0;
    // Ready once the reserve meets the threshold or the curve sells out (the LP fee does not
    // compound, so the two come together); no buy is larger than what the curve can still fill.
    while {
        let p = w.launch_pool(&mint);
        p.quote_reserve < w.launch(&mint).graduation_quote && p.base_reserve > 0
    } {
        let p = w.launch_pool(&mint);
        let remaining = w
            .launch(&mint)
            .graduation_quote
            .saturating_sub(p.quote_reserve);
        // A little more than what is missing (fees are taken off the way in), never a dust trade.
        let step = (remaining + remaining / 50)
            .clamp(SOL / 20, 10 * SOL)
            .min(w.crossing_buy_amount(&mint, &buyer.pubkey()));
        w.buy(&buyer, &mint, step).ok();
        rounds += 1;
        assert!(rounds < 50);
    }
    let p = w.launch_pool(&mint);
    assert!(p.curve && p.base_reserve > 0);
    let price_before = spot_price_wad(
        p.base_reserve,
        p.virtual_base,
        p.quote_reserve,
        p.virtual_quote,
    )
    .unwrap();

    // Anyone graduates it; the price does not move; the reserve is burned beyond the top-up.
    let supply_before = w.env.read::<Mint>(&mint).supply;
    let tx = w
        .env
        .send_paid_by(&[w.graduate_ix(&cranker.pubkey(), &mint)], &cranker, &[]);
    println!("graduate CU {} size {}", tx.cu(), tx.size);
    tx.ok();
    let ev: Graduated = tx.event();
    let p = w.launch_pool(&mint);
    assert!(!p.curve);
    assert_eq!((p.virtual_base, p.virtual_quote), (0, 0));
    assert_eq!(
        (ev.base_reserve, ev.quote_reserve),
        (p.base_reserve, p.quote_reserve)
    );
    // Protocol fees are only in SOL: the whole base vault is reserve, the SOL vault is the reserve
    // plus the fees not yet collected.
    assert_eq!(w.env.holding(&mint, &l_pool(&w, &mint)), p.base_reserve);
    assert_eq!(
        w.env.holding(&w.sol, &l_pool(&w, &mint)),
        p.quote_reserve + p.protocol_fees_quote
    );
    assert_eq!(ev.topup + ev.burned, 250_000_000_000_000);
    assert_eq!(w.env.read::<Mint>(&mint).supply, supply_before - ev.burned);
    let price_after = spot_price_wad(p.base_reserve, 0, p.quote_reserve, 0).unwrap();
    assert!(
        price_before.abs_diff(price_after) * 1_000_000 <= price_before,
        "price {price_before} -> {price_after}"
    );
    let l = w.launch(&mint);
    assert_eq!(
        (l.status, l.graduation_topup, l.graduation_burned),
        (STATUS_GRADUATED, ev.topup, ev.burned)
    );
    assert_eq!(w.env.holding(&mint, &launch::launch_address(&mint)), 0);
    let lp_mint = swap::lp_mint_address(&l.pool);
    assert_eq!(
        w.env.holding(&lp_mint, &launch::launch_address(&mint)),
        p.lp_supply - policy::MINIMUM_LIQUIDITY
    );
    assert_eq!(ev.lp_minted, p.lp_supply - policy::MINIMUM_LIQUIDITY);
    w.env
        .send_paid_by(&[w.graduate_ix(&cranker.pubkey(), &mint)], &cranker, &[])
        .expect_code(u32::from(LaunchError::AlreadyGraduated));

    // Trading goes on, creator fees included.
    let tx = w.buy(&buyer, &mint, SOL);
    tx.ok();
    assert_eq!(
        tx.event::<Swapped>().deltas_in,
        vec![DeltaPaid {
            holding: launch_quote,
            amount: fee_amount(SOL, 100).unwrap()
        }]
    );
    let held = w.env.holding(&mint, &buyer.pubkey());
    w.sell(&buyer, &mint, held / 4).ok();

    // The creator claims the fees.
    let accrued = w.launch(&mint).creator_fees_accrued;
    assert_eq!(
        w.env.holding(&w.sol, &launch::launch_address(&mint)),
        accrued
    );
    let before = w.env.holding(&w.sol, &creator.pubkey());
    let tx = w.env.send_paid_by(
        &[launch::claim_creator_fees(creator.pubkey(), mint, w.sol)],
        &creator,
        &[],
    );
    tx.ok();
    assert_eq!(tx.event::<CreatorFeesClaimed>().amount, accrued);
    assert_eq!(w.env.holding(&w.sol, &creator.pubkey()), before + accrued);
    assert_eq!(w.env.holding(&w.sol, &launch::launch_address(&mint)), 0);
    w.env
        .send_paid_by(
            &[launch::claim_creator_fees(buyer.pubkey(), mint, w.sol)],
            &buyer,
            &[],
        )
        .expect_fail();

    // Others may add and remove liquidity now; the launch's own LP never moves.
    let lkeys = swap::LiquidityKeys {
        provider: buyer.pubkey(),
        pool: l.pool,
        base_mint: mint,
        quote_mint: w.sol,
        hook_program: Some(bordrless_launch::ID),
    };
    let ixs = [
        token::create_holding(buyer.pubkey(), lp_mint, buyer.pubkey()),
        swap::add_liquidity(
            &lkeys,
            AddLiquidityArgs {
                base_desired: w.env.holding(&mint, &buyer.pubkey()) / 2,
                quote_desired: 5 * SOL,
                min_lp: 1,
                base_hook_accounts: 0,
                quote_hook_accounts: 0,
                hook_data: vec![],
            },
            launch::hook_extras(&mint, &w.sol),
        ),
    ];
    w.env.send_paid_by(&ixs, &buyer, &[]).ok();
    let shares = w.env.holding(&lp_mint, &buyer.pubkey());
    assert!(shares > 0);
    w.env
        .send_paid_by(
            &[swap::remove_liquidity(
                &lkeys,
                RemoveLiquidityArgs {
                    lp_amount: shares,
                    min_base: 0,
                    min_quote: 0,
                    base_hook_accounts: 0,
                    quote_hook_accounts: 0,
                    hook_data: vec![],
                },
                launch::hook_extras(&mint, &w.sol),
            )],
            &buyer,
            &[],
        )
        .ok();
    assert_eq!(w.env.holding(&lp_mint, &buyer.pubkey()), 0);
    assert_eq!(
        w.env.holding(&lp_mint, &launch::launch_address(&mint)),
        ev.lp_minted
    );
}

/// The pool of a launch.
fn l_pool(w: &World, mint: &anchor_lang::prelude::Pubkey) -> anchor_lang::prelude::Pubkey {
    launch::pool_address(mint, &w.sol, policy::LP_FEE_BPS)
}

#[test]
fn launch_terms_are_bounded() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(5 * SOL);
    let (_, tx) = w.create_launch(&creator, "FEE", policy::MAX_CREATOR_FEE_BPS + 1, VQ);
    tx.expect_code(u32::from(LaunchError::CreatorFeeTooHigh));
    let (_, tx) = w.create_launch(&creator, "LOW", 0, policy::MIN_VIRTUAL_QUOTE - 1);
    tx.expect_code(u32::from(LaunchError::VirtualQuoteOutOfBounds));
    let (_, tx) = w.create_launch(&creator, "OK", 0, policy::MIN_VIRTUAL_QUOTE);
    tx.ok();
}

/// The DEX config carries two protocol fee models: a pool the launchpad creates (a curve, by its
/// hook authority) takes Bordrless's share of what its hooks cut and keeps that model through
/// graduation; a pool anyone creates directly pays the ordinary flat rate.
#[test]
fn launch_pools_share_their_cuts_and_ordinary_pools_pay_the_dex_rate() {
    let mut w = World::new();
    let config: bordrless_swap::state::Config = w.env.read(&swap::config_address());
    assert_eq!(
        (config.protocol_fee_bps, config.launch_protocol_share_bps),
        (policy::PROTOCOL_FEE_BPS, policy::LAUNCH_PROTOCOL_SHARE_BPS)
    );

    // The launchpad's pool: a quarter of the cuts, on its curve and after graduation.
    let creator = w.wallet_with_sol(20 * SOL);
    let (mint, tx) = w.create_launch(&creator, "LPFE", 100, VQ);
    tx.ok();
    let created: PoolCreated = tx.event();
    assert_eq!(
        (
            created.fee_model,
            created.protocol_fee_bps,
            created.protocol_share_bps
        ),
        (FEE_MODEL_SHARE, 0, policy::LAUNCH_PROTOCOL_SHARE_BPS)
    );
    let p = w.launch_pool(&mint);
    assert!(p.curve && p.shares_cuts());
    assert_eq!(
        (p.fee_model, p.protocol_fee_bps, p.protocol_share_bps),
        (FEE_MODEL_SHARE, 0, policy::LAUNCH_PROTOCOL_SHARE_BPS)
    );
    let before = w.launch_pool(&mint);
    let tx = w.buy(&creator, &mint, SOL);
    tx.ok();
    let ev: Swapped = tx.event();
    let creator_fee = fee_amount(SOL, 100).unwrap();
    let lp_fee = fee_amount(SOL - creator_fee, policy::LP_FEE_BPS).unwrap();
    assert_eq!(
        ev.protocol_fee,
        protocol_share(creator_fee, policy::LAUNCH_PROTOCOL_SHARE_BPS).unwrap() + lp_fee
    );
    assert_eq!(
        w.launch_pool(&mint).protocol_fees_quote,
        before.protocol_fees_quote + ev.protocol_fee
    );
    w.env.warp(policy::SNIPER_WINDOW_SECS);
    let buyer = w.wallet_with_sol(200 * SOL);
    let mut rounds = 0;
    // Ready once the reserve meets the threshold or the curve sells out (the LP fee does not
    // compound, so the two come together); no buy is larger than what the curve can still fill.
    while {
        let p = w.launch_pool(&mint);
        p.quote_reserve < w.launch(&mint).graduation_quote && p.base_reserve > 0
    } {
        let p = w.launch_pool(&mint);
        let remaining = w
            .launch(&mint)
            .graduation_quote
            .saturating_sub(p.quote_reserve);
        let step = (remaining + remaining / 50)
            .clamp(SOL / 20, 10 * SOL)
            .min(w.crossing_buy_amount(&mint, &buyer.pubkey()));
        w.buy(&buyer, &mint, step).ok();
        rounds += 1;
        assert!(rounds < 50);
    }
    let cranker = w.env.funded(SOL);
    w.env
        .send_paid_by(&[w.graduate_ix(&cranker.pubkey(), &mint)], &cranker, &[])
        .ok();
    let p = w.launch_pool(&mint);
    assert!(!p.curve && p.shares_cuts());
    assert_eq!(
        (p.protocol_fee_bps, p.protocol_share_bps),
        (0, policy::LAUNCH_PROTOCOL_SHARE_BPS)
    );
    let before = p;
    let tx = w.buy(&buyer, &mint, SOL);
    tx.ok();
    let ev: Swapped = tx.event();
    assert_eq!(
        ev.protocol_fee,
        protocol_share(creator_fee, policy::LAUNCH_PROTOCOL_SHARE_BPS).unwrap() + lp_fee
    );
    assert_eq!(
        w.launch_pool(&mint).protocol_fees_quote,
        before.protocol_fees_quote + ev.protocol_fee
    );

    // A pool anyone creates directly: the ordinary 1%.
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
        hook_program: anchor_lang::prelude::Pubkey::default(),
        hook_flags: 0,
        virtual_base: 0,
        virtual_quote: 0,
        base_amount: 100_000_000_000,
        quote_amount: 100_000_000_000,
        base_hook_accounts: 0,
        quote_hook_accounts: 0,
        hook_data: vec![],
    };
    let tx = w
        .env
        .send_paid_by(&[swap::create_pool(&keys, args, vec![])], &lp, &[]);
    tx.ok();
    let created: PoolCreated = tx.event();
    assert_eq!(
        (
            created.fee_model,
            created.protocol_fee_bps,
            created.protocol_share_bps
        ),
        (FEE_MODEL_FLAT, policy::PROTOCOL_FEE_BPS, 0)
    );
    let p: Pool = w.env.read(&pool);
    assert!(!p.shares_cuts());
    assert_eq!(
        (p.fee_model, p.protocol_fee_bps, p.protocol_share_bps),
        (FEE_MODEL_FLAT, policy::PROTOCOL_FEE_BPS, 0)
    );
}
