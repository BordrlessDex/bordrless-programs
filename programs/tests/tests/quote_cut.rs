//! A custom hook's SOL cut (`docs/quote-cut.md`): a launch config whose custom hook also takes
//! SOL on every trade. The pool hook asks the hook's `quote_cut` (here `hook_tester`, scripted
//! per mint), caps its answer by the config, and takes it from the same SOL as the creator fee
//! (a buy's input, a sell's output) into the same holding, where Bordrless's share applies as it
//! does to the creator fee; the creator's own trades skip it; a trade by anyone else without the
//! four accounts, or with any of them wrong, is refused; every config and launch made before is
//! untouched.

use anchor_lang::prelude::Pubkey;
use anchor_lang::{AnchorDeserialize, Discriminator};
use bordrless_core::policy;
use bordrless_core::protocol_share;
use bordrless_hook::{token_flags, HookAccountList, Phase};
use bordrless_launch::client as launch;
use bordrless_launch::constants::*;
use bordrless_launch::error::LaunchError;
use bordrless_launch::events::{LaunchQuoteCut, QuoteCutConfigured, QuoteCutTaken};
use bordrless_launch::instructions::launch::{registry_list, registry_list_with_quote_cut};
use bordrless_launch::instructions::CreateConfigArgs;
use bordrless_launch::state::{LaunchConfig, LaunchRules, QuoteCut};
use bordrless_program_tests::env::Tx;
use bordrless_program_tests::fixture::World;
use bordrless_program_tests::launch::*;
use bordrless_swap::events::{DeltaPaid, Swapped};
use hook_tester::client as tester;
use hook_tester::{quote_mode, QuoteState, TesterError};
use solana_keypair::Keypair;
use solana_signer::Signer;

const SHARE: u16 = policy::LAUNCH_PROTOCOL_SHARE_BPS;
const FEE_BPS: u16 = 100;

#[track_caller]
fn refused(tx: &Tx, e: LaunchError) {
    tx.expect_code(u32::from(e));
    assert!(
        tx.logs()
            .iter()
            .any(|l| l.contains(&format!("Error Code: {e:?}"))),
        "expected {e:?}\n{}",
        tx.logs().join("\n")
    );
}

fn share(cuts: u64) -> u64 {
    protocol_share(cuts, SHARE).unwrap()
}

/// `amount * bps / 10_000`, rounded down.
fn fee_of(amount: u64, bps: u16) -> u64 {
    u64::try_from(u128::from(amount) * u128::from(bps) / 10_000).unwrap()
}

fn b64(s: &str) -> Vec<u8> {
    let table = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    };
    let bytes: Vec<u8> = s.bytes().filter(|c| *c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= table(*c) << (18 - 6 * i);
        }
        let take = chunk.len() - 1;
        for i in 0..take {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}

/// The events of type `E` logged with `emit!` (`Program data:` lines).
fn logged<E: Discriminator + AnchorDeserialize>(tx: &Tx) -> Vec<E> {
    tx.logs()
        .iter()
        .filter_map(|l| l.strip_prefix("Program data: "))
        .map(b64)
        .filter(|d| d.len() >= 8 && d[..8] == *E::DISCRIMINATOR)
        .map(|d| E::deserialize(&mut &d[8..]).expect("event decodes"))
        .collect()
}

fn trader(w: &mut World, mint: &Pubkey, sol: u64) -> Keypair {
    let t = w.wallet_with_sol(sol);
    w.holdings(&t, *mint, &[t.pubkey()]);
    t
}

fn args(hook: Option<Pubkey>, label: &str) -> CreateConfigArgs {
    CreateConfigArgs {
        rules: LaunchRules::NONE,
        creator_fee_bps: FEE_BPS,
        custom_hook: hook,
        custom_hook_flags: if hook.is_some() {
            token_flags::BEFORE_TRANSFER
        } else {
            0
        },
        label: label.to_string(),
    }
}

