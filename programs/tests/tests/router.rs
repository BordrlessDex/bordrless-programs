//! The router (`docs/router.md`): an app's integrator fee on any DEX swap. A buy pays the fee from
//! its quote input before the DEX swaps the rest; a sell pays it from what the DEX delivered; 20% of
//! every fee to the DEX treasury's holding, the rest to the integrator; the DEX swap otherwise
//! exactly as the app built it, on a launch pool (plain, kit, a custom hook) and an ordinary pool;
//! every refusal.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_router::{client as router, fee_split, IntegratorFeePaid, RouterError};
use bordrless_swap::client as swap;
use bordrless_swap::events::Swapped;
use bordrless_swap::instructions::{CreatePoolArgs, SwapArgs};
use bordrless_token::client as token;
use solana_keypair::Keypair;
use solana_signer::Signer;

#[track_caller]
fn refused(tx: &Tx, e: RouterError) {
    tx.expect_code(u32::from(e));
    assert!(
        tx.logs()
            .iter()
            .any(|l| l.contains(&format!("Error Code: {e:?}"))),
        "expected {e:?}\n{}",
        tx.logs().join("\n")
    );
}

/// A launch by `creator` with `rules` and a 1% creator fee.
fn launch_with_rules(w: &mut World, creator: &Keypair, symbol: &str, rules: LaunchRules) -> Pubkey {
    let (mint, tx) = w.create_launch_with(creator, symbol, 100, VQ, rules);
    tx.ok();
    mint
}

struct Ctx {
    integrator: Keypair,
    integrator_quote: Pubkey,
    treasury_quote: Pubkey,
}

/// The integrator's and the treasury's holdings of bridged SOL.
fn ctx(w: &mut World) -> Ctx {
    let integrator = w.wallet_with_sol(SOL);
    let treasury = w.env.treasury.pubkey();
    let payer = w.env.funded(SOL);
    w.env
        .send_paid_by(
            &[token::create_holding(payer.pubkey(), w.sol, treasury)],
            &payer,
            &[],
        )
        .ok();
    Ctx {
        integrator_quote: token::holding_address(&w.sol, &integrator.pubkey()),
        treasury_quote: token::holding_address(&w.sol, &treasury),
        integrator,
    }
}

fn routed(c: &Ctx, trader: &Pubkey, dex: &Instruction, fee_bps: u16, floor: u64) -> Instruction {
    router::swap(
        *trader,
        c.integrator_quote,
        c.treasury_quote,
        dex,
        fee_bps,
        floor,
    )
}

fn trader(w: &mut World, mint: &Pubkey, sol: u64) -> Keypair {
    let t = w.wallet_with_sol(sol);
    w.holdings(&t, *mint, &[t.pubkey()]);
    t
}

