//! Independent adversarial audit of `bordrless_router` (`docs/router.md`). Tests named `finding_*`
//! demonstrate a finding; `sound_*` pin down an attack that the router (or the DEX, the token
//! program or the runtime under it) refuses; `measure_*` print the headroom a routed swap leaves.

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_launch::state::LaunchRules;
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::hooks::{fixed, NewPool, SwapSpec};
use bordrless_program_tests::launch::*;
use bordrless_router::{client as router, fee_split, IntegratorFeePaid, RouterError};
use bordrless_swap::client as swap;
use bordrless_swap::error::SwapError;
use bordrless_swap::events::Swapped;
use bordrless_swap::instructions::{CreatePoolArgs, SwapArgs};
use bordrless_token::client as token;
use solana_keypair::Keypair;
use solana_signer::Signer;

/// Anchor's `AccountNotInitialized`.
const ACCOUNT_NOT_INITIALIZED: u32 = 3012;

#[track_caller]
fn refused(tx: &Tx, e: RouterError) {
    tx.expect_code(u32::from(e));
}

/// An ordinary (flat-fee, hook-less) pool of `base`/`quote` seeded by `lp`.
fn ordinary_pool(
    w: &mut World,
    lp: &Keypair,
    base: Pubkey,
    quote: Pubkey,
    b: u64,
    q: u64,
) -> Pubkey {
    let pool = swap::pool_address(&base, &quote, 30, None);
    let keys = swap::CreatePoolKeys {
        payer: lp.pubkey(),
        authority: lp.pubkey(),
        treasury: w.env.treasury.pubkey(),
        base_mint: base,
        quote_mint: quote,
        hook_caller: None,
    };
    let args = CreatePoolArgs {
        lp_fee_bps: 30,
        hook_program: Pubkey::default(),
        hook_flags: 0,
        virtual_base: 0,
        virtual_quote: 0,
        base_amount: b,
        quote_amount: q,
        base_hook_accounts: 0,
        quote_hook_accounts: 0,
        hook_data: vec![],
    };
    w.env
        .send_paid_by(&[swap::create_pool(&keys, args, vec![])], lp, &[])
        .ok();
    pool
}