/// `create_quote_cut_config`, sent.
fn make_config(
    w: &mut World,
    creator: &Keypair,
    args: CreateConfigArgs,
    author_share_bps: u16,
    cut: QuoteCut,
) -> (Pubkey, Tx) {
    let config = Keypair::new();
    let ix = launch::create_quote_cut_config(
        creator.pubkey(),
        config.pubkey(),
        args,
        author_share_bps,
        cut,
    );
    let tx = w.env.send_paid_by(&[ix], creator, &[&config]);
    (config.pubkey(), tx)
}

struct Coin {
    creator: Keypair,
    mint: Pubkey,
    config: Pubkey,
}

/// A coin on `hook_tester` whose hook takes a SOL cut of at most `cut`, its `quote_cut` scripted
/// to answer nothing yet; past the sniper window.
fn coin(w: &mut World, cut: QuoteCut) -> Coin {
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
    let (config, tx) = make_config(
        w,
        &creator,
        args(Some(hook_tester::ID), "Whale bait"),
        0,
        cut,
    );
    tx.ok();
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "BAIT", VQ, &config);
    let tx = w.env.send_paid_by(&[ix], &creator, &[&mint_kp]);
    tx.ok();
    let ev = tx.event::<LaunchQuoteCut>();
    assert_eq!(
        (ev.mint, ev.hook, ev.max_buy_bps, ev.max_sell_bps),
        (mint, hook_tester::ID, cut.max_buy_bps, cut.max_sell_bps)
    );
    w.holdings(&creator, mint, &[creator.pubkey()]);
    w.env.warp(31);
    Coin {
        creator,
        mint,
        config,
    }
}

fn script(w: &mut World, c: &Coin, mode: u8, data: Vec<u8>) {
    w.env
        .send_paid_by(
            &[tester::set_quote_answer(
                c.creator.pubkey(),
                c.mint,
                mode,
                data,
            )],
            &c.creator,
            &[],
        )
        .ok();
}

fn rate(bps: u16) -> Vec<u8> {
    bps.to_le_bytes().to_vec()
}

fn state(w: &World, mint: &Pubkey) -> QuoteState {
    w.env.read(&tester::quote_state_address(mint))
}

/// A swap by `t` on `mint` with exactly `extras` after the pool's four.
fn swap_with(
    w: &World,
    mint: &Pubkey,
    t: &Pubkey,
    buy: bool,
    amount: u64,
    extras: Vec<anchor_lang::prelude::AccountMeta>,
) -> anchor_lang::solana_program::instruction::Instruction {
    let slice = w.launch_base_slice(mint, t, t, buy);
    let mut ix = launch::swap_with_base_slice(
        &w.launch_keys(mint),
        *t,
        *t,
        u8::from(buy),
        amount,
        0,
        slice,
    );
    ix.accounts.extend(extras);
    ix
}

/// A swap by `t` that lands; answers its `Swapped` and the transaction.
fn trade(w: &mut World, mint: &Pubkey, t: &Keypair, buy: bool, amount: u64) -> (Swapped, Tx) {
    let ix = w.launch_swap_ix(&t.pubkey(), mint, u8::from(buy), amount, 0);
    let tx = w.env.send_paid_by(&[ix], t, &[]);
    tx.ok();
    (tx.event::<Swapped>(), tx)
}

#[track_caller]
fn pool_is_sound(w: &World, mint: &Pubkey) {
    let p = w.launch_pool(mint);
    let pool = w.launch_pool_key(mint);
    assert_eq!(
        w.env.holding(&w.sol, &pool),
        p.quote_reserve + p.protocol_fees_quote,
        "the quote vault"
    );
    assert_eq!(w.env.holding(mint, &pool), p.base_reserve, "the base vault");
}