/// A routed buy and sell on the launch of `mint`: the fee and the share land, the DEX swapped the
/// rest; answers the CU of each.
fn round_trip(w: &mut World, c: &Ctx, mint: &Pubkey, fee_bps: u16) -> (u64, u64) {
    let t = trader(w, mint, 5 * SOL);
    let me = t.pubkey();
    let sol = w.sol;
    let (i0, r0, t0) = (
        w.env.holding(&sol, &c.integrator.pubkey()),
        w.env.holding(&sol, &w.env.treasury.pubkey()),
        w.env.holding(&sol, &me),
    );
    // A buy of 1 SOL: the fee from it, the DEX swaps the rest.
    let dex = w.launch_swap_ix(&me, mint, 1, SOL, 0);
    let tx = w
        .env
        .send_paid_by(&[routed(c, &me, &dex, fee_bps, 0)], &t, &[]);
    tx.ok();
    let (fee, share) = fee_split(SOL, fee_bps);
    assert_eq!(tx.event::<Swapped>().amount_in, SOL - fee);
    let ev = tx.event::<IntegratorFeePaid>();
    assert_eq!(
        (
            ev.direction,
            ev.charged_on,
            ev.fee,
            ev.bordrless_share,
            ev.integrator
        ),
        (1, SOL, fee, share, c.integrator.pubkey())
    );
    assert_eq!(
        w.env.holding(&sol, &c.integrator.pubkey()) - i0,
        fee - share
    );
    assert_eq!(w.env.holding(&sol, &w.env.treasury.pubkey()) - r0, share);
    assert_eq!(t0 - w.env.holding(&sol, &me), SOL);
    let buy_cu = tx.cu();
    // A sell of half: the fee from what the DEX delivered; the trader keeps the rest.
    let held = w.env.holding(mint, &me);
    let before = w.env.holding(&sol, &me);
    let dex = w.launch_swap_ix(&me, mint, 0, held / 2, 0);
    let tx = w
        .env
        .send_paid_by(&[routed(c, &me, &dex, fee_bps, 0)], &t, &[]);
    tx.ok();
    let delivered = tx.event::<Swapped>().delivered_out;
    let (fee, share) = fee_split(delivered, fee_bps);
    let ev = tx.event::<IntegratorFeePaid>();
    assert_eq!((ev.direction, ev.charged_on, ev.fee), (0, delivered, fee));
    assert_eq!(w.env.holding(&sol, &me) - before, delivered - fee);
    assert_eq!(
        w.env.holding(&sol, &w.env.treasury.pubkey()) - r0,
        fee_split(SOL, fee_bps).1 + share
    );
    (buy_cu, tx.cu())
}

#[test]
fn a_routed_buy_and_sell_pay_the_integrator_and_bordrless_on_every_kind_of_launch() {
    let mut w = World::new();
    let c = ctx(&mut w);
    let creator = w.wallet_with_sol(20 * SOL);
    for (name, rules) in [
        ("plain", LaunchRules::NONE),
        (
            "kit (holder rewards, burn, max wallet)",
            presets::every_rule(),
        ),
    ] {
        let mint = launch_with_rules(&mut w, &creator, &format!("R{}", name.len()), rules);
        // Past the sniper window and the early-buyer window (whose buys stay locked an hour).
        w.env.warp(61);
        let (buy, sell) = round_trip(&mut w, &c, &mint, 100);
        println!("{name}: routed buy {buy} CU, sell {sell} CU");
        assert!(buy < 400_000 && sell < 400_000);
    }
}

#[test]
fn a_routed_swap_on_an_ordinary_pool() {
    let mut w = World::new();
    let c = ctx(&mut w);
    let lp = w.wallet_with_sol(300 * SOL);
    let a = w.mint_to_owner(&lp, 6, 1_000_000_000_000, "AAA");
    let pool = swap::pool_address(&a, &w.sol, 30, None);
    let keys = swap::CreatePoolKeys {
        payer: lp.pubkey(),
        authority: lp.pubkey(),
        treasury: w.env.treasury.pubkey(),
        base_mint: a,
        quote_mint: w.sol,
        hook_caller: None,
    };
    let args = CreatePoolArgs {
        lp_fee_bps: 30,
        hook_program: Pubkey::default(),
        hook_flags: 0,
        virtual_base: 0,
        virtual_quote: 0,
        base_amount: 100_000_000_000,
        quote_amount: 100 * SOL,
        base_hook_accounts: 0,
        quote_hook_accounts: 0,
        hook_data: vec![],
    };
    w.env
        .send_paid_by(&[swap::create_pool(&keys, args, vec![])], &lp, &[])
        .ok();
    let t = trader(&mut w, &a, 5 * SOL);
    let me = t.pubkey();
    let dex = |direction: u8, amount_in: u64| {
        swap::swap(
            &swap::SwapKeys {
                trader: me,
                pool,
                base_mint: a,
                quote_mint: w.sol,
                trader_base: token::holding_address(&a, &me),
                trader_quote: token::holding_address(&w.sol, &me),
                hook_program: None,
                base_mint_writable: false,
                quote_mint_writable: false,
            },
            SwapArgs {
                direction,
                amount_in,
                min_amount_out: 0,
                in_hook_accounts: 0,
                out_hook_accounts: 0,
                hook_data: vec![],
            },
            vec![],
        )
    };
    let i0 = w.env.holding(&w.sol, &c.integrator.pubkey());
    let tx = w
        .env
        .send_paid_by(&[routed(&c, &me, &dex(1, SOL), 50, 0)], &t, &[]);
    tx.ok();
    let (fee, share) = fee_split(SOL, 50);
    assert_eq!(tx.event::<Swapped>().amount_in, SOL - fee);
    assert_eq!(
        w.env.holding(&w.sol, &c.integrator.pubkey()) - i0,
        fee - share
    );
    let held = w.env.holding(&a, &me);
    let tx = w
        .env
        .send_paid_by(&[routed(&c, &me, &dex(0, held), 50, 0)], &t, &[]);
    tx.ok();
    assert_eq!(
        tx.event::<IntegratorFeePaid>().fee,
        fee_split(tx.event::<Swapped>().delivered_out, 50).0
    );
}

