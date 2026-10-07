//! The runtime budgets of `docs/hooks-v2.md` §6 ("Runtime" in §9): each path a client sends,
//! built as the backend builds it (a compute-unit limit and price in front), sent as a v0
//! transaction that loads the protocol's fixed addresses from a lookup table held in LiteSVM, with
//! its compute, its inner instructions, its instruction trace, its CPI height and its size
//! measured, printed and held under a ceiling. Then a swap routed through `hook_tester` with every
//! rule on.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::instruction::Instruction;
use bordrless_bridge::client as bridge;
use bordrless_kit::client as kit;
use bordrless_kit::events::RewardsClaimed;
use bordrless_launch::constants::STATUS_GRADUATED;
use bordrless_program_tests::env::{compute_unit_limit, compute_unit_price, Tx};
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::hooks::SwapSpec;
use bordrless_program_tests::launch::*;
use bordrless_swap::events::Swapped;
use bordrless_token::client as token;
use solana_keypair::Keypair;
use solana_message::AddressLookupTableAccount;
use solana_signer::Signer;

/// A path's ceilings: compute units, instruction trace, CPI height, v0 bytes.
struct Ceiling {
    cu: u64,
    trace: usize,
    height: u8,
    bytes: usize,
}

/// The mainnet shape of every prepared transaction: a compute-unit limit and a priority price.
fn budget() -> [Instruction; 2] {
    [compute_unit_limit(1_400_000), compute_unit_price(20_000)]
}

fn with_budget(ixs: Vec<Instruction>) -> Vec<Instruction> {
    let mut all = budget().to_vec();
    all.extend(ixs);
    all
}

/// Prints a path's figures and holds them under `c` (and under the runtime's own limits).
#[track_caller]
fn measure(name: &str, tx: &Tx, c: Ceiling) {
    tx.ok();
    println!(
        "| {name} | {} keys | {} v0 bytes | {} inner + {} top-level = {} trace | height {} | {} CU |",
        tx.keys.len(),
        tx.size,
        tx.inner_len(),
        tx.trace_len() - tx.inner_len(),
        tx.trace_len(),
        tx.max_height(),
        tx.cu()
    );
    assert!(tx.size <= c.bytes.min(1_232), "{name}: {} bytes", tx.size);
    assert!(
        tx.trace_len() <= c.trace.min(64),
        "{name}: trace {}",
        tx.trace_len()
    );
    assert!(
        tx.max_height() <= c.height.min(5),
        "{name}: height {}",
        tx.max_height()
    );
    assert!(tx.cu() <= c.cu, "{name}: {} CU", tx.cu());
}

/// A wallet holding `sol` lamports of bridged SOL and a holding of `mint`.
fn holder(w: &mut World, mint: &Pubkey, sol: u64) -> Keypair {
    let t = w.wallet_with_sol(sol);
    w.holdings(&t, *mint, &[t.pubkey()]);
    t
}

/// A buy paid in SOL as the backend prepares it: the bridged-SOL holding, the wrap, the token's
/// holding, the swap.
fn buy_with_sol(w: &World, buyer: &Pubkey, mint: &Pubkey, lamports: u64) -> Vec<Instruction> {
    vec![
        token::create_holding(*buyer, w.sol, *buyer),
        bridge::wrap_sol(*buyer, lamports),
        token::create_holding(*buyer, *mint, *buyer),
        w.launch_swap_ix(buyer, mint, 1, lamports, 0),
    ]
}

/// A sell paid out in SOL: the bridged-SOL holding (idempotent), the swap, then the SOL above
/// what the wallet held before unwrapped.
fn sell_for_sol(w: &World, seller: &Pubkey, mint: &Pubkey, amount: u64) -> Vec<Instruction> {
    vec![
        token::create_holding(*seller, w.sol, *seller),
        w.launch_swap_ix(seller, mint, 0, amount, 0),
        bridge::unwrap_sol_above(*seller, w.env.holding(&w.sol, seller)),
    ]
}