#[test]
fn the_constants_agree_with_the_programs_they_name() {
    let disc = solana_sha256_hasher::hash(b"global:quote_cut").to_bytes();
    assert_eq!(QUOTE_CUT_DISCRIMINATOR, disc[..8]);
    assert_eq!(
        QUOTE_CUT_DISCRIMINATOR,
        *hook_tester::instruction::QuoteCut::DISCRIMINATOR
    );
    assert_eq!(hook_tester::LAUNCH_ID, bordrless_launch::ID);
    assert_eq!(hook_tester::QUOTE_STATE_SEED, HOOK_STATE_SEED);
    assert_eq!(HOOK_VAULT_ID, hook_vault::ID);
    assert_eq!(HOOK_VAULT_SLOT_SEED, hook_vault::constants::SLOT_SEED);
    assert_eq!(
        usize::from(HOOK_VAULT_MAX_SLOTS),
        hook_vault::constants::MAX_SLOTS
    );
    // Both caps' bound, and the rate a quarter of which is still Bordrless's.
    assert_eq!(QUOTE_CUT_MAX_BPS, 1_000);
}

#[test]
fn the_hook_takes_sol_on_buys_and_sells_with_the_creator_fee() {
    let mut w = World::new();
    let cut = QuoteCut {
        max_buy_bps: 500,
        max_sell_bps: 800,
    };
    let c = coin(&mut w, cut);
    let l = w.launch(&c.mint);
    assert_eq!(l.quote_cut(), Some((hook_tester::ID, cut)));
    assert_eq!(l.quote_cut_accrued(), 0);
    script(&mut w, &c, quote_mode::RETURN, rate(300));

    // A buy: 3% of the SOL in, with the 1% creator fee, one delta; Bordrless a quarter of both.
    let t = trader(&mut w, &c.mint, 10 * SOL);
    let q = w.launch_quote(&c.mint, &t.pubkey(), true, SOL);
    let holding_before = w.env.holding(&w.sol, &launch::launch_address(&c.mint));
    let (ev, tx) = trade(&mut w, &c.mint, &t, true, SOL);
    let taken = fee_of(SOL, 300);
    assert_eq!(taken, 30_000_000);
    assert_eq!(
        ev.deltas_in,
        vec![DeltaPaid {
            holding: l.quote_holding,
            amount: q.creator_fee + taken
        }]
    );
    assert_eq!(ev.cuts_in, q.creator_fee + taken);
    // The LP fee is on what reached the pool, after the deltas (as with the creator fee alone).
    assert_eq!(ev.received_in, SOL - q.creator_fee - taken);
    assert_eq!(
        ev.protocol_fee,
        bordrless_core::fee_amount(ev.received_in, ev.lp_fee_bps).unwrap()
            + share(q.creator_fee + taken)
    );
    let taken_logs = logged::<QuoteCutTaken>(&tx);
    assert_eq!(taken_logs.len(), 1);
    assert_eq!(
        (
            taken_logs[0].actor,
            taken_logs[0].direction,
            taken_logs[0].answered_bps,
            taken_logs[0].bps,
            taken_logs[0].amount
        ),
        (t.pubkey(), 1, 300, 300, taken)
    );
    // The hook was told the buy before it ran, by the launchpad's signer for it.
    let s = state(&w, &c.mint);
    let told = s.told_pool().unwrap();
    assert_eq!(
        (
            told.phase,
            told.direction,
            told.amount_in,
            told.actor,
            told.pool
        ),
        (
            Phase::Before,
            1,
            SOL,
            t.pubkey(),
            w.launch_pool_key(&c.mint)
        )
    );
    assert!(s.signer_ok);
    assert_eq!((s.calls, s.balance, s.actor), (1, 0, t.pubkey()));
    let l = w.launch(&c.mint);
    assert_eq!(l.quote_cut_accrued(), taken);
    assert_eq!(l.creator_fees_accrued, q.creator_fee + taken);
    // The holding got what the deltas say, less nothing: Bordrless's share comes from the pool.
    assert_eq!(
        w.env.holding(&w.sol, &launch::launch_address(&c.mint)) - holding_before,
        q.creator_fee + taken
    );
    pool_is_sound(&w, &c.mint);

    // A sell: 3% of the SOL out (what the hook was told), with the creator fee.
    let held = w.env.holding(&c.mint, &t.pubkey());
    let q = w.launch_quote(&c.mint, &t.pubkey(), false, held / 2);
    let (ev, tx) = trade(&mut w, &c.mint, &t, false, held / 2);
    let s = state(&w, &c.mint);
    let told = s.told_pool().unwrap();
    assert_eq!(
        (told.phase, told.direction, told.amount_in),
        (Phase::After, 0, held / 2)
    );
    assert_eq!(
        s.balance,
        held - held / 2,
        "the hook reads the seller's balance after the sale left"
    );
    let taken_out = fee_of(told.amount_out, 300);
    assert_eq!(
        ev.deltas_out,
        vec![DeltaPaid {
            holding: l.quote_holding,
            amount: q.creator_fee + taken_out
        }]
    );
    assert_eq!(ev.protocol_fee, q.lp_fee + share(q.creator_fee + taken_out));
    assert_eq!(
        ev.delivered_out,
        q.delivered.unwrap()
            - taken_out
            - (share(q.creator_fee + taken_out) - share(q.creator_fee)),
        "the seller pays the cut and Bordrless's share of it"
    );
    assert_eq!(logged::<QuoteCutTaken>(&tx)[0].amount, taken_out);
    assert_eq!(w.launch(&c.mint).quote_cut_accrued(), taken + taken_out);
    pool_is_sound(&w, &c.mint);

    // The creator claims it all with the creator fees.
    let creator = c.creator.pubkey();
    let accrued = w.launch(&c.mint).creator_fees_accrued;
    let before = w.env.holding(&w.sol, &creator);
    let ix = launch::claim_creator_fees(creator, c.mint, w.sol);
    w.env.send_paid_by(&[ix], &c.creator, &[]).ok();
    assert_eq!(w.env.holding(&w.sol, &creator) - before, accrued);
}