fn dex_swap(
    trader: Pubkey,
    pool: Pubkey,
    base: Pubkey,
    quote: Pubkey,
    direction: u8,
    amount_in: u64,
) -> Instruction {
    swap::swap(
        &swap::SwapKeys {
            trader,
            pool,
            base_mint: base,
            quote_mint: quote,
            trader_base: token::holding_address(&base, &trader),
            trader_quote: token::holding_address(&quote, &trader),
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
}

/// A SOL-quoted ordinary pool, a trader holding both sides, an integrator; no treasury holding.
struct Setup {
    w: World,
    pool: Pubkey,
    base: Pubkey,
    t: Keypair,
    integrator: Keypair,
}

fn setup() -> Setup {
    let mut w = World::new();
    let lp = w.wallet_with_sol(300 * SOL);
    let base = w.mint_to_owner(&lp, 6, 1_000_000_000_000, "AAA");
    let sol = w.sol;
    let pool = ordinary_pool(&mut w, &lp, base, sol, 100_000_000_000, 100 * SOL);
    let t = w.wallet_with_sol(5 * SOL);
    w.holdings(&t, base, &[t.pubkey()]);
    let integrator = w.wallet_with_sol(SOL);
    Setup {
        w,
        pool,
        base,
        t,
        integrator,
    }
}

fn treasury_holding(w: &mut World, mint: Pubkey) -> Pubkey {
    let treasury = w.env.treasury.pubkey();
    let payer = w.env.funded(SOL);
    w.env
        .send_paid_by(
            &[token::create_holding(payer.pubkey(), mint, treasury)],
            &payer,
            &[],
        )
        .ok();
    token::holding_address(&mint, &treasury)
}

// ---------------------------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------------------------

/// The treasury's holding of the quote is never created by the router, the DEX or the deploy
/// scripts. Until someone creates it, every routed swap whose Bordrless share is at least one unit
/// reverts (`AccountNotInitialized` from the token program), on every pool of that quote. Dust
/// fees (share 0) go through because `pay` skips a zero amount.
#[test]
fn finding_routed_swaps_revert_until_the_treasurys_quote_holding_exists() {
    let Setup {
        mut w,
        pool,
        base,
        t,
        integrator,
    } = setup();
    let me = t.pubkey();
    let sol = w.sol;
    let integrator_quote = token::holding_address(&sol, &integrator.pubkey());
    let treasury_quote = token::holding_address(&sol, &w.env.treasury.pubkey());
    assert!(w.env.account(&treasury_quote).is_none());
    let dex = dex_swap(me, pool, base, sol, 1, SOL);
    let ix = router::swap(me, integrator_quote, treasury_quote, &dex, 100, 0);
    w.env
        .send_paid_by(std::slice::from_ref(&ix), &t, &[])
        .expect_code(ACCOUNT_NOT_INITIALIZED);
    // A dust buy whose share rounds to 0 lands (fee 4, share 0).
    let dust = dex_swap(me, pool, base, sol, 1, 499);
    let tx = w.env.send_paid_by(
        &[router::swap(
            me,
            integrator_quote,
            treasury_quote,
            &dust,
            100,
            0,
        )],
        &t,
        &[],
    );
    tx.ok();
    assert_eq!(
        (
            tx.event::<IntegratorFeePaid>().fee,
            tx.event::<IntegratorFeePaid>().bordrless_share
        ),
        (4, 0)
    );
    // Anyone may create it (create_holding is permissionless); then routing works.
    treasury_holding(&mut w, sol);
    w.env.send_paid_by(&[ix], &t, &[]).ok();
}

/// The same for any other quote: a pool quoted in a hook-less token X (here, base = bridged SOL and
/// quote = X, the sides "swapped") routes only once the treasury's holding of X exists, and the fee
/// is charged in X, not SOL.
#[test]
fn finding_a_pool_quoted_in_another_token_needs_its_own_treasury_holding() {
    let mut w = World::new();
    let lp = w.wallet_with_sol(300 * SOL);
    let x = w.mint_to_owner(&lp, 6, 1_000_000_000_000, "XXX");
    let sol = w.sol;
    let pool = ordinary_pool(&mut w, &lp, sol, x, 100 * SOL, 100_000_000_000);
    let t = w.wallet_with_sol(5 * SOL);
    w.holdings(&t, x, &[t.pubkey()]);
    let integrator = w.env.funded(SOL);
    w.holdings(&integrator, x, &[integrator.pubkey()]);
    let me = t.pubkey();
    let iq = token::holding_address(&x, &integrator.pubkey());
    let tq = token::holding_address(&x, &w.env.treasury.pubkey());
    // A "sell" of bridged SOL (base) for X (quote).
    let dex = dex_swap(me, pool, sol, x, 0, SOL);
    let ix = router::swap(me, iq, tq, &dex, 100, 0);
    w.env
        .send_paid_by(std::slice::from_ref(&ix), &t, &[])
        .expect_code(ACCOUNT_NOT_INITIALIZED);
    treasury_holding(&mut w, x);
    let tx = w.env.send_paid_by(&[ix], &t, &[]);
    tx.ok();
    let delivered = tx.event::<Swapped>().delivered_out;
    let (fee, share) = fee_split(delivered, 100);
    assert_eq!(w.env.holding(&x, &integrator.pubkey()), fee - share);
    assert_eq!(w.env.holding(&x, &w.env.treasury.pubkey()), share);
}

// ---------------------------------------------------------------------------------------------
// Sound: attacks that fail
// ---------------------------------------------------------------------------------------------

/// Index 8 (the DEX's `trader_quote`, the buy's input) replaced with another wallet's quote
/// holding, even one the trader is a delegate of: refused, so the fee and the input come only from
/// the signer's own holding. (A direct DEX buy would let the delegate spend it.)
#[test]
fn sound_a_buy_cannot_spend_someone_elses_quote_holding_even_as_delegate() {
    let Setup {
        mut w,
        pool,
        base,
        t,
        integrator,
    } = setup();
    let sol = w.sol;
    let tq = treasury_holding(&mut w, sol);
    let iq = token::holding_address(&sol, &integrator.pubkey());
    let victim = w.wallet_with_sol(5 * SOL);
    let victim_quote = token::holding_address(&sol, &victim.pubkey());
    w.env
        .send_paid_by(
            &[token::approve(
                victim.pubkey(),
                victim_quote,
                t.pubkey(),
                SOL,
            )],
            &victim,
            &[],
        )
        .ok();
    let me = t.pubkey();
    let mut dex = dex_swap(me, pool, base, sol, 1, SOL);
    dex.accounts[8].pubkey = victim_quote;
    // The DEX alone accepts it (the trader is the delegate) ...
    let direct = w.env.send_paid_by(&[dex.clone()], &t, &[]);
    direct.ok();
    // ... the router does not: the fee could otherwise be pulled from the victim's holding.
    let mut dex2 = dex_swap(me, pool, base, sol, 1, SOL / 10);
    dex2.accounts[8].pubkey = victim_quote;
    refused(
        &w.env
            .send_paid_by(&[router::swap(me, iq, tq, &dex2, 100, 0)], &t, &[]),
        RouterError::NotTradersHolding,
    );
}

/// A direction other than 0 or 1 reaches the DEX as a "sell" in the router's eyes, and the DEX
/// refuses it (`InvalidDirection`) before anything moves: the whole transaction reverts.
#[test]
fn sound_direction_two_is_refused_by_the_dex() {
    let Setup {
        mut w,
        pool,
        base,
        t,
        integrator,
    } = setup();
    let sol = w.sol;
    let tq = treasury_holding(&mut w, sol);
    let iq = token::holding_address(&sol, &integrator.pubkey());
    let me = t.pubkey();
    for direction in [2u8, 255] {
        let dex = dex_swap(me, pool, base, sol, direction, SOL);
        let tx = w
            .env
            .send_paid_by(&[router::swap(me, iq, tq, &dex, 100, 0)], &t, &[]);
        tx.expect_code(u32::from(SwapError::InvalidDirection));
    }
}

/// A pool whose quote mint has a token hook (a tax token) is refused outright (`QuoteHasHook`),
/// though the DEX itself swaps it.
#[test]
fn sound_a_hooked_quote_is_refused() {
    let mut w = World::new();
    let lp = w.wallet_with_sol(300 * SOL);
    let a = w.mint_to_owner(&lp, 6, 1_000_000_000_000, "AAA");
    let collector = w.env.funded(SOL);
    let taxed = w.tax_mint(&lp, 6, 1_000_000_000_000, "TAX", collector.pubkey(), 100);
    let spec = NewPool {
        base: a,
        quote: taxed,
        lp_fee_bps: 30,
        tester_flags: None,
        extras: vec![],
        base_amount: 100_000_000_000,
        quote_amount: 100_000_000_000,
    };
    let (pool, tx) = w.new_pool(&lp, &spec);
    tx.ok();
    let t = w.env.funded(5 * SOL);
    w.holdings(&t, a, &[t.pubkey()]);
    w.holdings(&t, taxed, &[t.pubkey()]);
    w.send_tokens(&lp, taxed, &t.pubkey(), 1_000_000).ok();
    let me = t.pubkey();
    let dex = w.env.swap_ix(&SwapSpec::new(me, pool, 1, 100_000));
    w.env.send_paid_by(std::slice::from_ref(&dex), &t, &[]).ok();
    let iq = token::holding_address(&taxed, &collector.pubkey());
    let tq = treasury_holding(&mut w, taxed);
    let dex = w.env.swap_ix(&SwapSpec::new(me, pool, 1, 200_000));
    let tx = w
        .env
        .send_paid_by(&[router::swap(me, iq, tq, &dex, 100, 0)], &t, &[]);
    refused(&tx, RouterError::QuoteHasHook);
}

/// Reentrancy: a pool hook that, during the DEX's CPI, calls back into the router (with the
/// signer it was given, `hook_tester`'s FORWARD mode) is stopped by the runtime
/// (`ReentrancyNotAllowed`): the router is already on the stack. The same hook calling the router
/// on a direct (unrouted) swap reaches the router, which refuses the call on its own checks.
#[test]
fn sound_a_pool_hook_cannot_reenter_the_router() {
    use bordrless_hook::pool_flags;
    use hook_tester::{callback, client as tester, mode};
    use solana_transaction::{InstructionError, TransactionError};
    let mut w = World::new();
    let lp = w.wallet_with_sol(300 * SOL);
    let a = w.mint_to_owner(&lp, 6, 1_000_000_000_000, "AAA");
    let sol = w.sol;
    let spec = NewPool {
        base: a,
        quote: sol,
        lp_fee_bps: 30,
        tester_flags: Some(pool_flags::AFTER_SWAP),
        extras: vec![fixed(bordrless_router::ID, false)],
        base_amount: 100_000_000_000,
        quote_amount: 100 * SOL,
    };
    let (pool, tx) = w.new_pool(&lp, &spec);
    tx.ok();
    let t = w.wallet_with_sol(5 * SOL);
    w.holdings(&t, a, &[t.pubkey()]);
    let me = t.pubkey();
    let tq = treasury_holding(&mut w, sol);
    let integrator = w.wallet_with_sol(SOL);
    let iq = token::holding_address(&sol, &integrator.pubkey());
    // The hook's after_swap forwards a router `swap` (any data: the runtime refuses first).
    let data = router::swap(
        me,
        iq,
        tq,
        &w.env.swap_ix(&SwapSpec::new(me, pool, 1, 1_000)),
        100,
        0,
    )
    .data;
    w.env
        .send_paid_by(
            &[tester::set_answer(
                lp.pubkey(),
                pool,
                callback::AFTER_SWAP,
                mode::FORWARD,
                data,
            )],
            &lp,
            &[],
        )
        .ok();
    let dex = w.env.swap_ix(&SwapSpec::new(me, pool, 1, SOL));
    let tx = w
        .env
        .send_paid_by(&[router::swap(me, iq, tq, &dex, 100, 0)], &t, &[]);
    match tx.err() {
        TransactionError::InstructionError(_, InstructionError::ReentrancyNotAllowed) => {}
        other => panic!(
            "expected ReentrancyNotAllowed, got {other:?}\n{}",
            tx.logs().join("\n")
        ),
    }
    // Unrouted, the forwarded call reaches the router, which refuses it (its accounts are the
    // hook's, not a trader's).
    let tx = w.env.send_paid_by(&[dex], &t, &[]);
    tx.expect_fail();
    assert!(tx
        .logs()
        .iter()
        .any(|l| l.contains(&format!("Program {} invoke [3]", bordrless_router::ID))));
}

/// The DEX's config at index 1 is address-checked, owner-checked and discriminator-checked; a pool
/// passed at index 2 that isn't a DEX pool fails; a quote mint at 4 that isn't the pool's fails.
#[test]
fn sound_the_routers_view_of_the_dex_accounts_cannot_be_spoofed() {
    let Setup {
        mut w,
        pool,
        base,
        t,
        integrator,
    } = setup();
    let sol = w.sol;
    let tq = treasury_holding(&mut w, sol);
    let iq = token::holding_address(&sol, &integrator.pubkey());
    let me = t.pubkey();
    let dex = dex_swap(me, pool, base, sol, 1, SOL);
    let routed = router::swap(me, iq, tq, &dex, 100, 0);
    let off = routed.accounts.len() - dex.accounts.len();
    // Config: another account.
    let mut ix = routed.clone();
    ix.accounts[off + 1].pubkey = pool;
    refused(
        &w.env.send_paid_by(&[ix], &t, &[]),
        RouterError::SwapAccounts,
    );
    // Quote mint at 4 replaced with the base mint (a "swapped sides" view).
    let mut ix = routed.clone();
    ix.accounts[off + 4].pubkey = base;
    refused(
        &w.env.send_paid_by(&[ix], &t, &[]),
        RouterError::SwapAccounts,
    );
    // Pool at 2: a holding (owned by the token program).
    let mut ix = routed.clone();
    ix.accounts[off + 2].pubkey = token::holding_address(&sol, &me);
    ix.accounts[off + 2].is_writable = true;
    w.env.send_paid_by(&[ix], &t, &[]).expect_fail();
    // The DEX trader at 0 not signing (another key): refused.
    let other = Keypair::new();
    let mut ix = routed.clone();
    ix.accounts[off] = AccountMeta::new_readonly(other.pubkey(), false);
    refused(
        &w.env.send_paid_by(&[ix], &t, &[]),
        RouterError::SwapAccounts,
    );
    // Fewer than 11 swap accounts.
    let mut ix = routed;
    ix.accounts.truncate(off + 10);
    refused(
        &w.env.send_paid_by(&[ix], &t, &[]),
        RouterError::SwapAccounts,
    );
}

/// The integrator's holding set to the pool's own quote vault: allowed (it is a holding of the
/// quote), and the fee simply lands in the vault as an untracked surplus. The reserves and the
/// accrued protocol fees are unchanged: nothing is extracted from the pool and nobody but the
/// signing trader pays. (The trader's own wallet shows the fee going to the pool.)
#[test]
fn sound_an_integrator_holding_aliasing_the_pool_vault_is_refused() {
    // Hardened after this audit (I3): the pool's own holding is refused as the integrator's.
    let Setup {
        mut w,
        pool,
        base,
        t,
        ..
    } = setup();
    let sol = w.sol;
    let tq = treasury_holding(&mut w, sol);
    let vault = swap::vault_address(&pool, &sol);
    let me = t.pubkey();
    let dex = dex_swap(me, pool, base, sol, 1, SOL);
    let tx = w
        .env
        .send_paid_by(&[router::swap(me, vault, tq, &dex, 100, 0)], &t, &[]);
    tx.expect_code(u32::from(
        bordrless_router::RouterError::WrongIntegratorHolding,
    ));
}

/// The treasury itself cannot trade through the router (its fee holding is its quote holding:
/// the token program refuses a transfer to itself). Harmless, and noted.
#[test]
fn sound_the_treasury_trading_through_the_router_reverts() {
    let Setup {
        mut w,
        pool,
        base,
        integrator,
        ..
    } = setup();
    let sol = w.sol;
    let tq = treasury_holding(&mut w, sol);
    let treasury = w.env.treasury.insecure_clone();
    w.env.fund(treasury.pubkey(), 10 * SOL);
    w.wrap_sol(&treasury, 2 * SOL).ok();
    w.holdings(&treasury, base, &[treasury.pubkey()]);
    let iq = token::holding_address(&sol, &integrator.pubkey());
    let dex = dex_swap(treasury.pubkey(), pool, base, sol, 1, SOL);
    let tx = w.env.send_paid_by(
        &[router::swap(treasury.pubkey(), iq, tq, &dex, 100, 0)],
        &treasury,
        &[],
    );
    tx.expect_fail();
}

/// Fee rounding: a buy under 10,000 / fee_bps units pays no fee at all (and the share is 0 under
/// 5 units of fee). Dust only; the DEX's own minimums and the transaction fee dominate.
#[test]
fn sound_fee_rounding_is_dust_only() {
    assert_eq!(fee_split(99, 100), (0, 0));
    assert_eq!(fee_split(10_000 / 100 * 5 - 1, 100), (4, 0));
    assert_eq!(fee_split(199, 1), (0, 0));
}

// ---------------------------------------------------------------------------------------------
// Measurements: CPI height and wire size of routed swaps
// ---------------------------------------------------------------------------------------------

#[test]
fn measure_height_and_size_of_routed_launch_swaps() {
    let mut w = World::new();
    let sol = w.sol;
    let tq = treasury_holding(&mut w, sol);
    let integrator = w.wallet_with_sol(SOL);
    let iq = token::holding_address(&sol, &integrator.pubkey());
    let creator = w.wallet_with_sol(20 * SOL);
    for (name, rules) in [
        ("plain", LaunchRules::NONE),
        ("every rule", presets::every_rule()),
    ] {
        let (mint, tx) =
            w.create_launch_with(&creator, &format!("M{}", name.len()), 100, VQ, rules);
        tx.ok();
        w.env.warp(61);
        let t = w.wallet_with_sol(5 * SOL);
        w.holdings(&t, mint, &[t.pubkey()]);
        let me = t.pubkey();
        let dex = w.launch_swap_ix(&me, &mint, 1, SOL / 20, 0);
        let direct = w.env.send_paid_by(std::slice::from_ref(&dex), &t, &[]);
        let dex = w.launch_swap_ix(&me, &mint, 1, SOL / 20, 0);
        let routed = w
            .env
            .send_paid_by(&[router::swap(me, iq, tq, &dex, 100, 0)], &t, &[]);
        println!(
            "{name} buy: direct size {} height {} | routed size {} height {}",
            direct.size,
            direct.max_height(),
            routed.size,
            routed.max_height()
        );
        let held = w.env.holding(&mint, &me);
        let dex = w.launch_swap_ix(&me, &mint, 0, held / 4, 0);
        let direct = w.env.send_paid_by(&[dex], &t, &[]);
        let dex = w.launch_swap_ix(&me, &mint, 0, held / 4, 0);
        let routed = w
            .env
            .send_paid_by(&[router::swap(me, iq, tq, &dex, 100, 0)], &t, &[]);
        println!(
            "{name} sell: direct size {} height {} | routed size {} height {}",
            direct.size,
            direct.max_height(),
            routed.size,
            routed.max_height()
        );
    }
}

/// The deepest launch: a custom hook (kit rules exclude one) (on transfers) taking a SOL cut. A routed sell
/// runs the custom hook at height 5 (router, DEX, token, launchpad, hook): the runtime's ceiling,
/// so a custom hook that makes any CPI from its transfer callback works on a direct swap and fails
/// routed. The wire size with a wallet's compute-budget limit and price is printed too.
#[test]
fn measure_routed_swaps_on_a_custom_hook_coin_with_a_sol_cut() {
    use bordrless_hook::token_flags;
    use bordrless_launch::client as launch;
    use bordrless_launch::instructions::CreateConfigArgs;
    use bordrless_launch::state::QuoteCut;
    use bordrless_program_tests::env::compute_unit_price;
    use hook_tester::{client as tester, quote_mode};
    let mut w = World::new();
    let sol = w.sol;
    let tq = treasury_holding(&mut w, sol);
    let integrator = w.wallet_with_sol(SOL);
    let iq = token::holding_address(&sol, &integrator.pubkey());
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
        label: "Routed heavy".to_string(),
    };
    let cut = QuoteCut {
        max_buy_bps: 500,
        max_sell_bps: 500,
    };
    let ix = launch::create_quote_cut_config(creator.pubkey(), config.pubkey(), args, 0, cut);
    w.env.send_paid_by(&[ix], &creator, &[&config]).ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "HEVY", VQ, &config.pubkey());
    w.env.send_paid_by(&[ix], &creator, &[&mint_kp]).ok();
    w.env.warp(61);
    let t = w.wallet_with_sol(5 * SOL);
    w.holdings(&t, mint, &[t.pubkey()]);
    let me = t.pubkey();
    let price = || compute_unit_price(10_000);
    for (label, direction) in [("buy", 1u8), ("sell", 0u8)] {
        let amount = if direction == 1 {
            SOL / 20
        } else {
            w.env.holding(&mint, &me) / 4
        };
        let dex = w.launch_swap_ix(&me, &mint, direction, amount, 0);
        let direct = w.env.send_paid_by(&[price(), dex.clone()], &t, &[]);
        direct.ok();
        let amount = if direction == 1 {
            SOL / 20
        } else {
            w.env.holding(&mint, &me) / 4
        };
        let dex = w.launch_swap_ix(&me, &mint, direction, amount, 0);
        let routed =
            w.env
                .send_paid_by(&[price(), router::swap(me, iq, tq, &dex, 100, 0)], &t, &[]);
        routed.ok();
        println!(
            "heavy {label}: direct size {} height {} CU {} | routed size {} height {} CU {}",
            direct.size,
            direct.max_height(),
            direct.cu(),
            routed.size,
            routed.max_height(),
            routed.cu()
        );
    }
}