#[test]
fn every_refusal_of_the_router() {
    let mut w = World::new();
    let c = ctx(&mut w);
    let creator = w.wallet_with_sol(20 * SOL);
    let mint = launch_with_rules(&mut w, &creator, "REF", LaunchRules::NONE);
    w.env.warp(31);
    let t = trader(&mut w, &mint, 5 * SOL);
    let me = t.pubkey();
    let buy = w.launch_swap_ix(&me, &mint, 1, SOL, 0);
    // A fee of 0 or over 1%.
    for fee in [0, 101, 10_000] {
        let tx = w
            .env
            .send_paid_by(&[routed(&c, &me, &buy, fee, 0)], &t, &[]);
        refused(&tx, RouterError::BadFee);
    }
    // The treasury's holding not the DEX config's treasury's.
    let mut ix = routed(&c, &me, &buy, 100, 0);
    ix.accounts[2].pubkey = token::holding_address(&w.sol, &c.integrator.pubkey());
    refused(
        &w.env.send_paid_by(&[ix], &t, &[]),
        RouterError::WrongTreasury,
    );
    // The integrator's holding is the trader's own, or of another mint.
    let mut ix = routed(&c, &me, &buy, 100, 0);
    ix.accounts[1].pubkey = token::holding_address(&w.sol, &me);
    refused(
        &w.env.send_paid_by(&[ix], &t, &[]),
        RouterError::WrongIntegratorHolding,
    );
    let mut ix = routed(&c, &me, &buy, 100, 0);
    ix.accounts[1].pubkey = token::holding_address(&mint, &me);
    refused(
        &w.env.send_paid_by(&[ix], &t, &[]),
        RouterError::WrongIntegratorHolding,
    );
    // A sell delivered to someone else: the fee could only come from the trader's own holding.
    w.env
        .send_paid_by(&[routed(&c, &me, &buy, 100, 0)], &t, &[])
        .ok();
    let other = trader(&mut w, &mint, SOL);
    let held = w.env.holding(&mint, &me);
    let to_other = w.launch_swap_to_ix(&me, &other.pubkey(), &mint, 0, held / 4);
    refused(
        &w.env
            .send_paid_by(&[routed(&c, &me, &to_other, 100, 0)], &t, &[]),
        RouterError::NotTradersHolding,
    );
    // The DEX swap's trader isn't the router's.
    let theirs = w.launch_swap_ix(&other.pubkey(), &mint, 1, SOL / 10, 0);
    let mut ix = routed(&c, &me, &theirs, 100, 0);
    ix.accounts
        .iter_mut()
        .filter(|a| a.pubkey == other.pubkey())
        .for_each(|a| a.is_signer = false);
    refused(
        &w.env.send_paid_by(&[ix], &t, &[]),
        RouterError::SwapAccounts,
    );
    // A sell under the app's floor after the fee: refused; at it: lands.
    let held = w.env.holding(&mint, &me);
    let dex = w.launch_swap_ix(&me, &mint, 0, held / 4, 0);
    let q = w.launch_quote(&mint, &me, false, held / 4);
    let delivered = q.delivered.unwrap();
    let keep = delivered - fee_split(delivered, 100).0;
    refused(
        &w.env
            .send_paid_by(&[routed(&c, &me, &dex, 100, keep + 1)], &t, &[]),
        RouterError::Slippage,
    );
    w.env
        .send_paid_by(&[routed(&c, &me, &dex, 100, keep)], &t, &[])
        .ok();
    // The DEX program, the token program swapped for others.
    let mut ix = routed(&c, &me, &buy, 100, 0);
    ix.accounts[3] = AccountMeta::new_readonly(bordrless_launch::ID, false);
    refused(
        &w.env.send_paid_by(&[ix], &t, &[]),
        RouterError::WrongProgram,
    );
}