#[test]
fn the_answer_is_capped_and_anything_but_a_rate_is_refused() {
    let mut w = World::new();
    let c = coin(
        &mut w,
        QuoteCut {
            max_buy_bps: 500,
            max_sell_bps: 0,
        },
    );
    let t = trader(&mut w, &c.mint, 20 * SOL);
    let l = w.launch(&c.mint);

    // More than the cap: the cap.
    script(&mut w, &c, quote_mode::RETURN, rate(9_000));
    let q = w.launch_quote(&c.mint, &t.pubkey(), true, SOL);
    let (ev, tx) = trade(&mut w, &c.mint, &t, true, SOL);
    assert_eq!(ev.deltas_in[0].amount, q.creator_fee + fee_of(SOL, 500));
    let e = &logged::<QuoteCutTaken>(&tx)[0];
    assert_eq!((e.answered_bps, e.bps), (9_000, 500));

    // No answer, or 0: no cut, no log; only the creator fee.
    for (mode, data) in [(quote_mode::NONE, vec![]), (quote_mode::RETURN, rate(0))] {
        script(&mut w, &c, mode, data);
        let q = w.launch_quote(&c.mint, &t.pubkey(), true, SOL);
        let (ev, tx) = trade(&mut w, &c.mint, &t, true, SOL);
        assert_eq!(
            ev.deltas_in,
            vec![DeltaPaid {
                holding: l.quote_holding,
                amount: q.creator_fee
            }]
        );
        assert!(logged::<QuoteCutTaken>(&tx).is_empty());
    }

    // Malformed: refused, nothing moves.
    for bad in [vec![1u8], vec![1, 2, 3], vec![0; 8]] {
        script(&mut w, &c, quote_mode::RETURN, bad);
        let ix = w.launch_swap_ix(&t.pubkey(), &c.mint, 1, SOL, 0);
        refused(
            &w.env.send_paid_by(&[ix], &t, &[]),
            LaunchError::QuoteCutBadAnswer,
        );
    }
    // The hook fails: the trade fails (a custom hook can refuse trades; the risk label says so).
    script(&mut w, &c, quote_mode::FAIL, vec![]);
    let ix = w.launch_swap_ix(&t.pubkey(), &c.mint, 1, SOL, 0);
    w.env
        .send_paid_by(&[ix], &t, &[])
        .expect_code(u32::from(TesterError::Refused));

    // The sell side has no cap: sells never ask the hook and need no accounts.
    script(&mut w, &c, quote_mode::FAIL, vec![]);
    let held = w.env.holding(&c.mint, &t.pubkey());
    let ix = swap_with(&w, &c.mint, &t.pubkey(), false, held / 2, vec![]);
    let tx = w.env.send_paid_by(&[ix], &t, &[]);
    tx.ok();
    assert!(logged::<QuoteCutTaken>(&tx).is_empty());
    pool_is_sound(&w, &c.mint);
}