#[test]
fn every_path_of_section_6_fits_its_budget() {
    let mut w = World::new();
    let table: AddressLookupTableAccount = w
        .env
        .put_lookup_table(Pubkey::new_unique(), &protocol_lookup_table(&w));
    let tables = [table];
    println!("| Path | Keys | v0 bytes | Trace | CPI height | Compute |");

    // create_launch with every module.
    let creator = w.wallet_with_sol(20 * SOL);
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    let ix = w.create_launch_ix(
        &creator.pubkey(),
        &mint,
        "EVRY",
        50,
        VQ,
        presets::every_rule(),
    );
    let tx = w
        .env
        .send_v0(&with_budget(vec![ix]), &creator, &[&mint_kp], &tables);
    measure(
        "create_launch, every module",
        &tx,
        // §6: ~32 keys, ~950 bytes, ~45 entries, height 4, ~400k CU. The PDA searches for a
        // fresh mint move the compute by about ±25k from run to run.
        Ceiling {
            cu: 400_000,
            trace: 45,
            height: 4,
            bytes: 1_100,
        },
    );
    // Past the sniper window and the early-buyer unlock; holders above the threshold, so a buy
    // pays the holder fee too.
    w.env.warp(3_601);
    let early: Vec<Keypair> = (0..3).map(|_| holder(&mut w, &mint, 2 * SOL)).collect();
    for h in &early {
        let ix = w.launch_swap_ix(&h.pubkey(), &mint, 1, SOL / 2, 0);
        w.env.send_paid_by(&[ix], h, &[]).ok();
    }
    let (eligible, min) = w.eligibility(&mint);
    assert!(eligible >= min);

    // A buy paid in SOL, every rule, by a wallet with nothing yet.
    let buyer = w.env.funded(10 * SOL);
    let ixs = with_budget(buy_with_sol(&w, &buyer.pubkey(), &mint, SOL / 2));
    let tx = w.env.send_v0(&ixs, &buyer, &[], &tables);
    measure(
        "Buy with SOL in, every rule",
        &tx,
        // §6: ~30 keys, ~850 bytes, ~32 entries, height 3, ~320k CU.
        Ceiling {
            cu: 320_000,
            trace: 32,
            height: 3,
            bytes: 850,
        },
    );
    let ev: Swapped = tx.event();
    assert_eq!(ev.deltas_in.len(), 2, "creator and holder fees");
    assert!(ev.burn_out > 0);

    // A sell paid out in SOL, every rule.
    let held = w.env.holding(&mint, &buyer.pubkey());
    let ixs = with_budget(sell_for_sol(&w, &buyer.pubkey(), &mint, held / 2));
    let tx = w.env.send_v0(&ixs, &buyer, &[], &tables);
    measure(
        "Sell with SOL out, every rule",
        &tx,
        // §6: ~30 keys, ~850 bytes, ~30 entries, height 3, ~300k CU.
        Ceiling {
            cu: 300_000,
            trace: 30,
            height: 3,
            bytes: 850,
        },
    );
    assert_eq!(tx.event::<Swapped>().deltas_out.len(), 2);

    // Trades by others earn the buyer rewards; then it sells everything, claims and unwraps.
    for h in &early {
        let ix = w.launch_swap_ix(&h.pubkey(), &mint, 1, SOL / 4, 0);
        w.env.send_paid_by(&[ix], h, &[]).ok();
    }
    let k = bordrless_program_tests::kit::KitToken::of(&w.env, mint);
    let rest = w.env.holding(&mint, &buyer.pubkey());
    let mut ixs = sell_for_sol(&w, &buyer.pubkey(), &mint, rest);
    let unwrap = ixs.pop().unwrap();
    ixs.push(kit::claim(buyer.pubkey(), mint, w.sol));
    ixs.push(unwrap);
    let tx = w.env.send_v0(&with_budget(ixs), &buyer, &[], &tables);
    measure(
        "Sell 100% with claim and unwrap",
        &tx,
        // §6: ~36 keys, ~1,000 bytes, ~40 entries, height 3, ~380k CU.
        Ceiling {
            cu: 380_000,
            trace: 40,
            height: 3,
            bytes: 1_000,
        },
    );
    assert!(tx.event::<RewardsClaimed>().amount > 0);
    assert_eq!(w.env.holding(&mint, &buyer.pubkey()), 0);
    assert_eq!(w.env.claimable(&k, &buyer.pubkey()), 0);

    // Claim one mint and unwrap.
    let claimer = &early[0];
    assert!(w.env.claimable(&k, &claimer.pubkey()) > 0);
    let ixs = with_budget(vec![
        token::create_holding(claimer.pubkey(), w.sol, claimer.pubkey()),
        kit::claim(claimer.pubkey(), mint, w.sol),
        bridge::unwrap_sol_above(claimer.pubkey(), w.env.holding(&w.sol, &claimer.pubkey())),
    ]);
    let tx = w.env.send_v0(&ixs, claimer, &[], &tables);
    measure(
        "Claim one mint and unwrap",
        &tx,
        // §6 estimated ~10 entries and ~70k CU; the measured path (budget, holding, claim,
        // unwrap) is 14 entries and about 80k, under the backend's 100k fallback per mint.
        Ceiling {
            cu: 100_000,
            trace: 16,
            height: 3,
            bytes: 700,
        },
    );

    // A wallet-to-wallet transfer, every rule: the full amount moves, nothing else.
    let (from, to) = (&early[1], &early[2]);
    let amount = w.env.holding(&mint, &from.pubkey()) / 3;
    let to_before = w.env.holding(&mint, &to.pubkey());
    let ix = w.kit_transfer_ix(mint, from.pubkey(), &from.pubkey(), &to.pubkey(), amount);
    let tx = w.env.send_v0(&with_budget(vec![ix]), from, &[], &tables);
    measure(
        "Wallet-to-wallet transfer",
        &tx,
        // §6: ~12 keys, ~500 bytes, 3 entries (5 with the budget instructions), height 2, ~45k.
        Ceiling {
            cu: 45_000,
            trace: 5,
            height: 2,
            bytes: 500,
        },
    );
    assert_eq!(w.env.holding(&mint, &to.pubkey()), to_before + amount);

    // Buy and graduate, every rule: the curve filled by wallets within max wallet, then a wallet
    // with nothing yet buys the rest in SOL and graduates the pool in the same transaction.
    let creator = w.wallet_with_sol(20 * SOL);
    let (grad_mint, tx) = w.create_launch_with(&creator, "GRAD", 50, VQ, presets::every_rule());
    tx.ok();
    w.env.warp(3_601);
    w.fill_curve(&grad_mint, SOL / 10);
    let last = w.env.funded(10 * SOL);
    let amount = w.crossing_buy_amount(&grad_mint, &last.pubkey());
    let mut ixs = buy_with_sol(&w, &last.pubkey(), &grad_mint, amount);
    ixs.push(w.graduate_ix(&last.pubkey(), &grad_mint));
    let tx = w.env.send_v0(&with_budget(ixs), &last, &[], &tables);
    measure(
        "Buy and graduate, every rule",
        &tx,
        // §6: ~36 keys, ~1,000 bytes, ~48 entries, height 4, ~520k CU.
        Ceiling {
            cu: 520_000,
            trace: 48,
            height: 4,
            bytes: 1_000,
        },
    );
    assert_eq!(w.launch(&grad_mint).status, STATUS_GRADUATED);
    assert!(w.env.kit_config(&grad_mint).graduated);

    // A swap routed through hook_tester with every rule: router > DEX > token > kit, and the
    // launch hook under the DEX.
    let trader = holder(&mut w, &mint, 2 * SOL);
    let mut spec = SwapSpec::new(trader.pubkey(), w.launch_pool_key(&mint), 1, SOL / 4);
    spec.base_mint_writable = true;
    let q = w.launch_quote(&mint, &trader.pubkey(), true, SOL / 4);
    let ix = w.env.routed_swap_ix(&spec);
    let tx = w.env.send_v0(&with_budget(vec![ix]), &trader, &[], &tables);
    measure(
        "Routed buy (hook_tester), every rule",
        &tx,
        // A router on top of a buy: one level more than a direct swap, under the limit of 5.
        Ceiling {
            cu: 320_000,
            trace: 32,
            height: 4,
            bytes: 850,
        },
    );
    assert_eq!(tx.max_height(), 4);
    let ev: Swapped = tx.event();
    expect_swapped(
        &ev,
        &q,
        &w.launch(&mint),
        true,
        w.launch_lp_fee(&mint, &trader.pubkey(), &trader.pubkey(), true),
    );
    assert_eq!(w.env.holding(&mint, &trader.pubkey()), q.delivered.unwrap());
}