#[test]
fn a_routed_trade_on_a_coin_whose_hook_takes_a_sol_cut() {
    use bordrless_hook::token_flags;
    use bordrless_launch::client as launch;
    use bordrless_launch::instructions::CreateConfigArgs;
    use bordrless_launch::state::QuoteCut;
    use hook_tester::{client as tester, quote_mode};
    let mut w = World::new();
    let c = ctx(&mut w);
    let creator = w.wallet_with_sol(20 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_paid_by(
            &[
                tester::init_script(creator.pubkey(), mint, vec![]),
                tester::init_quote_state(creator.pubkey(), mint),
                tester::set_quote_answer(
                    creator.pubkey(),
                    mint,
                    quote_mode::RETURN,
                    300u16.to_le_bytes().to_vec(),
                ),
            ],
            &creator,
            &[],
        )
        .ok();
    let config = Keypair::new();
    let args = CreateConfigArgs {
        rules: LaunchRules::NONE,
        creator_fee_bps: 100,
        custom_hook: Some(hook_tester::ID),
        custom_hook_flags: token_flags::BEFORE_TRANSFER,
        label: "Routed cut".to_string(),
    };
    let cut = QuoteCut {
        max_buy_bps: 500,
        max_sell_bps: 500,
    };
    let ix = launch::create_quote_cut_config(creator.pubkey(), config.pubkey(), args, 0, cut);
    w.env.send_paid_by(&[ix], &creator, &[&config]).ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "RCUT", VQ, &config.pubkey());
    w.env.send_paid_by(&[ix], &creator, &[&mint_kp]).ok();
    w.env.warp(31);
    let t = trader(&mut w, &mint, 5 * SOL);
    let me = t.pubkey();
    let dex = w.launch_swap_ix(&me, &mint, 1, SOL, 0);
    let tx = w
        .env
        .send_paid_by(&[routed(&c, &me, &dex, 100, 0)], &t, &[]);
    tx.ok();
    let (fee, _) = fee_split(SOL, 100);
    let ev = tx.event::<Swapped>();
    assert_eq!(ev.amount_in, SOL - fee);
    // The hook's 3% SOL cut was taken of what the router left to swap.
    assert_eq!(
        w.launch(&mint).quote_cut_accrued(),
        (SOL - fee) * 300 / 10_000
    );
    println!(
        "routed buy on a SOL-cut coin: {} CU, height {}",
        tx.cu(),
        tx.max_height()
    );
    assert!(tx.max_height() <= 5);
    let held = w.env.holding(&mint, &me);
    let dex = w.launch_swap_ix(&me, &mint, 0, held / 2, 0);
    w.env
        .send_paid_by(&[routed(&c, &me, &dex, 100, 0)], &t, &[])
        .ok();
}