#[test]
fn whale_bait_taxes_only_the_wallets_above_its_line() {
    let mut w = World::new();
    let c = coin(
        &mut w,
        QuoteCut {
            max_buy_bps: 1_000,
            max_sell_bps: 1_000,
        },
    );
    // 7% on any trade by a wallet holding at least 10M tokens (before a buy; after a sell).
    let line: u64 = 10_000_000 * 1_000_000;
    let mut data = line.to_le_bytes().to_vec();
    data.extend(rate(700));
    script(&mut w, &c, quote_mode::WHALE, data);

    let shrimp = trader(&mut w, &c.mint, 5 * SOL);
    let (_, tx) = trade(&mut w, &c.mint, &shrimp, true, SOL / 10);
    assert!(
        logged::<QuoteCutTaken>(&tx).is_empty(),
        "a first small buy pays nothing"
    );

    let whale = trader(&mut w, &c.mint, 100 * SOL);
    let (_, tx) = trade(&mut w, &c.mint, &whale, true, 20 * SOL);
    assert!(
        logged::<QuoteCutTaken>(&tx).is_empty(),
        "nothing held before the first buy"
    );
    assert!(w.env.holding(&c.mint, &whale.pubkey()) >= line);
    let (_, tx) = trade(&mut w, &c.mint, &whale, true, 5 * SOL);
    let e = &logged::<QuoteCutTaken>(&tx)[0];
    assert_eq!((e.bps, e.amount), (700, fee_of(5 * SOL, 700)));
    // A whale's sell that keeps it above the line pays; the shrimp's doesn't.
    let held = w.env.holding(&c.mint, &whale.pubkey());
    let (_, tx) = trade(&mut w, &c.mint, &whale, false, held / 10);
    assert_eq!(logged::<QuoteCutTaken>(&tx)[0].bps, 700);
    let small = w.env.holding(&c.mint, &shrimp.pubkey());
    let (_, tx) = trade(&mut w, &c.mint, &shrimp, false, small / 2);
    assert!(logged::<QuoteCutTaken>(&tx).is_empty());
    pool_is_sound(&w, &c.mint);
    println!(
        "whale bait: SOL taken {} lamports",
        w.launch(&c.mint).quote_cut_accrued()
    );
}

#[test]
fn a_trade_without_the_accounts_or_with_a_wrong_one_is_refused_but_the_creators() {
    let mut w = World::new();
    let c = coin(
        &mut w,
        QuoteCut {
            max_buy_bps: 500,
            max_sell_bps: 500,
        },
    );
    script(&mut w, &c, quote_mode::RETURN, rate(500));
    let t = trader(&mut w, &c.mint, 10 * SOL);
    let other = trader(&mut w, &c.mint, SOL);
    let good = launch::quote_cut_extras(&hook_tester::ID, &c.mint, &t.pubkey());

    // Without them (a client that passes the four extras only): refused.
    let ix = swap_with(&w, &c.mint, &t.pubkey(), true, SOL, vec![]);
    refused(
        &w.env.send_paid_by(&[ix], &t, &[]),
        LaunchError::QuoteCutAccountsMissing,
    );
    let ix = swap_with(&w, &c.mint, &t.pubkey(), true, SOL, good[..3].to_vec());
    refused(
        &w.env.send_paid_by(&[ix], &t, &[]),
        LaunchError::QuoteCutAccountsMissing,
    );

    // Each one wrong in turn: another wallet's holding (a whale passing a shrimp's), another
    // hook, another mint's state, the state read-only, another signer.
    let mut cases = Vec::new();
    let mut x = good.clone();
    x[1].pubkey = bordrless_token::client::holding_address(&c.mint, &other.pubkey());
    cases.push(("another wallet's holding", x));
    let mut x = good.clone();
    x[0].pubkey = tax_hook::ID;
    cases.push(("another hook", x));
    let mut x = good.clone();
    x[2].pubkey = tester::quote_state_address(&Pubkey::new_unique());
    cases.push(("another mint's state", x));
    let mut x = good.clone();
    x[2].is_writable = false;
    cases.push(("the state read-only", x));
    let mut x = good.clone();
    x[3].pubkey = launch::quote_cut_signer(&tax_hook::ID);
    cases.push(("another hook's signer", x));
    for (what, extras) in cases {
        let ix = swap_with(&w, &c.mint, &t.pubkey(), true, SOL, extras);
        let tx = w.env.send_paid_by(&[ix], &t, &[]);
        tx.expect_code(u32::from(LaunchError::QuoteCutWrongAccount));
        println!("refused: {what}");
    }
    // Right: lands.
    let ix = swap_with(&w, &c.mint, &t.pubkey(), true, SOL, good);
    w.env.send_paid_by(&[ix], &t, &[]).ok();

    // The creator trades without them and pays no cut (it would pay the creator); the hook is
    // not even asked.
    let calls = state(&w, &c.mint).calls;
    let creator = c.creator.pubkey();
    let q = w.launch_quote(&c.mint, &creator, true, SOL);
    let ix = swap_with(&w, &c.mint, &creator, true, SOL, vec![]);
    let tx = w.env.send_paid_by(&[ix], &c.creator, &[]);
    tx.ok();
    assert_eq!(tx.event::<Swapped>().deltas_in[0].amount, q.creator_fee);
    assert!(logged::<QuoteCutTaken>(&tx).is_empty());
    // With them too: still none.
    let ix = w.launch_swap_ix(
        &creator,
        &c.mint,
        0,
        w.env.holding(&c.mint, &creator) / 2,
        0,
    );
    let tx = w.env.send_paid_by(&[ix], &c.creator, &[]);
    tx.ok();
    assert!(logged::<QuoteCutTaken>(&tx).is_empty());
    assert_eq!(state(&w, &c.mint).calls, calls);
    pool_is_sound(&w, &c.mint);
}

#[test]
fn the_hook_can_trust_the_launchpads_signer_and_nobody_can_forge_it() {
    let mut w = World::new();
    let c = coin(
        &mut w,
        QuoteCut {
            max_buy_bps: 400,
            max_sell_bps: 400,
        },
    );
    script(&mut w, &c, quote_mode::SIGNED, rate(250));
    let t = trader(&mut w, &c.mint, 10 * SOL);
    let (_, tx) = trade(&mut w, &c.mint, &t, true, SOL);
    assert_eq!(logged::<QuoteCutTaken>(&tx)[0].bps, 250);

    // A direct call can't sign as the launchpad's PDA: the hook sees no signer and refuses.
    let signer = launch::quote_cut_signer(&hook_tester::ID);
    let ix = anchor_lang::solana_program::instruction::Instruction {
        program_id: hook_tester::ID,
        accounts: vec![
            anchor_lang::prelude::AccountMeta::new_readonly(signer, false),
            anchor_lang::prelude::AccountMeta::new_readonly(launch::launch_address(&c.mint), false),
            anchor_lang::prelude::AccountMeta::new_readonly(w.launch_pool_key(&c.mint), false),
            anchor_lang::prelude::AccountMeta::new_readonly(c.mint, false),
            anchor_lang::prelude::AccountMeta::new_readonly(w.sol, false),
            anchor_lang::prelude::AccountMeta::new_readonly(t.pubkey(), false),
            anchor_lang::prelude::AccountMeta::new_readonly(
                bordrless_token::client::holding_address(&c.mint, &t.pubkey()),
                false,
            ),
            anchor_lang::prelude::AccountMeta::new(tester::quote_state_address(&c.mint), false),
        ],
        data: {
            let mut d = QUOTE_CUT_DISCRIMINATOR.to_vec();
            let told = state(&w, &c.mint).told;
            d.extend(told);
            d
        },
    };
    w.env
        .send_paid_by(&[ix], &t, &[])
        .expect_code(u32::from(TesterError::BadHookSigner));
}

#[test]
fn a_config_takes_a_sol_cut_only_with_a_custom_hook_caps_within_10_percent_and_buys_asked() {
    let mut w = World::new();
    let creator = w.wallet_with_sol(10 * SOL);
    let ok = QuoteCut {
        max_buy_bps: 1_000,
        max_sell_bps: 1_000,
    };
    let bad = [
        (args(None, "No hook"), ok),
        (
            args(Some(hook_tester::ID), "Over"),
            QuoteCut {
                max_buy_bps: 1_001,
                max_sell_bps: 0,
            },
        ),
        (
            args(Some(hook_tester::ID), "Over"),
            QuoteCut {
                max_buy_bps: 0,
                max_sell_bps: 1_001,
            },
        ),
        (args(Some(hook_tester::ID), "Off"), QuoteCut::default()),
        (
            args(Some(hook_tester::ID), "Sell only"),
            QuoteCut {
                max_buy_bps: 0,
                max_sell_bps: 500,
            },
        ),
    ];
    for (a, cut) in bad {
        let (_, tx) = make_config(&mut w, &creator, a, 0, cut);
        refused(&tx, LaunchError::InvalidQuoteCut);
    }
    let (_, tx) = make_config(
        &mut w,
        &creator,
        args(Some(hook_tester::ID), "Share"),
        5_001,
        ok,
    );
    refused(&tx, LaunchError::InvalidAuthorShare);

    // A listed one: both events, the author's share fixed, the caps in the reserved bytes.
    let (config, tx) = make_config(
        &mut w,
        &creator,
        args(Some(hook_tester::ID), "Listed"),
        2_000,
        ok,
    );
    tx.ok();
    let ev = tx.event::<QuoteCutConfigured>();
    assert_eq!(
        (ev.config, ev.hook, ev.max_buy_bps, ev.max_sell_bps),
        (config, hook_tester::ID, 1_000, 1_000)
    );
    let lc: LaunchConfig = w.launch_config(&config);
    assert_eq!((lc.author_share_bps, lc.quote_cut()), (2_000, Some(ok)));
    assert_eq!(lc.reserved[0], 0);
    assert!(lc.reserved[5..].iter().all(|b| *b == 0));

    // A config made the old way has none, and nor does its launch; its pool's registry is the
    // four extras, as before.
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();
    w.env
        .send_paid_by(
            &[tester::init_script(creator.pubkey(), mint, vec![])],
            &creator,
            &[],
        )
        .ok();
    let (plain, tx) = w.create_config(&creator, args(Some(hook_tester::ID), "Plain"));
    tx.ok();
    assert_eq!(w.launch_config(&plain).quote_cut(), None);
    let ix = w.create_launch_from_config_ix(&creator.pubkey(), &mint, "PLN", VQ, &plain);
    let tx = w.env.send_paid_by(&[ix], &creator, &[&mint_kp]);
    tx.ok();
    assert!(tx.events::<LaunchQuoteCut>().is_empty());
    let l = w.launch(&mint);
    assert_eq!((l.quote_cut(), l.reserved), (None, [0; 22]));
    let pool = w.launch_pool_key(&mint);
    let reg = w.env.account(&launch::registry_address(&pool)).unwrap();
    assert_eq!(
        HookAccountList::decode(&reg.data).unwrap(),
        registry_list(l.holder_vault, l.kit_config)
    );
    w.env.warp(31);
    let t = trader(&mut w, &mint, 5 * SOL);
    let (ev, tx) = trade(&mut w, &mint, &t, true, SOL);
    assert_eq!(ev.deltas_in.len(), 1);
    assert!(logged::<QuoteCutTaken>(&tx).is_empty());
}

#[test]
fn the_pools_registry_lists_the_four_accounts_a_client_resolves() {
    let mut w = World::new();
    let c = coin(
        &mut w,
        QuoteCut {
            max_buy_bps: 100,
            max_sell_bps: 100,
        },
    );
    let l = w.launch(&c.mint);
    let pool = w.launch_pool_key(&c.mint);
    let list = HookAccountList::decode(
        &w.env
            .account(&launch::registry_address(&pool))
            .unwrap()
            .data,
    )
    .unwrap();
    assert_eq!(
        list,
        registry_list_with_quote_cut(l.holder_vault, l.kit_config, hook_tester::ID)
    );
    let actor = Pubkey::new_unique();
    let prefix = [launch::hook_signer(), pool, c.mint, w.sol, actor];
    let metas = list
        .resolve(&prefix, &Pubkey::default(), &Pubkey::default())
        .unwrap();
    let mut expected = launch::hook_extras(&c.mint, &w.sol);
    expected.extend(launch::quote_cut_extras(&hook_tester::ID, &c.mint, &actor));
    assert_eq!(metas, expected);
    // The bumps recorded at launch are the canonical ones.
    let (state_bump, signer_bump) = l.quote_cut_bumps();
    assert_eq!(
        (state_bump, signer_bump),
        (
            Pubkey::find_program_address(&[HOOK_STATE_SEED, c.mint.as_ref()], &hook_tester::ID).1,
            Pubkey::find_program_address(
                &[QUOTE_CUT_SIGNER_SEED, hook_tester::ID.as_ref()],
                &bordrless_launch::ID
            )
            .1
        )
    );
    assert_eq!(c.config, l.config);
}

#[test]
fn the_cut_runs_through_graduation_and_after() {
    let mut w = World::new();
    let c = coin(
        &mut w,
        QuoteCut {
            max_buy_bps: 200,
            max_sell_bps: 200,
        },
    );
    // The fixture's curve filler plans buys with the reference quote, which knows no SOL cut: the
    // hook answers nothing while it fills (every buy still asks it), then 2% from graduation on.
    let (_, tx) = w.graduate_launch(&c.mint);
    tx.ok();
    assert!(state(&w, &c.mint).calls > 0);
    assert_eq!(
        w.launch(&c.mint).status,
        bordrless_launch::constants::STATUS_GRADUATED
    );
    script(&mut w, &c, quote_mode::RETURN, rate(200));
    let t = trader(&mut w, &c.mint, 10 * SOL);
    let (_, tx) = trade(&mut w, &c.mint, &t, true, SOL);
    assert_eq!(logged::<QuoteCutTaken>(&tx)[0].amount, fee_of(SOL, 200));
    let held = w.env.holding(&c.mint, &t.pubkey());
    let (_, tx) = trade(&mut w, &c.mint, &t, false, held);
    assert_eq!(logged::<QuoteCutTaken>(&tx)[0].bps, 200);
    pool_is_sound(&w, &c.mint);
}

#[test]
fn a_trade_with_the_cut_stays_within_budget() {
    let mut w = World::new();
    let c = coin(
        &mut w,
        QuoteCut {
            max_buy_bps: 500,
            max_sell_bps: 500,
        },
    );
    script(&mut w, &c, quote_mode::RETURN, rate(300));
    let t = trader(&mut w, &c.mint, 10 * SOL);
    let (_, buy) = trade(&mut w, &c.mint, &t, true, SOL);
    let held = w.env.holding(&c.mint, &t.pubkey());
    let (_, sell) = trade(&mut w, &c.mint, &t, false, held / 2);
    for (name, tx) in [("buy", &buy), ("sell", &sell)] {
        println!(
            "{name} with a SOL cut: {} CU, CPI height {}, {} bytes",
            tx.cu(),
            tx.max_height(),
            tx.size
        );
        assert!(tx.cu() < 200_000, "{name}: {} CU", tx.cu());
        assert!(tx.max_height() <= 4);
    }
}
